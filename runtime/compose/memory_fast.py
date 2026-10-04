"""Experimental seekable RAM snapshots for fast postcopy resume.

The RAM file is deliberately not encrypted. Restrict this mode to explicitly
selected VMs and keep the worker's state directory private.
"""
import json
import os
import subprocess
import threading
import time

from microvm import VMError


def capabilities(manager, meta, names, disabled=()):
    manager.qmp(meta, 'migrate-set-capabilities', {
        'capabilities': ([{'capability': name, 'state': False} for name in disabled] +
                         [{'capability': name, 'state': True} for name in names])})


def restore_capabilities(manager, meta):
    try:
        capabilities(manager, meta, ('mapped-ram', 'postcopy-ram'))
        return 'postcopy'
    except VMError as error:
        if str(error) != 'qmp_command_failed':
            raise
        # Postcopy can be rejected when the host/container denies userfaultfd.
        # Retry without it; any other QMP problem still fails on this attempt.
        capabilities(manager, meta, ('mapped-ram',), disabled=('postcopy-ram',))
        print('GAP_FAST_RESTORE_FALLBACK '+json.dumps({'vm_id':meta['vm_id'],'mode':'preload'}),flush=True)
        return 'preload'


def save(manager, meta):
    started = time.monotonic()
    folder = manager.folder(meta)
    pending = folder / 'memory.fast.next'
    target = folder / 'memory.fast'
    pending.unlink(missing_ok=True)
    descriptor = os.open(pending, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.close(descriptor)
    try:
        capabilities(manager, meta, ('mapped-ram', 'multifd'), disabled=('postcopy-ram',))
        manager.qmp(meta, 'migrate', {'uri': 'file:' + str(pending)})
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            status = manager.qmp(meta, 'query-migrate').get('status')
            if status == 'completed': break
            if status in ('failed', 'cancelled'): raise VMError('fast_snapshot_save_failed')
            time.sleep(.02)
        else: raise VMError('fast_snapshot_save_timeout')
        transferred = time.monotonic()
        with pending.open('rb') as snapshot: os.fsync(snapshot.fileno())
        pending.replace(target)
        directory = os.open(folder, os.O_RDONLY | os.O_DIRECTORY)
        try: os.fsync(directory)
        finally: os.close(directory)
        print('GAP_FAST_HIBERNATE_PROFILE ' + json.dumps({
            'vm_id': meta['vm_id'],
            'migration_ms': round((transferred - started) * 1000),
            'total_ms': round((time.monotonic() - started) * 1000),
            'logical_mib': round(target.stat().st_size / 1024**2, 1),
            'allocated_mib': round(target.stat().st_blocks * 512 / 1024**2, 1)}), flush=True)
    except Exception:
        try: manager.qmp(meta, 'migrate_cancel')
        except Exception: pass
        pending.unlink(missing_ok=True)
        raise


def cleanup_after_postcopy(manager, meta, process, path, inode):
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        if process.poll() is not None: break
        try:
            status = manager.qmp(meta, 'query-migrate').get('status')
            if status == 'completed': break
            if status in ('failed', 'cancelled'): return
        except (OSError, VMError):
            pass
        time.sleep(.2)
    else: return
    try:
        if path.stat().st_ino == inode:
            path.unlink()
            print('GAP_FAST_SNAPSHOT_CLEANUP ' + json.dumps({'vm_id': meta['vm_id']}), flush=True)
    except FileNotFoundError:
        pass


def restore(manager, meta):
    started = time.monotonic()
    folder = manager.folder(meta)
    path = folder / 'memory.fast'
    if not path.is_file(): raise VMError('fast_snapshot_missing')
    process = None
    guest_started = False
    try:
        with (folder / 'restore.log').open('wb') as log, manager.disk_crypto.secret(meta) as (secret, fds), manager.seed_crypto.image(meta, folder) as (seed_path, seed_fds):
            process = subprocess.Popen(manager.command(meta, seed_path) + [x.replace('--object', '-object') for x in secret] + ['-incoming', 'defer'],
                pass_fds=fds + seed_fds, stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=log,
                env={'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'})
        manager.children[meta['vm_id']] = process
        spawned = time.monotonic()
        manager.enforce_cpu(meta, process, quota_vcpus=max(4, meta['vcpus']))
        enforced = time.monotonic()
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if process.poll() is not None: raise VMError('fast_snapshot_qemu_exited')
            try:
                manager.qmp(meta, 'query-status')
                break
            except OSError:
                time.sleep(.01)
        else: raise VMError('fast_snapshot_qmp_timeout')
        ready = time.monotonic()
        restore_mode = restore_capabilities(manager, meta)
        if not manager.execution_allowed(meta): raise VMError('microvm_suspended_or_policy_unavailable')
        # Never replay a memory image after guest instructions can become visible.
        meta['state'] = 'running'
        manager.save(meta)
        manager.qmp(meta, 'migrate-incoming', {'uri': 'file:' + str(path)})
        incoming_returned = time.monotonic()
        incoming_migration_status = manager.qmp(meta, 'query-migrate').get('status')
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if process.poll() is not None: raise VMError('fast_snapshot_restore_failed')
            status = manager.qmp(meta, 'query-status').get('status')
            if status == 'running':
                guest_started = True
                break
            time.sleep(.01)
        else: raise VMError('fast_snapshot_restore_timeout')
        loaded = time.monotonic()
        resumed = time.monotonic()
        manager.enforce_cpu(meta, process, quota_only=True)
        quota_restored = time.monotonic()
        if manager.runtime: manager.runtime.execution_started(meta)
        meta.pop('snapshot_tag', None)
        meta.pop('snapshot_format', None)
        meta.pop('resume_error', None)
        manager.save(meta)
        threading.Thread(target=cleanup_after_postcopy,
                         args=(manager, meta.copy(), process, path, path.stat().st_ino),
                         daemon=True).start()
        print('GAP_FAST_RESTORE_PROFILE ' + json.dumps({
            'vm_id': meta['vm_id'],
            'spawn_ms': round((spawned - started) * 1000),
            'cpu_admission_ms': round((enforced - spawned) * 1000),
            'qmp_ready_ms': round((ready - enforced) * 1000),
            'incoming_command_ms': round((incoming_returned - ready) * 1000),
            'running_wait_ms': round((loaded - incoming_returned) * 1000),
            'guest_start_ms': round((resumed - loaded) * 1000),
            'quota_restore_ms': round((quota_restored - resumed) * 1000),
            'total_ms': round((time.monotonic() - started) * 1000),
            'restore_mode': restore_mode,
            'incoming_migration_status': incoming_migration_status,
            'migration_status': manager.qmp(meta, 'query-migrate').get('status')}), flush=True)
    except Exception as error:
        if process and process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
        manager.capture_stop(meta)
        meta['resume_error'] = str(error) if isinstance(error, VMError) else type(error).__name__
        # The catalog is marked running before migration only to fence replay.
        # If no guest instruction ran, the intact snapshot remains retryable.
        meta['state'] = 'stopped' if guest_started else 'hibernated'
        manager.save(meta)
        raise

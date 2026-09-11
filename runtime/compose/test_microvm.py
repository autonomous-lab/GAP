"""Controller safety checks without starting a hypervisor."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from microvm import MicroVMs, VMError, validate

PROJECT = 'prj_' + 'a' * 24
OWNER = 'did:gap:' + 'b' * 64
VM = 'vm_' + 'c' * 32


class MicroVMTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.images = self.root / 'images'
        self.images.mkdir()
        lines = []
        for name in ('vmlinuz', 'initramfs', 'rootfs.ext4'):
            (self.images / name).write_bytes(name.encode())
            lines.append(hashlib.sha256(name.encode()).hexdigest() + '  ' + name)
        (self.images / 'SHA256SUMS').write_text('\n'.join(lines) + '\n')
        self.manager = MicroVMs({'state_dir': str(self.root / 'state'), 'image_dir': str(self.images)}, None)
        self.meta = {'project_id': PROJECT, 'owner_did': OWNER, 'vm_id': VM, 'state': 'stopped',
                     'vcpus': 1, 'memory_mib': 1024, 'disk_gib': 4, 'ports': [], 'ssh_port': 22000}
        self.manager.save(self.meta)
        self.manager.folder(self.meta).mkdir()

    def test_manifest_requires_every_asset_once_and_valid_hash(self):
        self.manager.image_version()
        manifest = self.images / 'SHA256SUMS'
        original = manifest.read_text()
        manifest.write_text(original.splitlines()[0] + '\n' + original.splitlines()[0] + '\n' + original.splitlines()[2] + '\n')
        with self.assertRaises(VMError):
            self.manager.image_version()
        manifest.write_text(original)
        (self.images / 'vmlinuz').write_bytes(b'changed')
        with self.assertRaises(VMError):
            self.manager.image_version()

    def test_owner_and_generation_cannot_mutate_another_vm(self):
        with self.assertRaises(VMError):
            self.manager.read(PROJECT, 'did:gap:' + 'd' * 64)
        with self.assertRaisesRegex(VMError, 'vm_generation_mismatch'):
            self.manager.perform(PROJECT, OWNER, 'vm/destroy', {'vm_id': 'vm_' + 'e' * 32})
        self.assertTrue(self.manager.folder(self.meta).exists())

    def test_running_vm_cannot_be_resized_or_deleted(self):
        with patch.object(self.manager, 'alive', return_value=True):
            for action in ('update', 'destroy'):
                with self.assertRaises(VMError):
                    self.manager.perform(PROJECT, OWNER, 'vm/' + action, {'vm_id': VM})
        self.assertTrue(self.manager.folder(self.meta).exists())

    def test_destroy_retains_by_default_and_deletes_only_exact_confirmed_vm(self):
        sentinel = self.root / 'unrelated'
        sentinel.write_text('keep')
        result = self.manager.perform(PROJECT, OWNER, 'vm/destroy', {'vm_id': VM})
        self.assertEqual(result['vm']['retained_volume_id'], VM)
        self.assertTrue((self.manager.root / 'retained' / VM).is_dir())
        self.assertEqual(sentinel.read_text(), 'keep')

    def test_confirmed_deletion_and_invalid_fields(self):
        for body in ({'vm_id': VM, 'delete_data': True}, {'vm_id': VM, 'path': '/'},
                     {'vm_id': VM, 'force': 'true'}):
            with self.assertRaises(VMError):
                self.manager.perform(PROJECT, OWNER, 'vm/destroy', body)
        self.manager.perform(PROJECT, OWNER, 'vm/destroy', {'vm_id': VM, 'delete_data': True, 'confirm_data_loss': True})
        self.assertFalse(self.manager.folder(self.meta).exists())

    def test_shrink_and_host_control_input_rejected(self):
        with self.assertRaisesRegex(VMError, 'disk_shrink'):
            self.manager.perform(PROJECT, OWNER, 'vm/update', {'vm_id': VM, 'disk_gib': 3})
        for body in ({'vcpus': True}, {'ports': [22]}, {'ports': [8000, 8000]}, {'kernel': '/host/kernel'}):
            with self.assertRaises(VMError):
                validate('vm/create', body)

    def test_cumulative_quota_counts_stopped_vms_and_releases_destroyed(self):
        self.manager.quota_provider = lambda *_: {'vcpus':2,'memory_mib':4096,'max_vms':2}
        other = 'prj_' + 'd' * 24
        with patch.object(self.manager, '_perform') as execute:
            with self.assertRaisesRegex(VMError, 'agent_quota_exceeded_vcpus'):
                self.manager.perform(other, OWNER, 'vm/create', {'vcpus': 2})
            with self.assertRaisesRegex(VMError, 'agent_quota_exceeded_memory_mib'):
                self.manager.perform(other, OWNER, 'vm/create', {'memory_mib': 4096})
            execute.assert_not_called()
            self.manager.perform(other, OWNER, 'vm/create', {'vcpus': 1, 'memory_mib': 3072})
            self.manager.perform(other, 'did:gap:' + 'e'*64, 'vm/create', {'vcpus': 2})
        self.manager.perform(PROJECT, OWNER, 'vm/destroy', {'vm_id': VM})
        self.assertEqual(self.manager.quota_usage(OWNER), {'vcpus': 0, 'memory_mib': 0})

    def test_live_quota_changes_resize_and_fail_closed(self):
        limit = {'vcpus': 1, 'memory_mib': 512}
        self.manager.quota_provider = lambda project, owner: limit
        with self.assertRaisesRegex(VMError, 'agent_quota_exceeded_memory_mib'):
            self.manager.perform(PROJECT, OWNER, 'vm/start', {'vm_id': VM})
        self.manager.perform(PROJECT, OWNER, 'vm/update', {'vm_id': VM, 'memory_mib': 512})
        with self.assertRaisesRegex(VMError, 'agent_quota_exceeded_vcpus'):
            self.manager.perform(PROJECT, OWNER, 'vm/update', {'vm_id': VM, 'vcpus': 2})
        limit['vcpus'] = 2
        self.manager.perform(PROJECT, OWNER, 'vm/update', {'vm_id': VM, 'vcpus': 2})
        self.assertEqual(self.manager.quota_usage(OWNER), {'vcpus': 2, 'memory_mib': 512})
        self.manager.quota_provider = lambda project, owner: None
        with self.assertRaisesRegex(VMError, 'invalid_agent_quota'):
            self.manager.perform(PROJECT, OWNER, 'vm/start', {'vm_id': VM})

    def test_parallel_projects_cannot_overallocate_one_owner(self):
        self.manager.quota_provider = lambda *_: {'vcpus':2,'memory_mib':4096,'max_vms':3}
        import concurrent.futures
        import threading
        barrier = threading.Barrier(2)
        # One CPU is already allocated. Both contenders request the last CPU.
        def reserve(project, owner, action, body):
            self.manager.save(dict(self.meta, project_id=project))
            return True
        def attempt(project):
            barrier.wait(timeout=5)
            try:
                return self.manager.perform(project, OWNER, 'vm/create', {})
            except VMError as error:
                return str(error)
        with patch.object(self.manager, '_perform', side_effect=reserve):
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                results = list(pool.map(attempt, ['prj_'+'d'*24, 'prj_'+'e'*24]))
        self.assertCountEqual(results, [True, 'agent_quota_exceeded_vcpus'])
        self.assertEqual(self.manager.quota_usage(OWNER)['vcpus'], 2)

    def test_vm_count_limit_includes_hibernated_and_stopped(self):
        other = 'prj_' + 'd' * 24
        for state in ('stopped', 'hibernated'):
            self.manager.save(dict(self.meta, state=state))
            with self.assertRaisesRegex(VMError, 'agent_quota_exceeded_max_vms'):
                self.manager.perform(other, OWNER, 'vm/create', {})
        self.manager.quota_provider = lambda *_: {'vcpus':2,'memory_mib':4096,'max_vms':2}
        with patch.object(self.manager, '_perform') as execute:
            self.manager.perform(other, OWNER, 'vm/create', {})
            execute.assert_called_once()
        self.manager.save(dict(self.meta, state='destroyed'))
        self.assertEqual(self.manager.vm_count(OWNER), 0)

    def test_lock_prevents_concurrent_controller_mutation(self):
        with self.manager.lock(PROJECT):
            with self.assertRaisesRegex(VMError, 'vm_operation_in_progress'):
                self.manager.perform(PROJECT, OWNER, 'vm/destroy', {'vm_id': VM})


if __name__ == '__main__':
    unittest.main()

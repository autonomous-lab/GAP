"""Owner-scoped, bounded SSH PTYs. No caller-selected host, key or host command."""
import base64
import fcntl
import os
import secrets
import select
import signal
import struct
import subprocess
import termios
import threading
import time


class TerminalError(ValueError):
    pass


class Session:
    def __init__(self, command, identity, cols, rows):
        self.identity = identity
        self.id = secrets.token_hex(24)
        self.lock = threading.RLock()
        self.buffer = bytearray()
        self.start = 0
        self.seq = 0
        self.last_input = None
        self.created = self.seen = self.typed = time.monotonic()
        self.closed = False
        self.reason = None
        self.master, slave = os.openpty()
        try:
            self.resize(cols, rows)
            self.process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                start_new_session=True, env={'PATH':'/usr/bin:/bin','LANG':'C.UTF-8','TERM':'xterm-256color'})
        except Exception:
            os.close(self.master)
            raise
        finally:
            os.close(slave)
        os.set_blocking(self.master, False)
        threading.Thread(target=self.read, daemon=True).start()

    def resize(self, cols, rows):
        if type(cols) is not int or type(rows) is not int or not 20 <= cols <= 300 or not 5 <= rows <= 100:
            raise TerminalError('invalid_terminal_dimensions')
        if getattr(self, 'dimensions', None) == (cols, rows): return
        self.dimensions = cols, rows
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        # SSH propagates SIGWINCH to the remote PTY.
        if hasattr(self, 'process') and self.process.poll() is None:
            self.process.send_signal(signal.SIGWINCH)

    def read(self):
        try:
            while not self.closed:
                if not select.select([self.master], [], [], .2)[0]:
                    continue
                with self.lock:
                    if self.closed: break
                    chunk = os.read(self.master, 65536)
                    if not chunk: break
                    self.buffer.extend(chunk)
                    excess = max(0, len(self.buffer) - 1024 * 1024)
                    if excess:
                        del self.buffer[:excess]
                        self.start += excess
        except OSError:
            pass
        finally:
            self.close('shell_exited')

    def close(self, reason='closed'):
        with self.lock:
            if self.closed: return
            self.closed = True
            self.reason = reason
            os.close(self.master)
            if self.process.poll() is None:
                self.process.kill()
            self.process.wait(timeout=2)

    def exchange(self, body):
        seq, cursor = body.get('seq'), body.get('cursor')
        if type(seq) is not int or type(cursor) is not int or cursor < 0:
            raise TerminalError('invalid_terminal_sequence')
        encoded = body.get('input', '')
        if not isinstance(encoded, str) or len(encoded) > 22000:
            raise TerminalError('terminal_input_too_large')
        try: data = base64.b64decode(encoded, validate=True)
        except ValueError: raise TerminalError('invalid_terminal_input')
        if len(data) > 16384: raise TerminalError('terminal_input_too_large')
        with self.lock:
            if seq not in (self.seq, self.seq + 1) or (seq == self.seq and data != self.last_input):
                raise TerminalError('terminal_sequence_conflict')
            if cursor > self.start + len(self.buffer): raise TerminalError('invalid_terminal_cursor')
            self.seen = time.monotonic()
            if not self.closed:
                self.resize(body.get('cols'), body.get('rows'))
                if seq == self.seq + 1:
                    # Never replay partially written input. A stalled PTY closes rather
                    # than accepting an ambiguous retry that could execute twice.
                    try:
                        offset = 0
                        deadline = time.monotonic() + .25
                        while offset < len(data):
                            if not select.select([], [self.master], [], max(0, deadline-time.monotonic()))[1]:
                                raise OSError('terminal input stalled')
                            offset += os.write(self.master, data[offset:])
                    except OSError:
                        self.close('terminal_input_stalled')
                    if data: self.typed = time.monotonic()
            self.seq, self.last_input = seq, data
            skipped = cursor < self.start
            offset = max(cursor, self.start) - self.start
            output = bytes(self.buffer[offset:offset+65536])
            return {'terminal_id': self.id, 'seq': seq, 'cursor': self.start+offset+len(output),
                    'output': base64.b64encode(output).decode(), 'truncated': skipped,
                    'closed': self.closed and offset+len(output) >= len(self.buffer), 'reason': self.reason}


class Terminals:
    def __init__(self, runner):
        self.runner = runner
        self.sessions = {}
        self.lock = threading.RLock()
        threading.Thread(target=self.watch, daemon=True).start()

    def close_vm(self, vm_id):
        with self.lock:
            for s in self.sessions.values():
                if s.identity[2] == vm_id: s.close('vm_disconnected')

    def rpc(self, project, owner, action, body):
        if isinstance(body,dict) and action=='authorize':
            with self.lock:s=self.sessions.get(body.get('terminal_id',''))
            if s is None or s.identity[:2]!=(project,owner):raise TerminalError('terminal_not_found')
            body=dict(body,vm_id=s.identity[2])
        if not isinstance(body, dict) or not isinstance(body.get('vm_id'), str):
            raise TerminalError('terminal_vm_required')
        identity = project, owner, body['vm_id']
        if action not in ('open', 'io', 'close', 'authorize', 'keepalive'): raise TerminalError('unknown_terminal_action')
        if action != 'open':
            with self.lock: s = self.sessions.get(body.get('terminal_id', ''))
            if s is None or s.identity != identity: raise TerminalError('terminal_not_found')
            if action == 'close':
                s.close(); return {'closed':True}
            # Current policy is checked even when browser polling is continuous.
            meta = self.runner.hypervisor.read(project, owner, body['vm_id'])
            if meta['state'] != 'running' or not self.runner.runtime.check_policy(meta):
                s.close('vm_unavailable'); raise TerminalError('vm_unavailable')
            self.runner.runtime.check_credit(meta)
            if action in ('authorize','keepalive'):
                if action=='keepalive':s.seen=time.monotonic()
                return {'terminal_id':s.id,'closed':s.closed,'reason':s.reason}
            previous = s.typed
            result = s.exchange(body)
            if s.typed != previous: self.runner.runtime.touch(meta)
            return result
        cols, rows = body.get('cols'), body.get('rows')
        if type(cols) is not int or type(rows) is not int or not 20 <= cols <= 300 or not 5 <= rows <= 100:
            raise TerminalError('invalid_terminal_dimensions')
        # Resume is an explicit asynchronous VM job before opening, never a
        # blocking restore hidden behind a short HTTP terminal request.
        lock = self.runner.runtime.lock(project)
        if not lock.acquire(blocking=False): raise TerminalError('vm_busy_retry')
        try:
            meta = self.runner.hypervisor.read(project, owner, body['vm_id'])
            if meta['state'] != 'running' or not self.runner.runtime.check_policy(meta):
                raise TerminalError('vm_not_running')
            self.runner.runtime.check_credit(meta)
            guest = self.runner.hypervisor.guest(project, owner, body['vm_id'])
            key = self.runner.hypervisor.folder(meta) / 'terminal_key'
            if not key.exists(): raise TerminalError('terminal_prepare_required')
            guest['ssh_key'] = str(key)
            from runner import ssh_command
            command = ssh_command(guest)
            command[command.index('-T')] = '-tt'
            command[command.index('ServerAliveInterval=10')] = 'ServerAliveInterval=0'
            command[-1] = 'exec /bin/sh -l'
            with self.lock:
                live = [s for s in self.sessions.values() if not s.closed]
                if len(self.sessions) >= 128 or len(live) >= 32 or sum(s.identity[1] == owner for s in live) >= 2:
                    raise TerminalError('terminal_limit_reached')
                if body.get('transport')=='ttyd':
                    from ttyd_terminal import TtydSession
                    s=TtydSession(command,identity,cols,rows)
                else:s = Session(command, identity, cols, rows)
                self.sessions[s.id] = s
            self.runner.runtime.touch(meta)
            return {'terminal_id':s.id,'transport':'ttyd' if hasattr(s,'prefix') else 'ssh-pty','url':s.prefix+'/' if hasattr(s,'prefix') else None,'idle_seconds':900}
        finally:
            lock.release()

    def watch(self):
        while True:
            time.sleep(1)
            with self.lock: sessions = list(self.sessions.items())
            for key, s in sessions:
                now = time.monotonic()
                try:
                    if not s.closed:
                        if s.process.poll() is not None:s.close('shell_exited')
                        if now-s.seen > 30 or now-s.typed > 900 or now-s.created > 8*3600:
                            s.close('terminal_expired')
                        else:
                            project, owner, vm_id = s.identity
                            meta = self.runner.hypervisor.read(project, owner, vm_id)
                            if meta['state'] != 'running' or not self.runner.runtime.check_policy(meta):
                                s.close('vm_unavailable')
                            self.runner.runtime.check_credit(meta)
                    if s.closed and now-s.seen > 30:
                        with self.lock: self.sessions.pop(key, None)
                except Exception:
                    s.close('terminal_authorization_unavailable')

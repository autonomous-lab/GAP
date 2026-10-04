import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock

from memory_fast import cleanup_after_postcopy, restore_capabilities
from microvm import VMError


class FastSnapshotTests(unittest.TestCase):
    def test_userfaultfd_denial_preloads_existing_snapshot(self):
        manager = Mock()
        manager.qmp.side_effect = [VMError('qmp_command_failed'), None]
        meta = {'vm_id': 'vm_'+'a'*32}
        self.assertEqual(restore_capabilities(manager, meta), 'preload')
        self.assertEqual(manager.qmp.call_args.args[2]['capabilities'], [
            {'capability':'postcopy-ram','state':False},
            {'capability':'mapped-ram','state':True}])

    def test_other_qmp_failure_is_not_hidden(self):
        manager = Mock()
        manager.qmp.side_effect = VMError('qmp_monitor_busy')
        with self.assertRaisesRegex(VMError, 'qmp_monitor_busy'):
            restore_capabilities(manager, {'vm_id':'vm_'+'a'*32})

    def test_cleanup_removes_completed_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'memory.fast'
            path.write_bytes(b'snapshot')
            manager = Mock()
            manager.qmp.return_value = {'status': 'completed'}
            process = Mock()
            process.poll.return_value = None
            cleanup_after_postcopy(manager, {'vm_id': 'vm_'+'a'*32}, process, path, path.stat().st_ino)
            self.assertFalse(path.exists())

    def test_cleanup_does_not_remove_replaced_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'memory.fast'
            path.write_bytes(b'old')
            old_inode = path.stat().st_ino
            replacement = Path(directory) / 'replacement'
            replacement.write_bytes(b'new')
            os.replace(replacement, path)
            manager = Mock()
            manager.qmp.return_value = {'status': 'completed'}
            process = Mock()
            process.poll.return_value = None
            cleanup_after_postcopy(manager, {'vm_id': 'vm_'+'a'*32}, process, path, old_inode)
            self.assertEqual(path.read_bytes(), b'new')


if __name__ == '__main__': unittest.main()

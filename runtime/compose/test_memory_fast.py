import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock

from memory_fast import cleanup_after_postcopy


class FastSnapshotTests(unittest.TestCase):
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

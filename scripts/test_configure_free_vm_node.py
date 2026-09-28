import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class ConfigureFreeVMNodeTests(unittest.TestCase):
    def test_preserves_config_ownership_mode_and_existing_secret(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            config=root/'data/gap-compose/config/runner.json'
            image=root/'data/gap-compose/free-guest-image-v2'
            config.parent.mkdir(parents=True)
            image.mkdir(parents=True)
            for name in ('SHA256SUMS','rootfs.ext4','vmlinuz','initramfs'):
                (image/name).touch()
            config.write_text(json.dumps({'hypervisor':{'image_dir':'/images'}}))
            config.chmod(0o600)
            env=root/'.env'
            env.write_text('GAP_NODE_SEED=untouched\nGAP_FREE_VM_ABUSE_KEY='+'a'*64+'\n')
            env.chmod(0o600)
            previous=(config.stat(),env.stat())
            script=Path(__file__).with_name('configure-free-vm-node.py')
            for _ in range(2):
                subprocess.run([sys.executable,str(script),'--root',str(root),
                    '--ssh-host','node.example.com','--max-active','2'],check=True,capture_output=True)
            updated=json.loads(config.read_text())
            self.assertEqual(updated['hypervisor']['image_dir'],'/images')
            self.assertEqual(updated['hypervisor']['free_image_dir'],'/free-images-v2')
            self.assertEqual(updated['free_vm'],{'enabled':True,'ssh_port':2121})
            self.assertIn('GAP_NODE_SEED=untouched\n',env.read_text())
            self.assertEqual(env.read_text().count('GAP_FREE_VM_ABUSE_KEY='),1)
            for before,after in zip(previous,(config.stat(),env.stat())):
                self.assertEqual((before.st_uid,before.st_gid,before.st_mode & 0o777),
                    (after.st_uid,after.st_gid,after.st_mode & 0o777))


if __name__=='__main__':
    unittest.main()

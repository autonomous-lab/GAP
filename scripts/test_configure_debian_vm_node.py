import json
from pathlib import Path
import tempfile
import unittest
import importlib.util


class ConfigureDebianVMTests(unittest.TestCase):
    def test_preserves_other_runner_settings_and_requires_image(self):
        script=Path(__file__).with_name('configure-debian-vm-node.py')
        spec=importlib.util.spec_from_file_location('configure_debian_vm_node',script)
        module=importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder)
            config=root/'data/gap-compose/config/runner.json'
            config.parent.mkdir(parents=True)
            config.write_text(json.dumps({'hypervisor':{'image_dir':'/images'},'token_file':'/config/service.token'}))
            with self.assertRaisesRegex(ValueError,'missing Debian image asset'):
                module.configure(root)
            image=root/'data/gap-compose/debian-guest-image-v1'
            image.mkdir()
            for asset in ('SHA256SUMS','rootfs.ext4','vmlinuz','initramfs'):
                (image/asset).touch()
            module.configure(root)
            updated=json.loads(config.read_text())
            self.assertEqual(updated['hypervisor'],{'image_dir':'/images','debian_image_dir':'/debian-images'})
            self.assertEqual(updated['token_file'],'/config/service.token')

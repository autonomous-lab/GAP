import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


class ConfigureSwapImagesTests(unittest.TestCase):
    def test_requires_both_immutable_images_and_preserves_runner_settings(self):
        script=Path(__file__).with_name('configure-swap-images-node.py')
        spec=importlib.util.spec_from_file_location('configure_swap_images_node',script)
        module=importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as folder:
            root=Path(folder)
            config=root/'data/gap-compose/config/runner.json'
            config.parent.mkdir(parents=True)
            config.write_text(json.dumps({'hypervisor':{'image_dir':'/images',
                'cpu_quota_socket':'/run/gap-cpu/quota.sock'},'token_file':'/config/service.token'}))
            with self.assertRaisesRegex(ValueError,'missing free-guest-image-v4'):
                module.configure(root)
            for name in ('free-guest-image-v4','debian-guest-image-v2'):
                image=root/'data/gap-compose'/name
                image.mkdir()
                for asset in ('SHA256SUMS','rootfs.ext4','vmlinuz','initramfs'):
                    (image/asset).touch()
            module.configure(root)
            updated=json.loads(config.read_text())
            self.assertEqual(updated['hypervisor'],{'image_dir':'/images',
                'cpu_quota_socket':'/run/gap-cpu/quota.sock',
                'free_image_v4_dir':'/free-images-v4',
                'debian_image_v2_dir':'/debian-images-v2'})
            self.assertEqual(updated['token_file'],'/config/service.token')


if __name__=='__main__':unittest.main()

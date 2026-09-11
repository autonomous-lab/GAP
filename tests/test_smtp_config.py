import importlib.util
from pathlib import Path
import tempfile
import unittest
spec=importlib.util.spec_from_file_location('smtp_config',Path(__file__).resolve().parents[1]/'scripts/configure-elestio-smtp.py')
module=importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class SmtpConfigTests(unittest.TestCase):
    def test_extracts_only_sender_preserves_other_settings_and_is_idempotent(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);script=root/'postfix.sh';env=root/'.env'
            script.write_text('docker run -e RELAYHOST_USERNAME=gap.vm.elestio.app@vm.elestio.app -e RELAYHOST_PASSWORD=secret postfix')
            env.write_text('GAP_MASTER_KEY=preserve\nGAP_EMAIL_VERIFICATION_REQUIRED=0\nGAP_SMTP_PORT=587\n')
            module.configure(script,env)
            original=env.read_text()
            self.assertIn('GAP_MASTER_KEY=preserve\n',original)
            self.assertIn('GAP_EMAIL_VERIFICATION_REQUIRED=0\n',original)
            self.assertIn('GAP_SMTP_PORT=25\n',original)
            self.assertNotIn('secret',original)
            module.configure(script,env)
            self.assertEqual(original,env.read_text())
            self.assertEqual(env.stat().st_mode & 0o777,0o600)
            env.write_text(original+'GAP_SMTP_PORT=26\n')
            before=env.read_bytes()
            with self.assertRaises(ValueError):module.configure(script,env)
            self.assertEqual(env.read_bytes(),before)

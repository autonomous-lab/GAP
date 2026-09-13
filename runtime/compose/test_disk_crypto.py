import json,os,tempfile,unittest
from pathlib import Path
from disk_crypto import DiskCrypto
from microvm import VMError
class Keys(unittest.TestCase):
 def test_binding_missing_keys_and_rotation(self):
  with tempfile.TemporaryDirectory() as d:
   path=Path(d)/'keys.json';ring={'active':'v1','keys':{'v1':'ab'*32}}
   path.write_text(json.dumps(ring));path.chmod(0o600)
   c=DiskCrypto(path);m=dict(vm_id='one',project_id='project',owner_did='owner');c.initialize(m);key=c.key(m)
   self.assertNotEqual(key,c.key(dict(m,vm_id='two')))
   with c.secret(m) as (args,fds):
    self.assertNotIn(key.decode(),' '.join(args));self.assertEqual(os.read(fds[0],128),key)
   ring['active']='v2';ring['keys']['v2']='cd'*32;path.write_text(json.dumps(ring))
   self.assertEqual(key,c.key(m))
   del ring['keys']['v1'];path.write_text(json.dumps(ring))
   with self.assertRaises(VMError):c.key(m)
   with self.assertRaises(VMError):c.key({})
   with self.assertRaises(VMError):DiskCrypto().key(m)
   path.chmod(0o644)
   with self.assertRaises(VMError):DiskCrypto(path)
if __name__=='__main__':unittest.main()

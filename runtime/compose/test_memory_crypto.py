import io,unittest
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from memory_crypto import seal,unseal,CHUNK
class Memory(unittest.TestCase):
 def test_authentication_truncation_and_boundaries(self):
  a=AESGCM(b'a'*32);plain=b'private-memory'*(CHUNK//7);encrypted=io.BytesIO()
  seal(io.BytesIO(plain),encrypted,a,len(plain));blob=encrypted.getvalue()
  self.assertNotIn(b'private-memory',blob)
  output=io.BytesIO();unseal(io.BytesIO(blob),output,a,len(plain));self.assertEqual(output.getvalue(),plain)
  tampered=bytearray(blob);tampered[-1]^=1
  for bad in (blob[:-1],blob+b'x',bytes(tampered),blob[:12]):
   with self.assertRaises(Exception):unseal(io.BytesIO(bad),io.BytesIO(),a,len(plain))
  with self.assertRaises(Exception):unseal(io.BytesIO(blob),io.BytesIO(),AESGCM(b'b'*32),len(plain))
  with self.assertRaises(Exception):unseal(io.BytesIO(blob),io.BytesIO(),a,1)
if __name__=='__main__':unittest.main()

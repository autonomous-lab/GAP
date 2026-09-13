#!/usr/bin/env python3
"""Encrypted control backup and offline verification. Never activates a restore."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import sqlite3
import struct
import time
import tempfile
import zipfile
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding, rsa
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

MAGIC = b'GAPCONTROL1\0'
LIMIT = 128 * 1024 * 1024

def exclusive(path, data):
    with open(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), 'wb') as f:
        f.write(data); f.flush(); os.fsync(f.fileno())

def config_files(root):
    root = Path(root)
    config = json.loads((root / 'control.json').read_text())
    names = {'control.json'}
    def visit(value):
        if isinstance(value, dict):
            for v in value.values(): visit(v)
        elif isinstance(value, list):
            for v in value: visit(v)
        elif isinstance(value, str) and value.startswith('/config/'):
            name = value[len('/config/'):]
            if Path(name).name != name: raise ValueError('unsupported config path')
            names.add(name)
    visit(config)
    result = {}
    for name in sorted(names):
        p = root / name
        if p.is_symlink() or not p.is_file(): raise ValueError('invalid configuration file')
        result['config/' + name] = p.read_bytes()
    return result

def check_db(db):
    if db.execute('PRAGMA integrity_check').fetchall() != [('ok',)]: raise ValueError('database integrity failed')
    if db.execute('PRAGMA foreign_key_check').fetchall(): raise ValueError('foreign keys failed')
    tables = [r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
    if not {'metadata','customers','wallet_entries','operations','reservations','capacity','customer_suspensions','vm_migrations'} <= set(tables):
        raise ValueError('control schema incomplete')
    if db.execute('SELECT count(*) FROM customers WHERE balance<0 OR spent<0').fetchone()[0]: raise ValueError('invalid wallet')
    # Every table is retained; summaries stay INSIDE the encrypted envelope.
    return {t: db.execute('SELECT count(*) FROM "' + t.replace('"','""') + '"').fetchone()[0] for t in tables}

def pack(database, config):
    files = config_files(config)
    source = sqlite3.connect(Path(database).resolve().as_uri()+'?mode=ro', uri=True)
    try:
        with sqlite3.connect(':memory:') as copy:
            source.backup(copy, pages=256, sleep=.01)
            counts = check_db(copy)
            files['authority.sqlite'] = copy.serialize()
    finally: source.close()
    if config_files(config) != {k:v for k,v in files.items() if k.startswith('config/')}:
        raise ValueError('configuration changed during backup')
    manifest = dict(version=1, created_at=int(time.time()), tables=counts,
                    sha256={k:hashlib.sha256(v).hexdigest() for k,v in files.items()})
    files['manifest.json'] = json.dumps(manifest,sort_keys=True).encode()
    if sum(map(len,files.values()))>LIMIT: raise ValueError('backup size limit')
    out = io.BytesIO()
    with zipfile.ZipFile(out,'w',zipfile.ZIP_DEFLATED) as z:
        for k,v in files.items(): z.writestr(k,v)
    return out.getvalue()

def encrypt(raw, public):
    key = serialization.load_pem_public_key(public)
    if not isinstance(key,rsa.RSAPublicKey) or key.key_size<3072: raise ValueError('RSA key too small')
    secret = AESGCM.generate_key(bit_length=256); nonce = os.urandom(12)
    wrapped = key.encrypt(secret,padding.OAEP(mgf=padding.MGF1(hashes.SHA256()),algorithm=hashes.SHA256(),label=MAGIC))
    header = MAGIC+struct.pack('>H',len(wrapped))+wrapped+nonce
    return header+AESGCM(secret).encrypt(nonce,raw,header)

def decrypt(data, private):
    if len(data)>LIMIT or not data.startswith(MAGIC): raise ValueError('invalid envelope')
    size=struct.unpack('>H',data[len(MAGIC):len(MAGIC)+2])[0]; start=len(MAGIC)+2; end=start+size
    key=serialization.load_pem_private_key(private,password=None)
    secret=key.decrypt(data[start:end],padding.OAEP(mgf=padding.MGF1(hashes.SHA256()),algorithm=hashes.SHA256(),label=MAGIC))
    return AESGCM(secret).decrypt(data[end:end+12],data[end+12:],data[:end+12])

def verify(raw, scratch="/dev/shm"):
    with zipfile.ZipFile(io.BytesIO(raw)) as z:
        entries=z.infolist()
        if len(entries)>100 or sum(e.file_size for e in entries)>LIMIT or len({e.filename for e in entries})!=len(entries):
            raise ValueError('invalid archive')
        files={e.filename:z.read(e) for e in entries}
    manifest=json.loads(files.pop('manifest.json'))
    if manifest['version']!=1 or manifest['sha256']!={k:hashlib.sha256(v).hexdigest() for k,v in files.items()}:
        raise ValueError('manifest mismatch')
    config=json.loads(files['config/control.json'])
    # SQLite serialization preserves WAL header flags. Open the standalone
    # backup on tmpfs so SQLite can handle journal mode without touching live data.
    with tempfile.TemporaryDirectory(prefix='gap-restore-',dir=scratch) as directory:
        restored=Path(directory)/'authority.sqlite'
        exclusive(restored,files['authority.sqlite'])
        with sqlite3.connect(restored) as db:
            if check_db(db)!=manifest['tables']: raise ValueError('restored table mismatch')
            if db.execute("SELECT value FROM metadata WHERE key='operator'").fetchone()!=(config.get('operator_id'),):
                raise ValueError('operator identity mismatch')
    config=json.loads(files['config/control.json'])
    if not config.get('operator_id'): raise ValueError('missing operator identity')
    return dict(verified=True, tables=len(manifest['tables']), config_files=len(files)-1)

def main():
    p=argparse.ArgumentParser(description=__doc__);sub=p.add_subparsers(dest='action',required=True)
    b=sub.add_parser('backup');b.add_argument('--database',required=True);b.add_argument('--config',required=True);b.add_argument('--public-key',required=True);b.add_argument('--output',required=True)
    v=sub.add_parser('verify');v.add_argument('--archive',required=True);v.add_argument('--private-key',required=True);v.add_argument('--scratch-dir',default='/dev/shm')
    args=p.parse_args();os.umask(0o077)
    if args.action=='backup':
        data=encrypt(pack(args.database,args.config),Path(args.public_key).read_bytes());exclusive(args.output,data)
        print(json.dumps(dict(created=True,bytes=len(data),sha256=hashlib.sha256(data).hexdigest())))
    else: print(json.dumps(verify(decrypt(Path(args.archive).read_bytes(),Path(args.private_key).read_bytes()),args.scratch_dir)))
if __name__=='__main__': main()

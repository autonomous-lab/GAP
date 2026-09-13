import base64
import io
import json
from pathlib import Path
import tarfile
import tempfile
import threading
import sqlite3
from types import SimpleNamespace
import unittest

from microvm import VMError
from vm_transfer import Transfers, unpack, FILES

MOVE='move_'+'a'*32
VM='vm_'+'b'*32
PROJECT='prj_'+'c'*24
OWNER='did:gap:'+'d'*64

class TransferTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        self.row=dict(migration_id=MOVE,source_node='one',target_node='two',phase='source_fenced',
            project_id=PROJECT,owner_did=OWNER,vm_id=VM,revision=2,disk_sha256='f'*64)
        self.ledger=SimpleNamespace(config={'node_id':'two'},transport=lambda body:self.row)
        self.runner=SimpleNamespace(hypervisor=SimpleNamespace(root=self.root),runtime=SimpleNamespace(ledger=self.ledger))
        self.transfer=Transfers(self.runner)
    def write(self,offset,data):
        return self.transfer.operation(dict(operation='write',migration_id=MOVE,offset=offset,size=6,
            archive_sha256='e'*64,data_base64=base64.b64encode(data).decode()))
    def test_replayed_chunks_are_identical_and_restart_resumes(self):
        self.assertEqual(self.write(0,b'abc')['received'],3)
        self.transfer=Transfers(self.runner)
        self.assertEqual(self.write(0,b'abc')['received'],3)
        with self.assertRaisesRegex(VMError,'migration_chunk_conflict'):self.write(0,b'xyz')
        with self.assertRaisesRegex(VMError,'migration_chunk_out_of_order'):self.write(4,b'ef')
        self.assertEqual(self.write(3,b'def')['received'],6)
        self.assertEqual((self.transfer.folder(MOVE)/'import.tar').read_bytes(),b'abcdef')
    def test_wrong_host_and_late_upload_are_refused(self):
        self.ledger.config['node_id']='one'
        with self.assertRaisesRegex(VMError,'migration_node_mismatch'):self.write(0,b'abc')
        self.ledger.config['node_id']='two';self.row['phase']='cancelling'
        with self.assertRaisesRegex(VMError,'migration_phase_conflict'):self.write(0,b'abc')
    def archive(self,name,kind=tarfile.REGTYPE):
        path=self.root/'test.tar'
        with tarfile.open(path,'w') as tar:
            member=tarfile.TarInfo(name);member.type=kind;member.size=0;member.linkname='/etc/passwd'
            tar.addfile(member,io.BytesIO())
        return path
    def test_archive_cannot_escape_or_link_or_omit_files(self):
        for name,kind in [('../outside',tarfile.REGTYPE),('client_key',tarfile.SYMTYPE),('client_key',tarfile.LNKTYPE)]:
            with self.subTest(name=name,kind=kind):
                target=self.root/'unpacked';target.mkdir(exist_ok=True)
                with self.assertRaisesRegex(VMError,'invalid_migration_archive'):unpack(self.archive(name,kind),target)
        with self.assertRaisesRegex(VMError,'migration_archive_incomplete'):unpack(self.archive('meta.json'),self.root/'incomplete')
        self.assertFalse((self.root.parent/'outside').exists())
    def test_settlement_failure_keeps_source_fenced_and_unmetered(self):
        self.row.update(phase='target_staged',revision=3)
        self.ledger.config['node_id']='one';self.ledger.last_checkpoints={}
        self.ledger.sync=lambda *args,**kwargs:None
        meta=dict(vm_id=VM,project_id=PROJECT,owner_did=OWNER)
        folder=self.root/'guest';folder.mkdir();(folder/'.migration-fence').write_text(json.dumps({'transfer_id':MOVE}))
        self.runner.hypervisor.folder=lambda meta:folder
        self.runner.hypervisor.read=lambda *args:meta
        self.runner.hypervisor.alive=lambda meta:False
        self.runner.runtime.lock=lambda project:threading.RLock()
        samples=[];self.runner.runtime.sample=lambda meta:samples.append(meta)
        with self.assertRaisesRegex(VMError,'migration_settlement_unavailable'):self.transfer.settle(MOVE)
        self.assertEqual(len(samples),1)
        self.assertTrue((folder/'.migration-meter-off').exists())
        self.assertTrue((folder/'.migration-fence').exists())
        with self.assertRaisesRegex(VMError,'migration_settlement_unavailable'):self.transfer.settle(MOVE)
        self.assertEqual(len(samples),1)

    def test_activation_is_pollable_and_late_export_is_refused(self):
        self.row['phase']='committed'
        entered=threading.Event();finish=threading.Event()
        def activate(identity):
            entered.set();finish.wait(5)
            return dict(phase='activated')
        self.transfer.activate=activate
        self.addCleanup(finish.set)
        result=self.transfer.operation(dict(operation='activate',migration_id=MOVE))
        self.assertEqual(result['phase'],'working')
        self.assertTrue(entered.wait(1))
        thread=self.transfer.tasks[MOVE]
        self.assertEqual(self.transfer.operation(dict(operation='status',migration_id=MOVE))['operation'],'activate')
        finish.set();thread.join(2)
        self.assertFalse(thread.is_alive())
        self.assertEqual(self.transfer.operation(dict(operation='status',migration_id=MOVE))['phase'],'activated')
        self.ledger.config['node_id']='one'
        (self.transfer.folder(MOVE)/'state.json').write_text(json.dumps({'phase':'exported'}))
        with self.assertRaisesRegex(VMError,'migration_phase_conflict'):
            self.transfer.operation(dict(operation='export',migration_id=MOVE))

    def test_discard_removes_data_before_ack_and_retries_lost_ack(self):
        self.row['phase']='cancelling'
        self.runner.runtime.lock=lambda _:threading.RLock()
        self.runner.hypervisor.list=lambda *_:[]
        folder=self.transfer.folder(MOVE)
        (folder/'incoming').mkdir();(folder/'incoming/disk.qcow2').write_bytes(b'disk')
        (folder/'import.tar').write_bytes(b'archive')
        calls=[]
        def transport(body):
            if body['action']=='migration-status':return self.row
            self.assertFalse((folder/'incoming').exists())
            self.assertFalse((folder/'import.tar').exists())
            calls.append(body)
            if len(calls)==1:raise VMError('lost_ack')
            self.row['phase']='target_discarded'
            return self.row
        self.ledger.transport=transport
        with self.assertRaisesRegex(VMError,'lost_ack'):
            self.transfer.discard(MOVE)
        self.assertEqual(self.transfer.discard(MOVE)['phase'],'discarded')
        self.assertEqual(calls[0],calls[1])
        self.row['phase']='committed'
        with self.assertRaisesRegex(VMError,'migration_phase_conflict'):
            self.transfer.discard(MOVE)

    def test_source_restore_requires_target_discard_and_preserves_new_fence(self):
        self.ledger.config['node_id']='one'
        meta=dict(vm_id=VM,project_id=PROJECT,owner_did=OWNER,state='stopped')
        guest=self.root/'guest';guest.mkdir()
        fence=guest/'.migration-fence';fence.write_text(json.dumps({'transfer_id':MOVE}))
        self.runner.hypervisor.read=lambda *_:meta
        self.runner.hypervisor.folder=lambda _:guest
        self.runner.hypervisor.owner_lock=lambda _:threading.RLock()
        self.runner.runtime.lock=lambda _:threading.RLock()
        self.runner.runtime.sample=lambda _:None
        self.runner.runtime.gateway=None;self.runner.ingress=None
        self.runner.authorize=lambda *_:None
        self.row['phase']='cancelling'
        with self.assertRaisesRegex(VMError,'migration_target_discard_required'):
            self.transfer.restore(MOVE)
        self.assertTrue(fence.exists())
        self.row['phase']='target_discarded'
        def transport(body):
            if body['action']=='migration-cancel-attest':
                self.assertFalse(fence.exists())
                self.row['phase']='cancelled'
            return self.row
        self.ledger.transport=transport
        self.assertEqual(self.transfer.restore(MOVE)['phase'],'restored')
        fence.write_text(json.dumps({'transfer_id':'move_'+'e'*32}))
        # A late successful cancellation retry cannot unfreeze a newer move.
        self.assertEqual(self.transfer.restore(MOVE)['phase'],'restored')
        self.assertTrue(fence.exists())

    def test_activation_resumes_after_disk_rename_before_catalog_save(self):
        from vm_transfer import sha
        self.row.update(phase='committed',home_node='two')
        m=self.runner.hypervisor;runtime=self.runner.runtime
        folder=self.transfer.folder(MOVE);incoming=folder/'incoming';incoming.mkdir()
        (incoming/'disk.qcow2').write_bytes(b'validated-disk')
        (incoming/'.migration-fence').write_text(json.dumps({'transfer_id':MOVE}))
        images=self.root/'images';images.mkdir();(images/'vmlinuz').write_bytes(b'kernel')
        original=dict(vm_id=VM,project_id=PROJECT,owner_did=OWNER,state='stopped',vcpus=1,memory_mib=1024)
        (folder/'target.json').write_text(json.dumps(dict(meta=original,kernel_sha256=sha(images/'vmlinuz'),origin_url='https://source.test',restore_running=False)))
        resources={'cpu_quarters':4,'memory_mib':1024}
        m.capacity=SimpleNamespace(adopt_migrated_vm=lambda *_:dict(committed=resources),resources=lambda _:resources,ready=set())
        m.images=images;m.image_version=lambda:'target-image';m.network=None
        m.owner_lock=lambda _:threading.RLock();m.allocation_lock=lambda:threading.RLock()
        m.catalog=lambda _:self.root/'catalog.json';m.reserved_port=lambda *_:22001
        guest=self.root/'guest';m.folder=lambda _:guest
        catalog=[];m.list=lambda *_:list(catalog);m.read=lambda *_:catalog[0]
        calls=[]
        def save(meta):
            calls.append(meta)
            if len(calls)==1:raise VMError('catalog_write_interrupted')
            catalog[:]=[dict(meta)]
        m.save=save;m.public=lambda meta:dict(meta)
        runtime.lock=lambda _:threading.RLock();runtime.sample=lambda _:None
        runtime.gateway=None;self.runner.ingress=None;self.runner.authorize=lambda *_:None
        self.ledger.adopt_migrated_project=lambda *_:None
        path=self.root/'samples.sqlite'
        with sqlite3.connect(path) as db:db.execute('CREATE TABLE samples(vm TEXT)')
        self.ledger.db=lambda:sqlite3.connect(path)
        with self.assertRaisesRegex(VMError,'catalog_write_interrupted'):self.transfer.activate(MOVE)
        self.assertFalse(incoming.exists())
        self.assertEqual((guest/'disk.qcow2').read_bytes(),b'validated-disk')
        self.assertTrue((guest/'.migration-fence').exists())
        self.assertEqual(self.transfer.activate(MOVE)['phase'],'activated')
        self.assertFalse((guest/'.migration-fence').exists())
        self.assertEqual(catalog[0]['ingress_origin'],'https://source.test')

if __name__=='__main__':unittest.main()

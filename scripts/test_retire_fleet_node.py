import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import sqlite3
import sys

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'runtime/control'))
from authority import Authority
from access import Access

spec=importlib.util.spec_from_file_location('retirement',Path(__file__).with_name('retire-fleet-node.py'))
retirement=importlib.util.module_from_spec(spec)
spec.loader.exec_module(retirement)


class RetirementTests(unittest.TestCase):
    def test_orphan_release_requires_no_projects_and_expired_lease_preserving_money(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'authority.sqlite'
            authority=Authority(path,'test',clock=lambda:1000)
            Access(authority,b'a'*32)
            customer=authority.create_customer('operator','customer','Test')['customer_id']
            owner='did:gap:'+'a'*64
            project='prj_'+'a'*24
            authority.attach_principal('operator','owner',customer,'agent',owner)
            authority.attach_project('operator','project',customer,project,'node-03',owner)
            authority.topup('operator','fund',customer,1000,'promotional')
            authority.capacity_adopt_idle('node-03','adopt',project,owner,'vm_'+'a'*32,'offline')
            first=authority.checkpoint('node-03','reserve',project,owner,'reservation-test',0,0,100,10)
            authority.checkpoint('node-03','usage',project,owner,first['reservation_id'],25,0,100,10)
            with authority.db() as db:db.execute('INSERT INTO vm_host_projects VALUES(?,?)',(project,'node-03'))
            with self.assertRaisesRegex(ValueError,'still has projects'):
                retirement.release_orphans(authority,'node-03')
            with sqlite3.connect(path) as db:db.execute('DELETE FROM projects WHERE id=?',(project,))
            with self.assertRaisesRegex(ValueError,'lease is active'):
                retirement.release_orphans(authority,'node-03')
            authority.clock=lambda:2000
            wallet=authority.wallet(customer)
            result=retirement.release_orphans(authority,'node-03')
            self.assertEqual(result['released_vm_slots'],1)
            self.assertEqual(result['removed_vm_host_bindings'],1)
            self.assertEqual(result['retained_microcredits'],100)
            self.assertEqual(authority.wallet(customer),wallet)
            self.assertEqual(wallet['balance_microcredits'],875)
            self.assertEqual(wallet['spent_microcredits'],25)
            self.assertEqual(wallet['reserved_microcredits'],100)
            self.assertEqual(retirement.release_orphans(authority,'node-03'),result)
            self.assertEqual(authority.wallet(customer),wallet)

    def test_removes_active_routes_revokes_credentials_preserves_history_and_is_idempotent(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            control=root/'data/gap-control/config/control.json'
            runner=root/'data/gap-compose/config/runner.json'
            for path in (control,runner):path.parent.mkdir(parents=True)
            control.write_text(json.dumps({
                'node_token_files':{'node-01':'/config/one','node-03':'/config/three'},
                'identity_token_files':{'node-03':'/config/identity-three'},
                'console_paths':{'node-01':'','node-03':'/nodes/node-03'},
                'finance_sources':[{'node_id':'node-01'},{'node_id':'node-03'}],
                'placement_sources':[{'node_id':'node-01'},{'node_id':'node-03'}],
                'migration_peers':{'node-03':{'url':'https://three.example'}}}))
            runner.write_text(json.dumps({'free_vm':{'node_id':'node-01',
                'peers':{'node-02':{'host':'two.example'},'node-03':{'host':'three.example'}}},
                'fleet_billing':{'projects':['prj_existing']},'migration_peers':{}}))
            env=root/'.env'
            env.write_text('GAP_PUBLIC_EXPLORER_NODES="https://one.example,https://three.example/"\nGAP_FLEET_NODE03_HOST=three.example\nGAP_FLEET_NODE02_HOST=two.example\nOTHER=unchanged\n')
            history=root/'data/history.sqlite';history.write_bytes(b'preserve historical accounting')
            before={path:path.read_bytes() for path in (control,runner,env)}
            changes=retirement.plan(root,'node-03','https://three.example')
            self.assertEqual(len(changes),3)
            self.assertEqual(before,{path:path.read_bytes() for path in before})
            backup=retirement.apply(root,'node-03',changes)
            for path,original in before.items():
                self.assertEqual((backup/path.relative_to(root)).read_bytes(),original)
            self.assertEqual(backup.stat().st_mode & 0o777,0o700)
            config=json.loads(control.read_text())
            self.assertEqual(set(config['node_token_files']),{'node-01'})
            self.assertEqual(config['finance_sources'],[{'node_id':'node-01'}])
            self.assertEqual(config['placement_sources'],[{'node_id':'node-01'}])
            self.assertEqual(config['identity_token_files'],{})
            self.assertEqual(config['migration_peers'],{})
            self.assertEqual(set(json.loads(runner.read_text())['free_vm']['peers']),{'node-02'})
            self.assertEqual(json.loads(runner.read_text())['fleet_billing']['projects'],['prj_existing'])
            self.assertEqual(env.read_text(),'GAP_PUBLIC_EXPLORER_NODES="https://one.example"\nGAP_FLEET_NODE02_HOST=two.example\nOTHER=unchanged\n')
            self.assertEqual(history.read_bytes(),b'preserve historical accounting')
            self.assertEqual(retirement.plan(root,'node-03','https://three.example'),[])

    def test_cannot_retire_the_local_worker(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            runner=root/'data/gap-compose/config/runner.json'
            runner.parent.mkdir(parents=True)
            runner.write_text(json.dumps({'free_vm':{'node_id':'node-03','peers':{}}}))
            with self.assertRaisesRegex(ValueError,'local worker'):
                retirement.plan(root,'node-03','https://three.example')


if __name__=='__main__':unittest.main()

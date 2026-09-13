"""Atomic placement reservation tests with no production state."""
import concurrent.futures
from pathlib import Path
import tempfile
import unittest

from authority import Authority, Failure
from placement import Directory
from service import Application


OWNER='did:gap:'+'a'*64
P1='prj_'+'1'*24
P2='prj_'+'2'*24


def snapshot(node, region='EU', cpu=1, memory=1024, disk=10, checked=1000):
    return {'protocol':1,'node_id':node,'checked_at':checked,'max_age_seconds':30,'region':region,
            'microvm':{'available':True,'admission_ready':True,
                'headroom':{'vcpus':cpu,'memory_mib':memory,'disk_gib':disk},
                'pricing':{'available':True,'mode':'enforced','currency':'USD',
                    'tariff':{'version':'usd-v1'}}}}


class PlacementTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.now=1000
        self.a=Authority(Path(self.temp.name)/'authority.sqlite','operator',clock=lambda:self.now)
        self.customer=self.a.create_customer('operator','customer','Test')['customer_id']
        self.a.attach_principal('operator','owner',self.customer,'agent',OWNER)
        self.a.attach_project('operator','p1',self.customer,P1,'one',OWNER)
        self.a.attach_project('operator','p2',self.customer,P2,'one',OWNER)
        self.a.set_quotas('operator','quota',self.customer,
                          dict(max_vms=2,cpu_quarters=8,memory_mib=2048),0)
        self.values={'one':snapshot('one'),'two':snapshot('two','Manassas',cpu=2,memory=2048,disk=20)}
        directory=Directory([{'node_id':'one','url':'https://one.example'},
                             {'node_id':'two','url':'https://two.example'}],
                            clock=lambda:self.now,fetch=lambda source:self.values[source['node_id']])
        self.app=Application(self.a,'admin',{'one':'one-token','two':'two-token'},
                             allow_capacity=True,placement=directory)
        self.token=self.a.issue(self.customer,OWNER)['token']

    def reserve(self,request='reserve',project=P1,region='',cpu=4,memory=1024,disk=8):
        return self.app.handle('POST','/v1/placements',self.token,
            dict(request_id=request,project_id=project,cpu_quarters=cpu,
                 memory_mib=memory,disk_gib=disk,region=region))

    def test_region_selection_idempotency_and_targeted_node_access(self):
        placed=self.reserve(region='Manassas')
        self.assertEqual((placed['node_id'],placed['state'],placed['generation']),('two','reserved',1))
        self.values['two']=None
        self.assertEqual(self.reserve(region='Manassas'),placed)
        node=self.app.handle('POST','/node','two-token',
                             {'action':'placement-get','placement_id':placed['placement_id']})
        self.assertEqual((node['project_id'],node['owner_did']),(P1,OWNER))
        with self.assertRaisesRegex(Failure,'placement_node_mismatch'):
            self.app.handle('POST','/node','one-token',
                            {'action':'placement-get','placement_id':placed['placement_id']})

    def test_reservation_claim_is_atomic_with_capacity_prepare(self):
        placed=self.reserve(region='Manassas')
        vm='vm_'+'b'*32
        prepared=self.app.handle('POST','/node','two-token',dict(action='capacity-prepare',request_id='prepare',
            project_id=P1,owner_did=OWNER,vm_id=vm,expected_revision=0,cpu_quarters=4,
            memory_mib=1024,disk_gib=8,placement_id=placed['placement_id']))
        self.assertEqual(prepared['state'],'pending')
        with self.assertRaisesRegex(Failure,'placement_already_consumed'):
            self.app.handle('POST','/node','two-token',dict(action='capacity-prepare',request_id='other',
                project_id=P1,owner_did=OWNER,vm_id='vm_'+'c'*32,expected_revision=0,cpu_quarters=4,
                memory_mib=1024,disk_gib=8,placement_id=placed['placement_id']))
        committed=self.app.handle('POST','/node','two-token',dict(action='capacity-finish',request_id='finish',
            project_id=P1,vm_id=vm,expected_revision=prepared['revision'],
            transition_id=prepared['transition_id'],outcome='commit',evidence_id='local-proof'))
        self.assertEqual(committed['state'],'active')
        self.assertEqual(self.app.handle('POST','/node','two-token',
            {'action':'placement-get','placement_id':placed['placement_id']})['state'],'committed')

    def test_expired_reservation_cannot_create(self):
        placed=self.reserve()
        self.now=placed['expires_at']+1
        with self.assertRaisesRegex(Failure,'placement_expired'):
            self.app.handle('POST','/node',placed['node_id']+'-token',dict(action='capacity-prepare',
                request_id='late',project_id=P1,owner_did=OWNER,vm_id='vm_'+'d'*32,
                expected_revision=0,cpu_quarters=4,memory_mib=1024,disk_gib=8,
                placement_id=placed['placement_id']))

    def test_concurrent_requests_cannot_overbook_public_headroom(self):
        self.values={'one':snapshot('one',cpu=1,memory=1024,disk=8),'two':None}
        def attempt(pair):
            try:return self.reserve(request=pair[0],project=pair[1])['state']
            except Failure as error:return error.code
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            results=list(pool.map(attempt,[('first',P1),('second',P2)]))
        self.assertCountEqual(results,['reserved','no_eligible_placement'])

    def test_customer_quota_counts_unclaimed_reservations(self):
        self.reserve(project=P1)
        self.a.set_quotas('operator','lower',self.customer,
                          dict(max_vms=1,cpu_quarters=4,memory_mib=1024),1)
        with self.assertRaisesRegex(Failure,'customer_quota_exceeded_max_vms'):
            self.reserve(request='second',project=P2)


if __name__=='__main__':unittest.main()

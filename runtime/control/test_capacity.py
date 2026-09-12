"""State-machine and real HTTP checks for the operator capacity authority."""
import concurrent.futures
import json
import sqlite3
from pathlib import Path
import unittest

from authority import Authority, DEFAULT_QUOTAS, Failure
import test_authority
from test_authority import OWNER, AGENT, PROJECT, SECOND
import test_service

VM = 'vm_' + 'a' * 32
OTHER = 'vm_' + 'b' * 32


class CapacityTests(unittest.TestCase):
    setUp = test_authority.AuthorityTests.setUp
    fails = test_authority.AuthorityTests.fails

    def prepare(self, vm=VM, node='node-one', project=PROJECT, request='prepare', cpu=4, memory=1024, revision=0):
        return self.a.capacity_prepare(node,request,project,OWNER,vm,cpu,memory,revision)

    def finish(self, row, outcome='commit', request='finish', node=None, **changes):
        fields = dict(node=node or row['node_id'], request=request, project=row['project_id'], vm=row['vm_id'],
                      expected_revision=row['revision'], transition=row['transition_id'], outcome=outcome,
                      evidence='durable-local-receipt')
        fields.update(changes)
        return self.a.capacity_finish(**fields)

    def limits(self, count=2, cpu=8, memory=2048, request='limits', revision=0):
        return self.a.set_quotas('operator',request,self.customer,
                                 dict(max_vms=count,cpu_quarters=cpu,memory_mib=memory),revision)

    def used(self):
        return self.a.quotas(self.customer)['allocated']

    def test_default_single_vm_is_shared_by_concurrent_nodes_and_agents(self):
        self.a.attach_principal('operator','agent',self.customer,'agent',AGENT)
        third='prj_'+'c'*24
        self.a.attach_project('operator','third',self.customer,third,'node-two',AGENT)
        self.assertEqual(self.a.quotas(self.customer)['limits'],DEFAULT_QUOTAS)
        def allocate(args):
            authority=Authority(self.path,'operator-one')
            node,project,owner,vm=args
            try:
                return authority.capacity_prepare(node,'create',project,owner,vm,1,256,0)['state']
            except Failure as e:
                return e.code
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            results=list(pool.map(allocate,[('node-one',PROJECT,OWNER,VM),('node-two',third,AGENT,OTHER)]))
        self.assertCountEqual(results,['pending','customer_quota_exceeded_max_vms'])
        self.assertEqual(self.used(),dict(max_vms=1,cpu_quarters=1,memory_mib=256))

    def test_cpu_and_ram_are_independent_customer_wide_allocations(self):
        for dimension,cpu,memory in [('cpu_quarters',5,2048),('memory_mib',8,1280)]:
            with self.subTest(dimension=dimension):
                # Separate databases keep both scenarios independently meaningful.
                self.setUp()
                self.limits(cpu=cpu,memory=memory)
                self.prepare(cpu=4,memory=1024)
                self.fails('customer_quota_exceeded_'+dimension,
                           lambda:self.prepare(OTHER,'node-two',SECOND,cpu=2,memory=512))
                self.assertEqual(self.used()['max_vms'],1)

    def test_lost_response_restart_expiry_and_replay_never_free_or_duplicate_capacity(self):
        initial=self.prepare()
        self.now+=10000000
        self.a=Authority(self.path,'operator-one',lambda:self.now)
        self.assertEqual(self.prepare(),initial)
        self.fails('customer_quota_exceeded_max_vms',lambda:self.prepare(OTHER,'node-two',SECOND))
        self.fails('request_id_conflict',lambda:self.prepare(cpu=1))
        active=self.finish(initial)
        self.assertEqual(self.finish(initial),active)
        released=self.finish(active,'release','destroyed')
        self.assertEqual(self.finish(active,'release','destroyed'),released)
        # A stale acknowledgement is history, never permission to recreate.
        self.assertEqual(self.prepare(),initial)
        current=self.a.capacity_get('node-one',PROJECT,VM)
        self.assertEqual(current['state'],'released')
        self.fails('capacity_released',lambda:self.prepare(request='recreate',revision=current['revision']))
        self.assertEqual(self.used()['max_vms'],0)
        self.assertEqual(self.prepare(OTHER,'node-two',SECOND)['state'],'pending')

    def test_resize_holds_componentwise_max_until_commit_and_rejects_stale_completion(self):
        self.limits(count=3,cpu=8,memory=2048)
        active=self.finish(self.prepare())
        resize=self.prepare(request='resize',cpu=2,memory=1536,revision=active['revision'])
        self.assertEqual(self.used(),dict(max_vms=1,cpu_quarters=4,memory_mib=1536))
        self.fails('customer_quota_exceeded_cpu_quarters',lambda:self.prepare(OTHER,'node-two',SECOND,cpu=5,memory=256))
        self.fails('capacity_transition_pending',lambda:self.prepare(request='parallel',revision=resize['revision']))
        self.fails('capacity_transition_mismatch',lambda:self.finish(resize,request='wrong',transition='old-resize'))
        self.fails('capacity_revision_conflict',lambda:self.finish(resize,request='old-revision',expected_revision=1))
        committed=self.finish(resize,request='resized')
        self.assertEqual(committed['committed'],dict(cpu_quarters=2,memory_mib=1536))
        self.assertEqual(self.used()['cpu_quarters'],2)
        self.prepare(OTHER,'node-two',SECOND,cpu=5,memory=256)
        self.assertEqual(self.used(),dict(max_vms=2,cpu_quarters=7,memory_mib=1792))

    def test_abort_requires_receipt_and_reconciles_initial_creation_or_resize(self):
        pending=self.prepare()
        self.fails('invalid_identifier',lambda:self.finish(pending,'abort',evidence=''))
        self.fails('capacity_transition_pending',lambda:self.finish(pending,'release',transition=None))
        self.assertEqual(self.used()['max_vms'],1)
        aborted=self.finish(pending,'abort','abort-create')
        self.assertEqual(aborted['state'],'released')
        self.assertEqual(self.used()['max_vms'],0)
        self.limits()
        active=self.finish(self.prepare(OTHER,request='other'),request='other-created')
        resize=self.prepare(OTHER,request='grow',cpu=8,memory=2048,revision=active['revision'])
        restored=self.finish(resize,'abort','resize-rollback')
        self.assertEqual(restored['committed'],active['committed'])
        self.assertEqual(self.used(),dict(max_vms=1,cpu_quarters=4,memory_mib=1024))

    def test_quota_reduction_preserves_existing_and_pending_capacity_and_allows_shrink(self):
        self.limits()
        pending=self.prepare()
        self.limits(count=0,cpu=1,memory=256,revision=1,request='reduce')
        self.assertEqual(set(self.a.quotas(self.customer)['over_limit']),set(DEFAULT_QUOTAS))
        active=self.finish(pending)  # previously reserved; shrinking limits does not revoke it
        self.fails('customer_quota_exceeded_cpu_quarters',lambda:self.prepare(request='grow',cpu=5,revision=active['revision']))
        smaller=self.prepare(request='shrink',cpu=1,memory=256,revision=active['revision'])
        self.assertEqual(self.used()['memory_mib'],1024)
        self.finish(smaller,request='shrunk')
        self.assertEqual(self.a.quotas(self.customer)['over_limit'],['max_vms'])
        self.fails('quota_revision_conflict',lambda:self.limits(revision=1,request='lost-admin-update'))

    def test_scoping_immutable_vm_identity_and_other_customer_isolation(self):
        initial=self.prepare()
        self.fails('project_node_mismatch',lambda:self.prepare(node='node-two'))
        self.fails('capacity_binding_mismatch',lambda:self.prepare(node='node-two',project=SECOND))
        self.fails('capacity_binding_mismatch',lambda:self.a.capacity_finish('node-two','steal',SECOND,VM,1,'prepare','abort','receipt'))
        self.fails('project_owner_mismatch',lambda:self.a.capacity_prepare('node-one','wrong-owner',PROJECT,AGENT,OTHER,1,256,0))
        customer=self.a.create_customer('operator','other-customer','Other')['customer_id']
        self.assertEqual(self.a.quotas(customer)['allocated']['max_vms'],0)
        self.assertEqual(self.a.capacity_list(customer)['allocations'],[])
        self.assertEqual(self.used()['max_vms'],1)
        self.assertEqual(self.a.capacity_list(self.customer)['allocations'][0]['vm_id'],initial['vm_id'])

    def test_invalid_values_are_rejected_before_any_reservation(self):
        for cpu in (True,0,1.5,-1,4000001):
            self.fails('invalid_capacity_value',lambda:self.prepare(cpu=cpu))
        for memory in (True,0,255,256.0,2**40+1):
            self.fails('invalid_capacity_value',lambda:self.prepare(memory=memory))
        for revision in (True,-1,1.5,2**53):
            self.fails('invalid_capacity_value',lambda:self.prepare(revision=revision))
        self.fails('invalid_vm_id',lambda:self.prepare(vm='vm_../secret'))
        self.assertEqual(self.used()['max_vms'],0)

    def test_backup_restore_preserves_pending_hold_tombstone_and_audit_receipts(self):
        first=self.finish(self.prepare())
        self.finish(first,'release','deleted')
        self.prepare(OTHER,request='pending-after-delete')
        restored=Path(self.temp.name)/'restored.sqlite'
        with sqlite3.connect(self.a.path) as source,sqlite3.connect(restored) as target:
            source.backup(target)
        backup=Authority(restored,'operator-one')
        self.assertEqual(backup.quotas(self.customer),self.a.quotas(self.customer))
        self.assertEqual(backup.capacity_get('node-one',PROJECT,VM)['state'],'released')
        self.assertEqual(backup.capacity_get('node-one',PROJECT,OTHER)['state'],'pending')
        with backup.db() as db:
            receipt=json.loads(db.execute("SELECT result FROM operations WHERE actor='node:node-one' AND id='deleted'").fetchone()[0])
            self.assertEqual(receipt['state'],'released')
            self.assertEqual(receipt['evidence_id'],'durable-local-receipt')

    def test_inventory_pagination_and_failed_mutation_leave_no_partial_state(self):
        self.limits(count=105,cpu=105,memory=105*256)
        for index in range(102):
            self.prepare('vm_'+format(index,'032x'),request='create-'+str(index),cpu=1,memory=256)
        first=self.a.capacity_list(self.customer)
        self.assertEqual(len(first['allocations']),100)
        second=self.a.capacity_list(self.customer,first['next_cursor'])
        self.assertEqual(len(second['allocations']),2)
        self.assertIsNone(second['next_cursor'])
        with self.a.db() as db:
            before=db.execute('SELECT count(*) FROM operations').fetchone()[0]
        self.fails('customer_quota_exceeded_cpu_quarters',lambda:self.prepare(OTHER,request='too-big',cpu=4,memory=256))
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM operations').fetchone()[0],before)
        self.assertEqual(self.used()['max_vms'],102)

    def test_racing_commit_and_abort_cannot_both_acknowledge_the_same_transition(self):
        pending=self.prepare()
        def finish(outcome):
            try:
                return self.finish(pending,outcome,request=outcome)['state']
            except Failure as e:
                return e.code
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            outcomes=list(pool.map(finish,['commit','abort']))
        self.assertEqual(outcomes.count('capacity_revision_conflict'),1)
        current=self.a.capacity_get('node-one',PROJECT,VM)
        self.assertEqual(self.used()['max_vms'],int(current['state']=='active'))


class CapacityHTTPTests(unittest.TestCase):
    setUp=test_service.ServiceTests.setUp
    close=test_service.ServiceTests.close
    call=test_service.ServiceTests.call
    operator=test_service.ServiceTests.operator
    customer=test_service.ServiceTests.customer

    def test_authenticated_node_protocol_and_disabled_gate(self):
        customer=self.customer()
        body=dict(action='capacity-prepare',request_id='create',project_id=PROJECT,owner_did=OWNER,
                  vm_id=VM,cpu_quarters=1,memory_mib=256,expected_revision=0)
        status,value=self.call('/node','node-one-test',body)
        self.assertEqual((status,value['error']['code']),(409,'capacity_disabled'))
        self.app.allow_capacity=True
        status,pending=self.call('/node','node-one-test',body)
        self.assertEqual(status,200,pending)
        self.assertEqual(self.call('/node','node-one-test',body),(status,pending))
        status,value=self.call('/node','node-two-test',dict(body,project_id=SECOND,vm_id=OTHER))
        self.assertEqual((status,value['error']['code']),(409,'customer_quota_exceeded_max_vms'))
        finish=dict(action='capacity-finish',request_id='created',project_id=PROJECT,vm_id=VM,
                    expected_revision=pending['revision'],transition_id=pending['transition_id'],outcome='commit',evidence_id='local-created')
        self.assertEqual(self.call('/node','node-one-test',finish)[1]['state'],'active')
        self.assertEqual(self.call('/node','node-two-test',finish)[0],403)
        token=self.operator('issue-token',customer_id=customer)['token']
        self.assertEqual(self.call('/node',token,body)[0],403)
        for credential in ('node-one-test',token):
            self.assertEqual(self.call('/operator',credential,dict(action='set-quotas'))[0],403)
        status,summary=self.call('/v1/quotas',token)
        self.assertEqual(status,200)
        self.assertEqual(summary['allocated'],dict(max_vms=1,cpu_quarters=1,memory_mib=256))
        self.assertNotIn(VM,json.dumps(summary))
        health=self.call('/health')[1]
        self.assertTrue(health['capacity_enabled'])
        self.assertFalse(health['worker_capacity_enforcement'])

    def test_operator_revision_and_capacity_get_survive_http_response_loss(self):
        customer=self.customer()
        result=self.operator('set-quotas','limits',customer_id=customer,expected_revision=0,
                             limits=dict(max_vms=3,cpu_quarters=16,memory_mib=4096))
        self.assertEqual(result['revision'],1)
        status,value=self.call('/operator','admin-test',dict(action='set-quotas',request_id='stale',customer_id=customer,
                               expected_revision=0,limits=DEFAULT_QUOTAS))
        self.assertEqual((status,value['error']['code']),(409,'quota_revision_conflict'))
        self.app.allow_capacity=True
        body=dict(action='capacity-prepare',request_id='lost',project_id=PROJECT,owner_did=OWNER,
                  vm_id=VM,cpu_quarters=4,memory_mib=1024,expected_revision=0)
        self.call('/node','node-one-test',body)
        status,current=self.call('/node','node-one-test',dict(action='capacity-get',project_id=PROJECT,vm_id=VM))
        self.assertEqual(status,200)
        self.assertEqual(current['transition_id'],'lost')
        self.assertEqual(self.operator('capacity-list',customer_id=customer)['allocations'][0]['vm_id'],VM)
        for credential in ('node-two-test','admin-test'):
            self.assertEqual(self.call('/node',credential,dict(action='capacity-get',project_id=PROJECT,vm_id=VM))[0],403)

    def test_storage_failure_is_unavailable_not_quota_exhaustion(self):
        customer=self.customer()
        self.app.allow_capacity=True
        token=self.operator('issue-token',customer_id=customer)['token']
        path=Path(self.a.path)
        saved=path.with_suffix('.saved')
        path.rename(saved)
        path.mkdir()
        try:
            for route,credential,body in [('/v1/quotas',token,None),('/node','node-one-test',
                dict(action='capacity-prepare',request_id='create',project_id=PROJECT,owner_did=OWNER,
                     vm_id=VM,cpu_quarters=4,memory_mib=1024,expected_revision=0))]:
                status,value=self.call(route,credential,body)
                self.assertEqual((status,value['error']['code']),(503,'authority_unavailable'))
                self.assertNotIn('allocated',value)
        finally:
            path.rmdir()
            saved.rename(path)
        self.assertEqual(self.a.quotas(customer)['allocated']['max_vms'],0)


if __name__=='__main__':
    unittest.main()

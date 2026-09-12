import concurrent.futures
import unittest
from authority import Authority, Failure
import test_authority
from test_authority import OWNER, PROJECT, SECOND


class ReservationTests(unittest.TestCase):
    setUp=test_authority.AuthorityTests.setUp
    fails=test_authority.AuthorityTests.fails

    def checkpoint(self,node='node-one',request='first',project=PROJECT,reservation='reservation-one',consumed=0,unpaid=0,target=60,close=False):
        return self.a.checkpoint(node,request,project,OWNER,reservation,consumed,unpaid,target,10,close)

    def test_two_nodes_reserve_without_double_spending_and_settle_cumulatively(self):
        self.a.topup('operator','fund',self.customer,100,'promotional')
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            replies=list(pool.map(lambda pair:self.checkpoint(node=pair[0],project=pair[1],reservation=pair[0]),
                                 [('node-one',PROJECT),('node-two',SECOND)]))
        self.assertEqual(sum(r['allocated_microcredits'] for r in replies),100)
        view=self.a.wallet(self.customer)
        self.assertEqual((view['balance_microcredits'],view['reserved_microcredits']), (0,100))
        self.fails('insufficient_credits',lambda:self.a.debit('node-one','direct-spend',PROJECT,1))
        for node,project in [('node-one',PROJECT),('node-two',SECOND)]:
            self.checkpoint(node=node,request='settle',project=project,reservation=node,consumed=30,target=0)
        view=self.a.wallet(self.customer)
        self.assertEqual((view['spent_microcredits'],view['reserved_microcredits']), (60,40))

    def test_expiry_does_not_refund_unknown_consumption(self):
        self.a.topup('operator','fund',self.customer,60,'promotional')
        first=self.checkpoint()
        self.now+=1000000
        self.assertLess(first['lease_expires_at'],self.now)
        other=self.checkpoint(node='node-two',project=SECOND,reservation='other')
        self.assertEqual(other['allocated_microcredits'],0)
        self.assertEqual(self.a.wallet(self.customer)['reserved_microcredits'],60)
        self.fails('active_reservation_exists',lambda:self.checkpoint(request='parallel',reservation='duplicate'))
        final=self.checkpoint(request='close',consumed=25,target=0,close=True)
        self.assertTrue(final['closed'])
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'],35)
        self.assertEqual(self.checkpoint(request='close',consumed=25,target=0,close=True),final)
        self.fails('reservation_closed',lambda:self.checkpoint(request='reopen'))

    def test_lost_acknowledgement_restart_does_not_reserve_or_debit_twice(self):
        self.a.topup('operator','fund',self.customer,100,'promotional')
        first=self.checkpoint()
        self.a=Authority(self.path,'operator-one',lambda:self.now)
        self.assertEqual(self.checkpoint(),first)
        settled=self.checkpoint(request='settle',consumed=20)
        self.assertEqual(self.checkpoint(request='settle',consumed=20),settled)
        view=self.a.wallet(self.customer)
        self.assertEqual((view['balance_microcredits'],view['reserved_microcredits'],view['spent_microcredits']),(20,60,20))
        self.fails('invalid_consumption_checkpoint',lambda:self.checkpoint(request='backward',consumed=19))
        self.fails('invalid_consumption_checkpoint',lambda:self.checkpoint(request='excess',consumed=1000))
        self.fails('request_id_conflict',lambda:self.checkpoint(request='settle',consumed=21))

    def test_other_node_cannot_settle_release_or_rebind_a_reservation(self):
        self.a.topup('operator','fund',self.customer,100,'promotional')
        self.checkpoint()
        self.fails('reservation_binding_mismatch',lambda:self.checkpoint(node='node-two',project=SECOND,consumed=0))
        self.fails('project_node_mismatch',lambda:self.checkpoint(node='node-two',project=PROJECT))

    def test_unpaid_close_is_rolled_back_including_the_debit(self):
        self.a.topup('operator','fund',self.customer,60,'promotional')
        self.checkpoint()
        self.fails('unpaid_usage_requires_reconciliation',lambda:self.checkpoint(request='close',consumed=60,unpaid=10,close=True))
        self.assertEqual(self.a.wallet(self.customer)['spent_microcredits'],0)
        self.assertEqual(self.a.wallet(self.customer)['reserved_microcredits'],60)


if __name__=='__main__':unittest.main()

"""Real PostgreSQL contract tests. Requires an explicitly disposable database."""
import concurrent.futures
import os
import unittest
from authority import Failure
from postgres_authority import PostgresAuthority

DSN=os.environ.get('GAP_TEST_POSTGRES_DSN')
OWNER='did:gap:'+'a'*64
PROJECT='prj_'+'a'*24
SECOND='prj_'+'b'*24


@unittest.skipUnless(DSN and os.environ.get('GAP_TEST_POSTGRES_DISPOSABLE')=='1',
                     'explicit disposable PostgreSQL database required')
class PostgresTests(unittest.TestCase):
    def test_real_authority_atomicity_and_retry_across_instances(self):
        a=PostgresAuthority(DSN,'postgres-contract-test')
        customer=a.create_customer('operator','customer','Contract test')['customer_id']
        a.attach_principal('operator','owner',customer,'agent',OWNER)
        a.attach_project('operator','project',customer,PROJECT,'node-one',OWNER)
        a.attach_project('operator','second',customer,SECOND,'node-two',OWNER)
        # Above int32: PostgreSQL must keep SQLite's integer capacity.
        a.topup('operator','fund',customer,10**12,'promotional')
        first=a.debit('node-one','debit',PROJECT,10**12-1)
        b=PostgresAuthority(DSN,'postgres-contract-test')
        self.assertEqual(b.debit('node-one','debit',PROJECT,10**12-1),first)
        with self.assertRaisesRegex(Failure,'request_id_conflict'):
            b.debit('node-one','debit',PROJECT,10**12-2)
        def spend(pair):
            try:
                PostgresAuthority(DSN,'postgres-contract-test').debit(pair[0],'last-credit',pair[1],1)
                return 'debited'
            except Failure as error:return error.code
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            outcomes=list(pool.map(spend,[('node-one',PROJECT),('node-two',SECOND)]))
        self.assertCountEqual(outcomes,['debited','insufficient_credits'])
        self.assertEqual(a.wallet(customer)['balance_microcredits'],0)
        self.assertEqual(a.wallet(customer)['spent_microcredits'],10**12)
        def reject(db):
            db.execute('UPDATE customers SET balance=123 WHERE id=?',(customer,))
            raise Failure('intentional_rejection')
        with self.assertRaisesRegex(Failure,'intentional_rejection'):
            a.mutation('operator','rollback',{},reject)
        self.assertEqual(a.wallet(customer)['balance_microcredits'],0)
        with a.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM operations WHERE id='rollback'").fetchone()[0],0)
            self.assertEqual(db.execute('SELECT sum(delta) FROM wallet_entries').fetchone()[0],0)
        import suspension
        body=dict(customer_id=customer,active=True,expected_revision=0,reason='Contract test suspension')
        suspension.set_policy(a,'suspend',body)
        self.assertIn(PROJECT,suspension.snapshot(a,'node-one')['projects'])
        suspension.set_policy(b,'restore',dict(body,active=False,expected_revision=1))
        self.assertFalse(suspension.status(a,customer)['policy']['active'])
        token=a.issue(customer,OWNER)['token']
        self.assertEqual(b.authenticate(token)['customer'],customer)
        b.revoke(token)
        with self.assertRaisesRegex(Failure,'invalid_control_credentials'):a.authenticate(token)
        with self.assertRaisesRegex(Failure,'operator_database_mismatch'):
            PostgresAuthority(DSN,'other-operator')


if __name__=='__main__':unittest.main()

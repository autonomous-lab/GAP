import concurrent.futures
import json
from pathlib import Path
import tempfile
import unittest

from authority import Authority, Failure

OWNER = 'did:gap:' + 'a' * 64
AGENT = 'did:gap:' + 'b' * 64
PROJECT = 'prj_' + 'a' * 24
SECOND = 'prj_' + 'b' * 24


class AuthorityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'authority.sqlite'
        self.now = 1000
        self.a = Authority(self.path, 'operator-one', lambda: self.now)
        self.customer = self.a.create_customer('operator', 'customer', 'Customer')['customer_id']
        self.a.attach_principal('operator', 'owner', self.customer, 'agent', OWNER)
        self.a.attach_project('operator', 'project', self.customer, PROJECT, 'node-one', OWNER)
        self.a.attach_project('operator', 'second', self.customer, SECOND, 'node-two', OWNER)

    def fails(self, code, fn):
        with self.assertRaises(Failure) as found:
            fn()
        self.assertEqual(found.exception.code, code)

    def test_concurrent_nodes_cannot_spend_the_same_final_credit(self):
        self.a.topup('operator', 'fund', self.customer, 1, 'promotional')
        def spend(pair):
            # Different connections/instances model separate node requests.
            authority = Authority(self.path, 'operator-one')
            try:
                authority.debit(pair[0], 'last-credit', pair[1], 1)
                return 'debited'
            except Failure as error:
                return error.code
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            outcomes = list(pool.map(spend, [('node-one', PROJECT), ('node-two', SECOND)]))
        self.assertCountEqual(outcomes, ['debited', 'insufficient_credits'])
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'], 0)
        self.assertEqual(self.a.wallet(self.customer)['spent_microcredits'], 1)

    def test_lost_response_retry_survives_restart_and_rejects_changed_request(self):
        initial = self.a.topup('operator', 'fund', self.customer, 100, 'promotional')
        first = self.a.debit('node-one', 'usage-1', PROJECT, 25)
        restarted = Authority(self.path, 'operator-one')
        self.assertEqual(restarted.topup('operator', 'fund', self.customer, 100, 'promotional'), initial)
        self.assertEqual(restarted.debit('node-one', 'usage-1', PROJECT, 25), first)
        self.assertEqual(restarted.wallet(self.customer)['balance_microcredits'], 75)
        self.fails('request_id_conflict', lambda: restarted.debit('node-one', 'usage-1', PROJECT, 26))
        self.fails('request_id_conflict', lambda: restarted.topup('operator', 'fund', self.customer, 100, 'paid'))
        with restarted.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM wallet_entries').fetchone()[0], 2)
            self.assertEqual(db.execute('SELECT sum(delta) FROM wallet_entries').fetchone()[0], 75)

    def test_node_cannot_debit_another_nodes_project(self):
        self.a.topup('operator', 'fund', self.customer, 100, 'promotional')
        self.fails('project_node_mismatch', lambda: self.a.debit('node-two', 'stolen', PROJECT, 20))
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'], 100)

    def test_cross_customer_binding_and_owner_grant_are_immutable(self):
        other = self.a.create_customer('operator', 'other', 'Other')['customer_id']
        self.fails('principal_binding_conflict', lambda: self.a.attach_principal('operator', 'steal', other, 'agent', OWNER))
        self.fails('project_binding_conflict', lambda: self.a.attach_project('operator', 'move', self.customer, PROJECT, 'node-two', OWNER))
        self.fails('owner_membership_required', lambda: self.a.attach_project('operator', 'hijack', other, SECOND, 'node-two', OWNER))
        self.fails('owner_grant_immutable', lambda: self.a.grant('operator', 'revoke-owner', PROJECT, OWNER, 'none'))

    def test_membership_does_not_grant_other_agents_projects(self):
        self.a.attach_principal('operator', 'agent', self.customer, 'agent', AGENT)
        token = self.a.issue(self.customer, AGENT)['token']
        self.assertEqual(self.a.projects(self.customer, AGENT)['projects'], [])
        self.a.grant('operator', 'view', PROJECT, AGENT, 'viewer')
        self.assertEqual(self.a.projects(self.customer, AGENT)['projects'][0]['role'], 'viewer')
        self.a.grant('operator', 'revoke', PROJECT, AGENT, 'none')
        principal = self.a.authenticate(token)
        self.assertEqual(self.a.projects(principal['customer'], principal['agent'])['projects'], [])

    def test_tokens_are_hashed_expire_revoke_and_do_not_cross_operators(self):
        issued = self.a.issue(self.customer, OWNER, 60)
        token = issued['token']
        with self.a.db() as db:
            values = '\n'.join(db.iterdump())
        self.assertNotIn(token, values)
        self.assertEqual(self.a.authenticate(token)['customer'], self.customer)
        other = Authority(Path(self.temp.name) / 'other.sqlite', 'operator-two')
        self.fails('invalid_control_credentials', lambda: other.authenticate(token))
        self.fails('operator_database_mismatch', lambda: Authority(self.path, 'operator-two'))
        self.now += 60
        self.fails('invalid_control_credentials', lambda: self.a.authenticate(token))
        fresh = self.a.issue(self.customer, None)['token']
        self.a.revoke(fresh)
        self.fails('invalid_control_credentials', lambda: self.a.authenticate(fresh))
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)

    def snapshot(self):
        return dict(owner_did=OWNER, balance=123456789, spent=42, remainder='123/730',
                    shadow_remainder='456/730', budget=1000000000, budget_spent=42,
                    budget_epoch=3, exhausted_at=None, retention_claim=None)

    def test_staging_never_duplicates_live_credits_and_preserves_fractional_carry(self):
        snapshot = self.snapshot()
        first = self.a.stage_import('operator', 'stage', self.customer, 'node-one', PROJECT, snapshot)
        self.assertFalse(first['spendable'])
        self.assertEqual(self.a.stage_import('operator', 'stage', self.customer, 'node-one', PROJECT, snapshot), first)
        self.a.stage_import('operator', 'stage-again', self.customer, 'node-one', PROJECT, snapshot)
        view = self.a.wallet(self.customer)
        self.assertEqual(view['balance_microcredits'], 0)
        self.assertEqual(view['staged_legacy_microcredits'], snapshot['balance'])
        self.fails('insufficient_credits', lambda: self.a.debit('node-one', 'cannot-spend-legacy', PROJECT, 1))
        with self.a.db() as db:
            self.assertEqual(json.loads(db.execute('SELECT payload FROM imports').fetchone()[0]), snapshot)
        self.fails('legacy_snapshot_changed', lambda: self.a.stage_import('operator', 'changed', self.customer, 'node-one', PROJECT, dict(snapshot, balance=1)))
        self.fails('legacy_owner_mismatch', lambda: self.a.stage_import('operator', 'wrong-owner', self.customer, 'node-one', PROJECT, dict(snapshot, owner_did=AGENT)))

    def test_rejected_amounts_and_overflow_leave_no_partial_ledger_entry(self):
        for value in (True, 1.5, -1, 0, 10**15 + 1):
            self.fails('invalid_microcredits', lambda: self.a.topup('operator', 'bad', self.customer, value, 'paid'))
        self.a.topup('operator', 'max', self.customer, 10**15, 'unclassified')
        self.fails('invalid_microcredits', lambda: self.a.topup('operator', 'overflow', self.customer, 1, 'paid'))
        self.assertEqual(self.a.wallet(self.customer)['balance_microcredits'], 10**15)
        with self.a.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM wallet_entries').fetchone()[0], 1)

    def test_import_rejects_invalid_fractions_and_nan(self):
        for key, value, code in [('remainder', '1/0', 'invalid_fractional_carry'),
                                 ('remainder', '-1', 'invalid_fractional_carry'),
                                 ('remainder', True, 'invalid_fractional_carry'),
                                 ('exhausted_at', float('nan'), 'invalid_exhaustion_time')]:
            self.fails(code, lambda: self.a.stage_import('operator', 'bad', self.customer, 'node-one', PROJECT, dict(self.snapshot(), **{key: value})))


if __name__ == '__main__':
    unittest.main()

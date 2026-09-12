import json
from pathlib import Path
import tempfile
import threading
import unittest
import urllib.request
import urllib.error

from authority import Authority
from service import Application, Server
from test_authority import OWNER, AGENT, PROJECT, SECOND


class ServiceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.a = Authority(Path(self.temp.name) / 'control.sqlite', 'one')
        self.app = Application(self.a, 'admin-test', {'node-one': 'node-one-test', 'node-two': 'node-two-test'})
        self.server = Server(('127.0.0.1', 0), self.app)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.close)
        self.url = 'http://127.0.0.1:' + str(self.server.server_address[1])

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(5)

    def call(self, path, token='', body=None):
        request = urllib.request.Request(self.url + path, data=json.dumps(body).encode() if body is not None else None,
                                         headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
        try:
            response = urllib.request.urlopen(request, timeout=5)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            self.assertEqual(response.headers['Cache-Control'], 'no-store')
            return response.status, json.loads(response.read())

    def operator(self, action, request='x', **body):
        status, result = self.call('/operator', 'admin-test', dict(action=action, request_id=request, **body))
        self.assertEqual(status, 200, result)
        return result

    def customer(self):
        customer = self.operator('create-customer', 'customer', label='Customer')['customer_id']
        self.operator('attach-principal', 'owner', customer_id=customer, kind='agent', subject=OWNER)
        self.operator('attach-project', 'project', customer_id=customer, project_id=PROJECT, node_id='node-one', owner_did=OWNER)
        self.operator('attach-project', 'second', customer_id=customer, project_id=SECOND, node_id='node-two', owner_did=OWNER)
        return customer

    def test_two_nodes_share_one_wallet_with_authenticated_idempotent_http(self):
        customer = self.customer()
        self.operator('topup', 'fund', customer_id=customer, amount_microcredits=100, source='promotional')
        self.app.allow_debits = True
        first = self.call('/node', 'node-one-test', dict(action='debit', request_id='one', project_id=PROJECT, amount_microcredits=30))
        self.assertEqual(first[0], 200)
        self.assertEqual(self.call('/node', 'node-one-test', dict(action='debit', request_id='one', project_id=PROJECT, amount_microcredits=30)), first)
        second = self.call('/node', 'node-two-test', dict(action='debit', request_id='two', project_id=SECOND, amount_microcredits=70))
        self.assertEqual(second[0], 200)
        self.assertEqual(second[1]['balance_microcredits'], 0)
        self.assertEqual(self.call('/node', 'node-one-test', dict(action='debit', request_id='three', project_id=PROJECT, amount_microcredits=1))[0], 402)
        token = self.operator('issue-token', customer_id=customer)['token']
        self.assertEqual(self.call('/v1/wallet', token)[1]['spent_microcredits'], 100)
        self.assertEqual(len(self.call('/v1/projects', token)[1]['projects']), 2)

    def test_node_token_cannot_fund_or_mint_customer_credentials(self):
        self.customer()
        for token in ('', 'wrong', 'node-one-test', 'node-two-test'):
            status, _ = self.call('/operator', token, dict(action='create-customer', request_id='steal', label='Bad'))
            self.assertIn(status, (401, 403))
        self.assertEqual(self.call('/node', 'node-two-test', dict(action='project', project_id=PROJECT))[0], 403)
        self.assertEqual(self.call('/node', 'admin-test', dict(action='project', project_id=PROJECT))[0], 403)

    def test_readiness_uses_node_identity_and_live_feature_flags(self):
        status,data=self.call('/node','node-one-test',dict(action='readiness'))
        self.assertEqual(status,200);self.assertEqual(data['node_id'],'node-one')
        self.assertEqual(data['protocol'],'fleet-admission-v1')
        self.assertFalse(data['reservations']);self.assertFalse(data['capacity'])
        self.app.allow_reservations=True;self.app.allow_capacity=True
        self.assertTrue(self.call('/node','node-two-test',dict(action='readiness'))[1]['capacity'])
        self.assertIn(self.call('/node','wrong',dict(action='readiness'))[0],(401,403))

    def test_disabled_debits_cannot_accidentally_activate_production_billing(self):
        self.customer()
        status, health = self.call('/health')
        self.assertEqual(status, 200)
        self.assertFalse(health['legacy_cutover'])
        self.assertFalse(health['online_debits_enabled'])
        status, result = self.call('/node', 'node-one-test', dict(action='debit', request_id='one', project_id=PROJECT, amount_microcredits=1))
        self.assertEqual((status, result['error']['code']), (409, 'online_debits_disabled'))

    def test_client_scope_logout_and_unknown_node(self):
        customer = self.customer()
        self.operator('attach-principal', 'agent', customer_id=customer, kind='agent', subject=AGENT)
        token = self.operator('issue-token', customer_id=customer, agent_did=AGENT)['token']
        self.assertEqual(self.call('/v1/projects', token)[1]['projects'], [])
        self.assertEqual(self.call('/operator', token, dict(action='wallet', customer_id=customer))[0], 403)
        self.assertEqual(self.call('/node', token, dict(action='project', project_id=PROJECT))[0], 403)
        status, _ = self.call('/operator', 'admin-test', dict(action='attach-project', request_id='unknown-node', customer_id=customer,
                                                              project_id=PROJECT, node_id='foreign-operator', owner_did=OWNER))
        self.assertEqual(status, 400)
        self.assertEqual(self.call('/v1/logout', token, {})[0], 200)
        self.assertEqual(self.call('/v1/account', token)[0], 401)

    def test_malformed_body_is_bounded_and_sanitized(self):
        for body in ([], {'action': 'topup'}, {'action': 'create-customer', 'request_id': 'bad', 'label': True}):
            status, value = self.call('/operator', 'admin-test', body)
            self.assertEqual(status, 400)
            self.assertNotIn('admin-test', json.dumps(value))
        self.assertEqual(self.call('/operator', 'admin-test', {'padding': 'a' * 65536})[0], 413)

    def test_unavailable_database_is_not_reported_as_zero_credit_or_healthy(self):
        customer = self.customer()
        token = self.operator('issue-token', customer_id=customer)['token']
        path = Path(self.a.path)
        saved = path.with_suffix('.saved')
        path.rename(saved)
        path.mkdir()
        try:
            for route in ('/health', '/v1/wallet'):
                status, result = self.call(route, token)
                self.assertEqual(status, 503)
                self.assertEqual(result['error']['code'], 'authority_unavailable')
                self.assertNotIn('balance_microcredits', result)
        finally:
            path.rmdir()
            saved.rename(path)
        self.assertEqual(self.call('/health')[0], 200)

    def test_cached_checkpoint_includes_fresh_time_without_extending_its_lease(self):
        customer=self.customer()
        self.operator('topup','fund',customer_id=customer,amount_microcredits=100,source='promotional')
        self.app.allow_reservations=True
        self.a.clock=lambda:1000
        body=dict(action='checkpoint',request_id='reserve',project_id=PROJECT,owner_did=OWNER,
                  reservation_id='reservation',consumed_microcredits=0,unpaid_microcredits=0,
                  target_microcredits=100,lease_seconds=10)
        status,first=self.call('/node','node-one-test',body)
        self.assertEqual(status,200)
        self.a.clock=lambda:1015
        status,retry=self.call('/node','node-one-test',body)
        self.assertEqual(status,200)
        self.assertEqual(first['lease_expires_at'],retry['lease_expires_at'])
        self.assertGreater(retry['authority_now'],retry['lease_expires_at'])
        self.assertEqual(self.a.wallet(customer)['reserved_microcredits'],100)


if __name__ == '__main__':
    unittest.main()

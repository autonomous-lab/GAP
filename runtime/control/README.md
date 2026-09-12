# Operator account authority: foundation

This service is the single transactional registry and wallet for **one operator**.
It supports customers, human/agent membership, project placement and explicit
agent project grants. Nodes have distinct service credentials and can only read
their own project bindings. Customer credentials are hashed, expire within one
hour, can be revoked, and are accepted only by this control service. An agent's
membership alone does not give access to every project in the customer account.
Human membership uses an opaque subject ID; email proof still belongs to the
registration service. Operator assertions do not migrate email verification.

**Delivery boundary:** existing node login, email registration, project ownership
checks and worker metering are not connected to this service yet. The default
configuration disables online debits. Existing workloads and balances continue
using their original node. No production authentication or billing cutover is
performed by installing this service. This does not complete the shared-account
kanban card or global quota/lease enforcement.

## State and trust

SQLite WAL, FULL synchronization and `BEGIN IMMEDIATE` serialize mutations across
connections. Each database records its immutable operator ID and refuses to open
under another operator. Deploy one authoritative instance per operator, with its
own credentials and durable database. Do not copy it onto a second writable node.
Each independent operator starts a separate database and issues separate tokens.

The operator credential administers accounts, grants, funding and staging. It is
an infrastructure credential for this private service, not a replacement for the
individual administrator console. Every mutation stores its actor, request ID,
request digest, result and time. Node credentials cannot fund wallets, attach
projects or mint client credentials. A node may debit only its own registered
projects, and cannot supply a different customer or node identity in the body.
Do not send these credentials to untrusted federated discovery endpoints.

Control client tokens are not node bearer tokens. Do not forward them to execution
nodes. Destination-scoped signed execution credentials, human sign-in integration,
node revocation and membership lifecycle remain in the integration phase.

Wallet amounts are integer microcredits (1,000,000 per USD credit). One customer
wallet covers its projects on all trusted nodes. Funding is classified as paid,
promotional or unclassified; this classification is not payment verification.
No payment or Stripe integration is enabled. Each funding/debit request needs a
stable `request_id`; retry with exactly the same payload after a lost response.
Changing a payload under an existing ID returns `request_id_conflict`.
Failed transactions leave no partial balance update. Entries preserve node and
project attribution. The direct online debit primitive rejects insufficient
funds atomically; **it is not an offline execution lease or spending reservation**.

## Install once on the operator control host

Create private configuration/state directories owned by UID 10001, mode 0700:
`data/gap-control/config/` and `data/gap-control/state/`. Generate distinct random
credentials (at least 32 random bytes, base64url encoded) in mode-0600 files:
`operator.token`, `node-01.token`, `node-02.token`. Never commit them or print them.

`data/gap-control/config/control.json`:

```json
{
  "operator_id": "elestio",
  "database": "/data/authority.sqlite",
  "operator_token_file": "/config/operator.token",
  "node_token_files": {
    "node-01": "/config/node-01.token",
    "node-02": "/config/node-02.token"
  },
  "allow_online_debits": false
}
```

From the repository root:

```sh
python3 scripts/deploy-check.py
docker compose --project-directory . -f runtime/control/deploy.yml up -d --build
docker compose --project-directory . -f runtime/control/deploy.yml ps
```

The bridge listener is `172.17.0.1:8096`, not an Internet endpoint. Remote access
must use the operator's verified TLS edge or an authenticated tunnel; never open
8096 on a public interface. Do not mount execution-node state or Docker sockets.
The container has no capabilities and a read-only root filesystem. `/health`
explicitly reports the foundation phase and whether debits are enabled.

## Operator API and CLI

POST `/operator`, using the operator token file. Supported `action` values:

| Action | Fields, in addition to `action` |
| --- | --- |
| `create-customer` | `request_id`, `label` |
| `attach-principal` | `request_id`, `customer_id`, `kind` (`human`/`agent`), `subject`, optional `verified` for humans |
| `attach-project` | `request_id`, `customer_id`, `project_id`, `node_id`, `owner_did` |
| `grant` | `request_id`, `project_id`, `agent_did`, `role` (`viewer`/`operator`/`none`) |
| `topup` | `request_id`, `customer_id`, `amount_microcredits`, `source` |
| `wallet` | `customer_id` |
| `projects` | `customer_id`, optional `after` cursor |
| `issue-token` | `customer_id`, optional `agent_did`, optional `ttl_seconds` (60–3600) |
| `stage-import` | `request_id`, `customer_id`, `node_id`, `project_id`, `snapshot` |

Project attachment requires the owner's agent membership first. Binding conflicts
never silently transfer ownership. Non-owner agent grants can be revoked without
reissuing a token. Project lists paginate at 100 rows. `issue-token` deliberately
does not persist its secret in the operation log; an uncertain retry issues a new
expiring token. Use client POST `/v1/logout` to revoke that credential immediately.

```sh
python3 scripts/fleet-control.py call --request-file /private/create-customer.json
python3 scripts/fleet-control.py call --request-file /private/issue-token.json \
  --output /private/new-control-token.json
```

The CLI refuses redirects, refuses remote cleartext credentials, does not expose
tokens on stdout and refuses to overwrite its private output files. Client GET
`/v1/account`, `/v1/wallet` and `/v1/projects` use the issued control credential.
POST `/node` authenticates a configured node and accepts `action: project` with
`project_id`. Its `debit` action additionally requires `request_id` and
`amount_microcredits`, and is disabled by default.

## Legacy migration staging

On each existing worker host, export one consistent **read-only** SQLite snapshot:

```sh
python3 scripts/fleet-control.py export-legacy \
  --ledger data/gap-compose/worker/jobs/microvm-credits.sqlite \
  --node node-01 --output /private/node-01-wallet-inventory.json
```

Check the configured worker state directory before using that example path.
This command never stops metering or modifies a ledger. The export contains DIDs,
project IDs and financial state, so keep it private. It contains no API tokens,
identity seeds or passwords. Rational fractional carries, budgets, exhaustion
timestamps and deletion claims are preserved without converting through floats.

Create customer and principal/project bindings from verified source ownership.
Do not merge different DIDs merely because an email or display label matches;
human linkage needs a verified association. Post each exported `snapshot` with
`stage-import`. The unique `(source node, project)` key makes repeated inventory
imports idempotent. A changed snapshot is rejected rather than silently replacing
the reviewed checkpoint. A staged import has **zero effect on spendable balance**.
The wallet reports staged credits separately and explicitly as non-spendable.

There is intentionally no activation endpoint in this first delivery. The next
cutover must provide all of the following before funds become spendable:

1. Fence the source writer durably, flush its final metering sample and archive
   its audit ledger, tariff history, VM checkpoints and fractional carries.
2. Compare the final source checkpoint with the reviewed inventory, reconcile
   differences and durably record one unique transfer receipt.
3. Atomically import the final balance exactly once and switch the worker to
   bounded reservations and leases; remove the old local spending path.
4. Recover interruption at every boundary, without spending from both writers.
   A controller outage must never trigger the 72-hour zero-credit purge.

Backup the database using SQLite's online backup API, not by copying a live main
file without its WAL. Test restore in isolation and preserve the operator ID.
Restoring an old wallet into production requires reconciliation and writer fencing;
it must not resurrect already spent credits. High availability and encrypted
off-host backup/key management remain separate delivery gates.

## Verification

```sh
python3 -m unittest discover -s runtime/control -p 'test_*.py'
```

Tests exercise real HTTP, two authenticated node actors sharing one wallet,
concurrent final-credit spending, restart/replay, token expiry/revocation,
cross-operator rejection, project grants, funding overflow rollback, and legacy
staging that cannot duplicate or spend live balances. Existing worker metering
and node HTTP suites remain required when connecting the service to those paths.

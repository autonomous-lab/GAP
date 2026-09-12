# Operator accounts, spending reservations and worker leases

This service is the single transactional registry and wallet for **one operator**.
It supports customers, human/agent membership, project placement and explicit
agent project grants. Nodes have distinct service credentials and can only read
their own project bindings. Customer credentials are hashed, expire within one
hour, can be revoked, and are accepted only by this control service. An agent's
membership alone does not give access to every project in the customer account.
Human membership uses an opaque subject ID; email proof still belongs to the
registration service. Operator assertions do not migrate email verification.

**Delivery boundary:** worker metering now supports explicit per-project opt-in
through spending reservations and short execution leases. Verified local identities
can explicitly connect to the operator account and obtain project-scoped signed
credentials; local email proof and project ownership remain on their source node.
Existing workloads and balances continue using their original ledger until a
separate, fenced migration is completed. Installing this service does not migrate
them. The central VM-count/CPU/RAM reservation protocol is implemented below;
worker create/resize/destroy operations enforce it for explicitly opted-in, new
projects. Existing workloads are **not adopted or migrated automatically**.

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
nodes. Exchange them for destination-scoped project credentials as described below.
Human single-sign-on UI, node revocation and the full membership lifecycle remain
in the integration phase.

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
explicitly reports which reservation and project-access protocols are enabled.

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
| `quotas` | `customer_id` |
| `set-quotas` | `request_id`, `customer_id`, `expected_revision`, `limits` (`max_vms`, `cpu_quarters`, `memory_mib`) |
| `capacity-list` | `customer_id`, optional `after` cursor |
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

## Worker connection and partition behavior

Enable `allow_reservations: true` in the private control configuration. Keep
`allow_online_debits: false`: workers use cumulative checkpoints, not direct debits.
On the control host only, set `GAP_FLEET_RELAY_ENABLED=1` in the node's `.env`
and recreate its web service. This exposes POST `/v1/fleet/node` through the
existing verified HTTPS origin and relays **only** to `/node` on the fixed local
bridge authority. It does not expose `/operator`, accept caller-selected URLs,
follow redirects or forward human/admin sessions. The private service authenticates
each distinct node credential. The relay applies the node's rate limits; size
them for fleet checkpoint traffic before increasing the customer/node limits.

The worker's `runner.json` accepts:

```json
{
  "fleet_billing": {
    "url": "https://gap.geta.team/v1/fleet",
    "operator_id": "elestio",
    "node_id": "node-02",
    "token_file": "/config/fleet-node.token",
    "projects": [],
    "target_microcredits": 100000,
    "lease_seconds": 30
  }
}
```

Node 01 can use `http://172.17.0.1:8096` locally. Copy only that node's credential
into its private worker configuration, never the operator credential or another
node's credential. The client verifies HTTPS certificates, refuses redirects and
allows cleartext only for loopback/the local Docker bridge. Configuration and
credentials are loaded at worker startup. Preserve running VMs before replacing
their worker container.

`projects: []` prepares the connection without changing billing for any project.
Opt-in is accepted only for a financially empty project, with enforced pricing
and an existing authoritative customer/agent/project binding. A funded or used
legacy wallet returns `legacy_wallet_migration_required`; it is never copied into
the authority automatically. A durable local binding blocks local top-ups and
shadow mode. Removing the fleet configuration does not reactivate the local
spending path; it fences execution until correct configuration is restored.
Bindings also pin their operator and node identities. Do not clone a writable
worker ledger onto another host or restore an old checkpoint as current state;
that requires fencing and reconciliation, not an automatic lease renewal.

POST `/node` with action `checkpoint` carries a stable `request_id`, `project_id`,
`owner_did`, durable `reservation_id`, cumulative `consumed_microcredits`, current
`unpaid_microcredits`, `target_microcredits` (up to 1,000,000) and `lease_seconds`
(10–60). It settles newly reported consumption and reserves additional funds in
one transaction. A node can operate only on its registered projects. An active
reservation prevents a second reservation for the same node/project. Wallets show
free, reserved, total remaining and spent microcredits separately.

The worker records the pending request before sending it and applies each returned
allocation exactly once. A lost response or restart reuses that request. Every
response includes fresh authority time, even for a cached operation; replaying an
old response cannot issue a fresh lease. Worker deadlines use a monotonic clock,
start before the HTTP request, and are never restored from disk after a restart.
The worker must contact the authority before execution can resume after reboot.

An independent meter continues during long deployments that hold lifecycle locks.
A separate watchdog makes no HTTP calls and does not take those locks. Expiry
closes tracked connections and pauses guest CPUs, then the lifecycle loop hibernates
the VM. Checks before QEMU execution/resume prevent an expired lease from starting
guest instructions. Preemption is checked once per second, plus the bounded QMP
monitor timeout; this is not a promise of zero scheduling latency.

Controller failures, confirmed exhausted credit and credit already reserved by
another node have distinct states. Failure never becomes a zero-balance response.
Reservation expiry never refunds credits that may already have been consumed.
An explicit final checkpoint with `close: true`, after the worker is fenced and
all usage settled, returns the unused amount. The current worker intentionally
does not close reservations automatically after VM destruction; operator-driven
drain/reconciliation is required before releasing a possibly outstanding balance.

Retained disks continue to accumulate usage during a partition. Any unpaid usage
is repaid from the next allocation before execution resumes, with a separate
arrears entry and conserving finance projections. The local allowance is labelled
`balance_scope: node_reservation`; it must not be presented as the total customer
wallet. **Managed projects cannot use the old 72-hour local deletion timer.**
Their retention remains `preserve_until_authoritative_reconciliation` until a
customer-wide retention decision protocol is delivered. Do not activate legacy
production projects before that migration/retention gate is complete.

## Global capacity admission protocol

Set `allow_capacity: true` in the private control configuration to enable the
node protocol. Deploy protocol version 2 before enabling worker opt-in. `/health`
reports `capacity_protocol: 2` and
`worker_capacity_enforcement: "explicit_project_opt_in"`.
The authority neither scans nor adopts existing VM catalogs. Keep the existing local quota
checks and physical-host admission checks; they solve different constraints.
No client is migrated by enabling this flag.

Each customer defaults to one VM, 4 CPU quarters (1 vCPU) and 1024 MiB RAM.
`set-quotas` replaces all three limits in one audited transaction and requires
the current quota revision (`0` for defaults). Zero is allowed; increasing a
limit requires operator credentials. Integer CPU quarters preserve fractional
allocations without float arithmetic. These are provisioned allocations, counted
across the customer's agents, projects and trusted nodes, including stopped and
hibernated VMs. They are not measures of current CPU consumption or free host RAM.
Disk quotas, scheduler placement and central retention are separate work.

Client GET `/v1/quotas` returns only the authenticated customer's aggregate
limits, allocations, revision and `over_limit` dimensions, including for an agent
credential. It does not expose other agents' project or VM identifiers.
Operator `capacity-list` paginates at 100 records with `next_cursor`, including
released tombstones. A node can inspect only its own project's exact VM using
`capacity-get`. Administrative quota reductions preserve existing and pending
allocations, report over-limit dimensions, and allow reductions. They do not
pause workloads or cancel an already reserved creation.

All node actions use POST `/node` (or the existing HTTPS `/v1/fleet/node` relay):

| Action | Required fields |
| --- | --- |
| `capacity-get` | `project_id`, `vm_id` |
| `capacity-prepare` | `request_id`, `project_id`, `owner_did`, `vm_id`, `expected_revision`, `cpu_quarters`, `memory_mib` |
| `capacity-finish` | `request_id`, `project_id`, `vm_id`, `expected_revision`, `outcome`, `evidence_id`, and `transition_id` for commit/abort |
| `capacity-cancel-create` | `request_id`, `project_id`, `owner_did`, `vm_id`, `evidence_id` |
| `capacity-abort-resize` | `request_id`, `project_id`, `vm_id`, `expected_revision` (before prepare), `transition_id`, `evidence_id` |

The authenticated node determines placement; a supplied node/customer identity
cannot override it. Project ownership and the immutable VM/project/node binding
are checked before mutation. Each response includes `operator_id`, binding,
`revision`, `state`, `transition_id`, `committed` and `target` resources. Completion
responses also persist the local evidence receipt ID and outcome in the operation
log. Quota conflicts return `409 customer_quota_exceeded_max_vms`,
`customer_quota_exceeded_cpu_quarters` or `customer_quota_exceeded_memory_mib`.
Storage/transport failures remain unavailable errors, never a quota result.

The protocol is deliberately conservative:

1. **Prepare creation:** the node durably chooses a fresh `vm_` generation and
   stable request ID *before* sending `capacity-prepare`, with revision 0. The
   authority reserves count, CPU and RAM atomically and returns `pending`.
   Concurrent nodes cannot both consume the same last place.
2. **Apply and confirm:** after durably applying the local allocation, send
   `capacity-finish`, outcome `commit`, using the returned revision and
   `transition_id`. The state becomes `active` with a new revision.
3. **Resize:** prepare with the current active revision and desired full CPU/RAM
   allocation. Until confirmation, the reservation holds the componentwise
   maximum of old and new resources. A pending CPU reduction cannot fund a new
   VM before the old allocation is actually reduced. Only one transition can be
   pending for a VM.
4. **Abort:** only after durably preventing the uncertain creation from ever
   executing, or restoring the previous resource configuration for a resize,
   finish with outcome `abort` and the pending transition/revision. Aborted
   creation leaves a permanent released tombstone; an aborted resize returns to
   the previous active allocation. Do not abort merely because a job timed out.
5. **Release:** after successful destruction and a durable fence preventing
   restart, finish an active allocation with outcome `release`, its current
   revision and no `transition_id`. A pending transition must be reconciled first.
   Stopping or hibernating a VM is never grounds for releasing its quota.

`evidence_id` is a stable identifier of the trusted node's durable local result.
The central service records that assertion; it does not independently inspect
QEMU or cryptographically prove destruction. Workers must retain the referenced
receipt, serialize local operations per VM and fence abandoned generations.
The worker implements this protocol using a separate `fleet-capacity.sqlite` under
its hypervisor state root, independent of jobs whose payloads can be discarded.
Bindings persist the operator, node and owner. Each intent keeps its VM generation,
request IDs, target/previous resources, completion/cancellation receipts and phase.
Do not edit these records or issue manual capacity completions for live workers.

Opting a project into `fleet_billing.projects` also requires capacity admission;
there is no independent switch permitting credit-managed VMs to skip quotas.
A project with existing non-destroyed VMs cannot be silently adopted. Financially
used wallets still require the separate fenced migration. Removing the fleet
configuration, including disabling serverless mode, does not remove persisted
capacity fences: execution fails until the correct configuration and authority
are restored. Local count/resource limits and physical-host admission still apply.

A fresh generation and prepare request are durable before any local allocation.
CPU execution waits for a confirmed active allocation plus credit and policy
checks. Recovery reads the current central state before permitting execution;
process-local admission is never restored from a cached response alone. Stopped
and hibernated VMs continue consuming their full provisioned quotas.

Recovery never replays VM creation or resize. An absent creation is locally fenced
before `capacity-cancel-create`, which writes a released tombstone even if prepare
has not yet arrived. This prevents a delayed request from reserving an abandoned
generation. A resize still at its old resources uses `capacity-abort-resize`:
it cancels that exact pending transition or advances the old active revision to
reject a prepare still in transit. Thus a refused resize does not wedge the
existing VM. Applied target resources are committed instead; mismatches hold the
reservation and block execution for reconciliation. A partially created catalog
can retain an active allocation while remaining `creating`; destroy it explicitly
before retrying with a new generation.

Destruction records the exact VM and retain/delete choice before touching disks.
Recovery may finish that already authorized local destruction, including a crash
after disk removal/move but before the catalog update. It then releases capacity
with a durable receipt. A lost acknowledgement is replayed verbatim. Controller
outages never free quota speculatively, and pending intents with no catalog are
also reconciled by the lifecycle loop. An existing credit lease can continue
until its own deadline; capacity reservations are not expiring execution leases.

There is **no capacity timeout, expiry refund or operator force-release**.
Unknown outcomes continue consuming quota across controller restart, even after
credit execution leases expire. Use `capacity-get` and inspect the node's durable
state to reconcile. Never infer absence from an unreachable node. Retry a lost
response with the identical request ID/body. Idempotency responses are historical:
an old successful prepare can be replayed after its VM was released. It is not a
fresh authorization to execute. Read current state and check the expected revision
and locally fenced generation before acting. A released VM ID can never reserve
capacity again; create a new generation instead.

Online SQLite backups preserve pending holds, revisions, receipts and tombstones.
A restored old database must never replace the live authority without fencing and
reconciling every worker; otherwise it can forget newer reservations. This is the
same single-writer recovery boundary as for shared wallet balances.

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

There is intentionally no legacy activation endpoint in this delivery. The next
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
python3 -m unittest discover -s runtime/compose -p 'test_*.py'
```

To test the control image in isolation, mount only the CLI used by its tool
tests; that operator script is intentionally outside the service image:

```sh
docker build -t gap-control-candidate -f runtime/control/Dockerfile .
docker run --rm --read-only --tmpfs /tmp:rw,nosuid,nodev --network none \
  --cap-drop ALL --security-opt no-new-privileges \
  -v "$PWD/scripts/fleet-control.py:/scripts/fleet-control.py:ro" \
  --entrypoint python3 gap-control-candidate \
  -m unittest discover -s /opt/control -p 'test_*.py'
```

Tests exercise real HTTP, two authenticated node actors sharing one wallet,
concurrent final-credit spending, restart/replay, token expiry/revocation,
cross-operator rejection, project grants, funding overflow rollback, and legacy
staging that cannot duplicate or spend live balances. Existing worker metering
and node HTTP suites remain required when connecting the service to those paths.

The real KVM test uses isolated node/worker/controller state and only a read-only
guest base image mount. It stops the controller while holding a deployment lock,
checks open TCP closure and CPU preemption, verifies retained hibernation, then
restarts the controller and checks exact settlement and preserved-memory resume.
It also rejects a second VM through global admission, verifies an allowed resize
and rejected growth, then loses a destruction acknowledgement and reconstructs
the capacity adapter before reconciling the released allocation. Use graceful
guest shutdown before resize: an explicit forced power-off can lose unflushed
guest writes and is not a disk-durability test.
Run on a KVM host from the repository root (node image argument varies by host):

```sh
docker build -t gap-fleet-runner-candidate -f runtime/compose/Dockerfile .
docker build -t gap-fleet-kvm-test -f runtime/compose/Dockerfile.fleet-test \
  --build-arg GAP_TEST_NODE_IMAGE=gap-node-01-gap-node .
docker run --rm --device /dev/kvm --group-add "$(stat -c %g /dev/kvm)" \
  --memory 4g --cpus 3 \
  -v "$PWD/data/gap-compose/guest-image:/images:ro" gap-fleet-kvm-test
```

Do not mount a production VM catalog, wallet, customer volume or Docker socket
in this acceptance container. `/dev/kvm` needs its host group in the container;
passing `--device` alone does not grant UID 10001 access to a mode-0660 device.
# Verified account connection and project access

The account authority accepts an explicit connection from a node's identity
gateway. `POST /v1/fleet/connect` on the project node takes the existing local
owner bearer and `{ "request_id": "unique-operation", "project_id": "prj_..." }`.
The node checks project ownership, suspension and its durable verified-email
record before contacting the authority. An unverified legacy identity first uses
`POST /v1/identity/email` and `/v1/identity/email/verify` with its existing bearer.
The one-use code is bound to that DID; successful verification preserves its DID,
bearer, projects and balances. No existing verified address is silently replaced.

The authority associates the verified email with one customer per operator and
pins each agent to its source node. Email case is normalized; aliases and plus
addresses are not merged. Agents with the same verified address share a customer
but do not automatically receive rights to each other's projects. An existing
conflicting operator binding returns `legacy_identity_reconciliation_required`;
there is no automatic account merge. Repeating a request preserves the association
and issues a fresh short-lived credential without storing the token in operation
receipts. The connection does not import money or opt a project into worker billing.

The response contains `credential.token` (audience `control`, at most one hour).
On the authority's public gateway (node 01 for Elestio), use that credential with:

```text
GET  /v1/fleet/account
GET  /v1/fleet/projects?after=prj_...
GET  /v1/fleet/wallet
GET  /v1/fleet/quotas
POST /v1/fleet/project-token  {"project_id":"prj_...","ttl_seconds":120}
POST /v1/fleet/logout        {}
```

The projects list is filtered by the agent's grants and paginated in batches of
100. A project token requires owner/operator rights; viewer grants cannot mint a
management token. The returned `gapf1.` token is signed with Ed25519 and restricted
to the operator, destination node, exact project, owner and `project.manage` scope.
It works as a bearer on that project's existing Cloud and MicroVM management APIs,
including the dashboard browser-session exchange. It cannot create another project,
list an agent's other projects, connect an identity, or administer a node.
No agent seed, local owner bearer or email is embedded in the capability.

Nodes validate signatures and expiry locally, including after restart, and retain
their existing project/agent suspension and MicroVM approval checks. App ingress
does not require a central account lookup. Tokens default to 120 seconds and allow
30–300 seconds. Logout and grant removal block new issuance; an already issued
capability remains valid until expiry (at most five minutes), subject to local
suspension. A stored browser session does not extend the token's lifetime: clients
must obtain a new capability and reconnect when it expires. This API connection is
not yet a human single-sign-on UI or automatic cross-node project provisioning.

## Account-access deployment

Keep a private, persistent 32-byte hex signing seed on the control host only. Add
`signing_seed_file` and `identity_token_files` (node ID to private token file) to
`control.json`. Identity gateway credentials must be distinct from all worker and
operator credentials; workers cannot assert identities or mint account credentials.
The control image uses Alpine's `py3-cryptography` package for Ed25519 signing.

Configure the execution node with:

```text
GAP_FLEET_ACCESS_ENABLED=1
GAP_FLEET_OPERATOR_ID=elestio
GAP_FLEET_NODE_ID=node-01
GAP_FLEET_PUBLIC_KEY=<32-byte Ed25519 public key in hex>
GAP_FLEET_IDENTITY_URL=http://172.17.0.1:8096/identity
GAP_FLEET_IDENTITY_TOKEN=<this node's private identity gateway credential>
```

For node 02, use its own node ID/credential and
`https://gap.geta.team/v1/fleet/identity`. HTTPS certificates are verified,
redirects refused, transport bounded to two seconds and 64 KiB. Cleartext is
accepted only for a loopback or the local Docker bridge. No caller can choose
an upstream URL. `GAP_FLEET_RELAY_ENABLED=1` on node 01 exposes only the exact
identity/client route allowlist, never `/operator`. The signing seed stays off
execution nodes; deploy its public key through trusted configuration. Coordinate
key rotation with the five-minute token lifetime; replacing the sole trusted
public key immediately invalidates older tokens. Disabling access rejects fleet
capabilities while preserving local owner bearers.

Validation: `python3 -m unittest discover -s runtime/control -p 'test_*.py'`;
`GAP_TEST_BINARY=/path/to/gap-node python3 runtime/control/access_integration.py`
starts two isolated real nodes, an authority and a local SMTP sink. No email leaves
the fixture and no production database or wallet is used.

## Legacy wallet migration with a durable source fence

This migration path is limited to projects with **no VM assets**: no running,
stopped or hibernated VM and no retained disk. It never deletes or stops workloads
to make a project eligible. Projects with assets need a separate capacity/storage
migration. Billing must be enforced, historical estimated and debited usage must
agree, and no storage-deletion claim may exist. Reconcile discrepancies explicitly.

First attach the verified owner and project to the correct operator customer.
Back up the control SQLite database, worker ledger, capacity database and private
configuration using SQLite's backup API. Do not copy a live database without WAL.
Then use these private worker operations via `scripts/microvm-billing.py`:

1. `prepare-wallet-migration --project prj_... --owner did:gap:...` persists a
   capacity fence before a money fence. The result contains an immutable
   `transfer_id`, final `snapshot`, and `snapshot_digest`. No money is credited.
   Local spending, top-ups, budget changes and retention deletion are blocked.
2. Review the exact snapshot. POST `/operator` to the control authority with
   `action: "authorize-wallet-import"`, a unique `request_id`, and the returned
   `node_id`, `project_id`, `owner_did`, `transfer_id`, `snapshot`, `snapshot_digest`.
   This records approval of that exact amount; it still credits nothing.
3. `commit-wallet-migration --project prj_... --owner did:gap:...` asks the authority
   to apply the authorized transfer. Worker credentials alone cannot create a new
   import or change an approved amount. Credit and historical spent totals are
   applied once, even after a lost reply or concurrent retries. The worker then
   zeros its frozen local balance and records the receipt and historical baselines.
4. Add the project to that worker's `fleet_billing.projects` and restart the worker
   after verifying the host has no active VMs/jobs. The next allowance comes from
   the shared wallet. Historical usage is not charged again; fractional carries,
   budget counters and local financial history are preserved.

`wallet-migration-status` reads the durable source state. Repeating prepare/commit
reuses the original generation. If the authority credits the import but its reply
is lost, the source stays fenced until the same commit succeeds; never compensate
with a manual top-up or a second import. These fences do not expire and survive
removing the fleet configuration. The capacity fence also survives disabling the
serverless runtime. Preparation is an explicit cutover operation, with no automatic
cancel/unfence endpoint. A failure between the capacity and money fences is safe
to retry and has not credited money.

The transfer is recorded as `legacy_transfer`, not new cash or promotional funding.
Staged inventory remains audit data and is excluded from the pending amount once
the actual source transfer is credited. After a committed import, **do not restore
an old source ledger or downgrade the worker independently of the authority**:
doing so would restore previously transferred spending rights or replay historical
usage. Recovery must preserve both the credited central record and the source fence.

The isolated `runtime/compose/wallet_migration_kvm_test.py` acceptance test migrates
a funded fixture wallet, loses the credit acknowledgement, restores the ledger,
and then runs the real VM lifecycle and quota/partition tests. It preserves the
historical spend baseline and never mounts production catalogs or credit databases.


## Explicit operator confirmation of an existing owner

For an owner whose address was confirmed directly through an authenticated
support channel, the private operator API accepts `action: "confirm-identity"`
with `request_id`, `node_id`, `email`, `agent_did`, `project_id` and a mandatory
`reason` (10-1000 characters). Check the actual project owner on the source node
before using it. This is a deliberate administrative attestation, not an OTP
challenge. It records the confirmation method and reason in the durable operator
receipt, enforces the existing node/owner/email bindings, and never moves money.
Only operator credentials can use it; it is not exposed by the public relay.
The node's local OTP registry is unchanged. The response contains a secret control
credential; save it privately (`fleet-control.py --output`), never in logs.


## Human account and membership management

`/account` on the operator gateway redirects to the isolated management origin.
Humans sign in using a fresh email challenge (`POST /v1/fleet/login`, then
`/v1/fleet/login/verify`). Challenges are scoped to human login: an agent bearer
or an identity-link/signup code cannot grant account-wide access. A newly verified address opens an empty account without promotional funding. Operator-provided human control
credentials can also be entered without sending an email. Credentials stay in
page memory, expire within one hour, and logout revokes them. Refreshing the
page requires reconnecting; no credential is stored in browser storage.

The page displays the common wallet, global quotas, projects and memberships.
Human-only `GET/POST /v1/fleet/members` lists members/grants and supports `attach`,
`grant` (viewer/operator/none), `issue-token` (one-hour agent access) and `detach`, with `agent_did` and a unique
`request_id`; grant also needs `project_id` and `role`. An agent cannot administer
its own membership. Another customer's agent cannot be taken over. Owners of
projects cannot be detached. Detachment revokes control credentials and blocks
automatic reconnection until explicit reattachment by the same account owner.
Previously signed project capabilities expire within five minutes.

`POST /v1/cloud/projects/{id}/provision` on a target node accepts only a valid
operator-signed capability for that exact node/project. The operator must first
bind the project and owner at the authority. Provisioning is idempotent and
persists the project before responding; it does not copy any owner private key.
Local policy and MicroVM approvals still apply. This enables an existing owner's
project on a second node without registering another identity or sending email.
Worker billing opt-in remains an explicit operator configuration operation.
For secondary gateways, set `GAP_FLEET_ACCOUNT_ORIGIN` to the primary operator's
isolated management origin so `/account` opens the shared account console.

## Central retention after confirmed exhaustion

The central authority tracks a 72-hour deadline only when the customer's
unreserved balance AND all outstanding node allowances are zero. An unavailable
node's allowance is never inferred to be consumed or refunded. A top-up resets
the deadline before a deletion claim. A worker's cached deadline is informational;
it must obtain a fresh `retention-claim` at the destructive boundary. An outage
cannot authorize deletion. Claimed deletion fences new execution for that project,
including after a top-up, until the worker has removed all generations and retained
data and acknowledged `retention-finish`. Other projects may use the new funds.
Claims and local deletion progress survive restarts and lost acknowledgements.
Destroy operations also reconcile global capacity before completing retention.


The optional `console_paths` mapping names trusted same-origin management paths,
for example `{"node-01":"","node-02":"/nodes/node-02"}`. Paths must be unique,
empty for the local node or exactly `/nodes/<registered-node-id>`. Missing mappings
are reported as unavailable; the client never constructs a URL from a tenant field.
The account console loads all authorized project pages and aggregates VM inventory
with at most four concurrent node requests. A failed node is shown as incomplete
inventory. Management embeds the existing dashboard and renews its five-minute
capability every two minutes while the account credential remains valid. Renewal
preserves the CSRF value and does not revive a revoked browser session.

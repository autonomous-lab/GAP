# GAP customer platform, administration and node federation

Status: implementation in progress. The full platform is not complete.
This document extends the existing microVM console and records the requested
product behavior, dependencies, migration boundaries and acceptance criteria.

## Implementation progress

Implemented, tested and deployed to both node web servers in the first delivery
(the execution worker is now installed on both nodes; node-02 customer-path
validation remains a separate acceptance gate):

- Human microVM creation, selection and deletion at `/microvms`; multiple VMs
  within a project, independent routes/ports/disks and shared project credits.
  Deletion requires the full VM ID and supports retaining or erasing storage.
- Minimum creation defaults, optional public SSH
  key, stopped/start choice, price estimate, asynchronous job polling and safe
  retry identifiers after uncertain transport failures.
- `max_vms` approval quota, default one, adjustable live with the operator CLI.
  It currently covers an agent's projects on one node and includes stopped and
  hibernated VMs. Since b041620, multiple VM IDs can share one project.
- Email verification API and `/signup`, with first-project creation. Codes are
  HMAC protected in persistent SQLite, expire in ten minutes, have five attempts,
  cannot be replayed, and are subject to persistent sending limits.
- Billing accumulates within VM state/allocation/mode/tariff periods while
  five-second metering and prepaid enforcement continue. Historical raw samples
  remain archived; migration preserves account balances and fractional carry.
- Explicit per-node sales tariffs can be previewed and applied from `.env`
  through `microvm-billing.py`, with exact microcredit conversion and atomic
  expected-version checks. Restarts do not override live pricing. Provider
  costs now have separate immutable versions and explicit unknown values;
  public tariff discovery remains to implement.
- Existing identities continue to work and are not falsely marked verified.
  The email registry is node-local; it does not create a shared operator account.
- Elestio Postfix configuration discovered from the actual provisioning scripts.
  SMTP transport and registration were tested with an isolated mail sink; local
  relay connectivity and envelope acceptance were checked from both node network
  namespaces. Joseph has subsequently confirmed recipient-inbox delivery.

Individual administrator authentication is now deployed on node 01 at its
dedicated Elestio HTTPS origin: passwords, isolated email codes, revocable
sessions, origin isolation, CSRF and audit entries. The initial console provides
node-local agents/projects/VM inventory and project resource metadata. Owners
can submit access/quota requests; administrator approval changes node-local
quotas without restarting. Shared customer and fleet scope remain to implement.

Node 01 also exposes usage finance by complete UTC hour or rolling 30-day window.
Hourly projections conserve every debit and preserve original ledger tables.
Historical interval timing is marked as estimated. Provider costs are versioned
from the time of configuration; unknown costs suppress full usage margin.
Credit funding annotations distinguish paid, promotional and unclassified credit
additions without modifying wallet balances. Recorded cash is an operator
assertion, not Stripe verification. The GAP project grant is promotional.
Customer/fleet aggregation and complete provider cost coverage are still pending.

Node-local agent suspension now persists reason, administrator and generation
history; reactivation rejects stale decisions. Cloud management, static/function
routes and capabilities deny suspended owners. Realtime and sandbox workers
recheck short leases and terminate active work. The VM watchdog preempts guest
CPUs and forwarding independently of long jobs, then hibernates to disk. An
isolated KVM test verifies open TCP/WS closure under the job lock, hibernation
and state-preserving resume. This does not yet prevent new-account evasion or
provide a customer-wide retention hold; existing zero-credit rules still apply.

Still to implement: customer/operator account authority and shared wallets;
customer-wide and fleet-wide quotas; complete fleet dashboard and
customer-wide approval application; customer-wide suspension and retention holds; customer/fleet finance
aggregation; public tariff discovery; federation explorer; verified
operator badges; encrypted volumes, snapshots and backups with external keys.
Stripe remains a later phase. Do not present the local email registry or
per-agent count limit as the completed account/federation architecture.

The first account-authority foundation is implemented in
[`runtime/control`](../runtime/control/README.md): one operator-bound transactional
SQLite registry, customer/principal/project bindings, explicit agent grants,
short-lived hashed control credentials, integer shared-wallet operations and
non-spendable legacy import staging. Private node credentials are distinct and
restricted to that node's projects. Existing node authentication and worker
metering have not been switched over. Online debits are disabled by default.
Legacy exports are read-only and do not fence or transfer live balances. Remaining
gates include verified human linkage, node-scoped credential integration, final
source fencing and exactly-once balance transfer, and global capacity admission. This
foundation must not be reported as completed fleet accounts or quotas.

Worker spending reservations and execution leases are now implemented for explicit
project opt-in. Cumulative checkpoints settle once, reserve funds atomically and
cannot extend a cached lease on replay. Independent metering and a watchdog cover
long deployment locks; controller loss preserves disks instead of applying the
local credit-depletion purge. The HTTPS relay exposes only node-scoped control
operations. Financially used legacy projects cannot opt in without migration.
Customer-wide count/resource admission and authoritative retention decisions are
still pending; the two existing wallets have not been transferred.

## Confirmed requirements

- Humans can create microVMs from the web console.
- A customer may own multiple microVMs. The default total is one, and an
  administrator can approve a larger limit without restarting the stack.
- Authenticated administration covers customers/agents, projects, functions,
  static sites, databases, microVMs, approval requests and financial reporting.
- New agent registrations require an email address and verification code.
- Administrators can suspend abusive customers and their workloads.
- Stripe funding is a later phase, for human and agent-initiated purchases.
- Public pricing and documentation reflect the selected node's actual rates.
- Each node can initialize its own tariffs and costs through environment settings.
- Clients can explore participating nodes, location, hardware, prices,
  availability and measured latency.
- Both current nodes are hosted by Netcup in Germany, with 8 vCPUs / 16 GB RAM.
  Node inventory and SSH access: [nodes.md](nodes.md).

## Operator-scoped account decision and proposed federation architecture

Joseph selected one customer account and wallet per operator, shared across that
operator's fleet. Elestio operates its own trusted fleet; external operators have
independent accounts, credentials and balances. Federation provides discovery,
not an automatic shared wallet or automatic cross-operator workload migration.
Recommended initial architecture: one logical control service per operator for customer identity, email verification,
agent membership, global quotas, approvals, wallet transactions and placement.
Execution nodes retain their own resources, workload data, metering and tariffs.
The existing two nodes remain separate execution locations under the same operator.
This is a proposal, not an already deployed federation.

Clients sign in once through the common control service. It issues short-lived
signed credentials restricted to a customer/project, destination node and allowed
operations. Nodes verify signatures locally using the authority's public keys;
private signing keys and global administrator secrets are not copied to workers.
Node-to-controller management channels authenticate both endpoints. Public app
traffic follows the selected node's ingress route, without querying all nodes or
performing a central identity/database lookup for every application request.

Creation uses a transactional global quota reservation and node capacity admission.
The controller records the selected node and VM ownership before dispatch, using
idempotent operation IDs and expiring creation reservations. Reconciliation handles
crashes and failed provisioning without leaking slots or granting two final slots.
One creation reservation becoming stale does not authorize a second live owner:
worker fencing and durable generation checks must resolve unknown execution first.

Prepaid spending uses bounded reservations from the authoritative wallet. A node
receives a small spending allowance and execution lease; the same reserved credits
are unavailable to other nodes. Durable, numbered usage events settle the reserved
amount exactly once. Free balance, reserved balance and settled consumption are
separate. Do not reclaim reservations on timeout until unreported consumption is
reconciled or conservatively accounted for. An execution lease limits disconnected
operation, while metering reservations bound financial exposure. Retained storage
costs during disconnection need their own accounting policy; stopping CPU does not
eliminate disk costs.

Nodes report health, capacity, versioned tariffs and usage periodically. The shared
console reads a timestamped aggregate inventory. Detailed resource inspection and
lifecycle operations target the authoritative placement node. Existing workload
data remains at that node until an explicit migration transfers ownership/storage.

A control-service outage blocks new global reservations. Existing workloads may
continue only within their spending allowance and execution lease; expiry fences
execution/public access without treating the outage as confirmed zero credit.
An unreachable controller must not trigger the 72-hour credit-depletion purge.
Suspension is pushed immediately to connected nodes and bounded by lease expiry
on disconnected nodes; immediate revocation across a partition is not promised.

Initially the control service can run alongside node 01, as a separately managed
service with its own transactional state and backups. That is a control-plane
single point of failure. A resilient deployment needs database failover with a
single authoritative writer and leader fencing; simply configuring both nodes as
independent writable masters is unsafe. High availability is a separate phase.

For independent future operators, catalog federation alone is insufficient:
identity trust, admission policy, metering trust and financial settlement agreements
must be defined before allowing them to spend a shared customer wallet.

Mail transport is resolved: use the existing Elestio Postfix relay on each host
at `172.17.0.1:25`, with the sender from `/opt/elestio/startPostfix.sh`.
See `nodes.md` for verified node addresses and integration test boundaries.
Preserve existing identity secrets and balances during
the migration; do not clone the current local wallet onto every node.

## Customer and resource model

Introduce a customer as the quota and billing owner. A customer contains human
members and/or agents; an agent keeps its DID and scoped API credentials.
A project belongs to one customer and contains functions, site deployments,
databases and zero or more microVMs. Every VM has its own immutable VM ID,
project ID, customer ID and placement node ID.

The catalog now supports multiple VMs per project and resource quotas per agent.
Collection GET/POST `/vms`, selected reads via `?vm_id=`, and WebUI selection,
creation and deletion are implemented. The default VM retains its legacy `/vm`
and `/stack` aliases; additional ingress paths use the VM ID. Project wallets
and budgets remain shared, with independent VM billing periods.
Multiple projects must not become a way to bypass the new customer VM limit.
New count limits apply across all the customer's agents and projects. Count
stopped and hibernated VMs as well as running VMs; release the count only after
successful destruction. Resource allocation quotas remain separate from count
limits and are configurable by the administrator.

Future resource-addressed aliases can complement the existing collection API, for example:

- `GET/POST /v1/cloud/projects/{project}/vms`
- `GET/PATCH/DELETE /v1/cloud/projects/{project}/vms/{vm}`
- VM-scoped runtime, ingress, ports, SSH, jobs and usage subresources.

Preserve existing `/vm` integrations through a stable default-VM alias rather
than selecting an arbitrary machine from a collection. Each VM needs an
independent route, five-port allocation, job history, snapshot and usage stream.
Keep existing published URLs stable during migration; an additional VM receives
its own unambiguous route. Default-VM selection must be explicit and durable.

Creation must reserve count, resources and placement atomically. Concurrent
requests on two nodes must not both claim the final slot. Releases, retries,
worker crashes and partial creation need idempotent reconciliation. During a
coordinator outage, refuse unreserved creation and preserve existing workloads.

## Human microVM console

Replace the API-only empty-state link with a Create microVM workflow:

1. Choose project and node, displaying availability and current quota.
2. Choose name, vCPUs, RAM and disk. Recommend the smallest useful allocation.
3. Choose serverless/always-on, with approval requirements visible.
4. Optionally supply a public SSH key and configure HTTPS service port.
5. Show CPU/RAM cost per ON hour, persistent disk pricing, and network rates.
6. Submit one idempotent creation request and show job progress and errors.

Do not label a running QEMU process as a ready application. Distinguish creating,
booting, running, hibernated, stopped, unavailable and failed states. Make resource
limitations and disabled actions understandable. Preserve user inputs during
polling and never persist owner bearer tokens in browser storage.

The workspace lists all authorized VMs with project/node filters. Detail pages
cover resources, runtime mode, endpoints, SSH instructions, usage and budgets.
A pending approval is a first-class state with a request form, not a generic 403.
An approved higher VM count does not automatically grant more CPU/RAM.

## Administration

Build a new Cloud administrator console; the archived contract/escrow console
is not the correct product or authorization surface.

Use individual administrator identities and short-lived server-side sessions,
with Secure/HttpOnly cookies, CSRF protection for mutations, bounded login
attempts, expiry and explicit logout. Require a second factor for administrator
login. Email ownership verification by itself is not multifactor authentication.
Do not use one shared infrastructure secret as a permanent human login account.
API administration remains separately scoped, authenticated and auditable.

Navigation:

- Overview: customers, agents, projects, active/hibernated VMs, alerts and nodes.
- Customers and agents: ownership, verified email, status, quotas and audit trail.
- Projects: owner, node placement and resource summaries.
- Project detail: functions/versions/invocations, site releases/domains,
  database schema and bounded read-only inspection, VM collection and usage.
- MicroVMs: filters by customer/project/node/state; drill down and authorized actions.
- Approvals: new activation, additional VM slots, CPU/RAM increases and always-on.
- Finance: period and node/customer filters, costs, usage, funding and margins.
- Nodes: health, placement, advertised rates, configured costs and capacity.
- Audit: actor, action, target, timestamp, reason and outcome.

Paginate inventory and bound all detail queries. Never return identity seeds,
API bearer values, signing keys, database credentials or environment secrets.
Display customer source and content as inert text; preview sites in an isolated
sandbox/origin, not in the privileged administrator document.

Approval records need requested limits, reason, status, requester, reviewer and
review timestamp. Approval writes the enforced quota/permission atomically;
rejecting or cancelling must not leave a partial grant. Agents can request and
poll the same workflow through the API.

## Registration and suspension

Email verification flow: request challenge, send code, verify challenge, then
activate the account and issue credentials. Proposed defaults: a six-digit code,
ten-minute expiry, five verification attempts per challenge, resend cooldown,
per-address and per-IP limits. Store a keyed digest of the code; do not log it.
Resending invalidates the prior code, and successful verification consumes it.
Responses must avoid exposing whether an address belongs to an existing account.
Email delivery failures must not activate an unverified account.

Define a migration for existing DIDs and projects without email. Preserve their
ownership and balances, allow linking and verification, and explicitly decide
when unverified legacy accounts lose access. Do not break all existing agents
on the first deployment of the new registration flow.

Suspension is separate from credit depletion. It must fence management access
and new execution, public HTTPS/function/static routes, scheduled functions,
realtime token/session use and microVM TCP/UDP/SSH access as appropriate to the
suspension scope. Revoking an agent bearer alone does not stop public workloads.
Already-open connections require an explicit termination path. Record the reason,
reviewer and scope; prevent the suspended customer from bypassing restrictions
through another agent or project.

Preserve suspended data for review under a separate retention policy. Do not
silently apply the 72-hour zero-credit deletion timer to abuse suspension.
Restoration rechecks credit, quota and security status before enabling workloads.

## Finance: cash, credits, consumption and cost

Maintain distinct quantities:

- Cash collected from payments, refunds and chargebacks.
- Paid credit liability and promotional/operator-granted credits.
- Metered consumption, including consumption funded by promotional credits.
- Resource operating costs and fixed node costs.
- Estimated contribution margin, distinct from accounting net profit.

The existing 100-credit grant to the GAP project is not USD 100 of collected cash.
Do not make a top-up row automatically count as sales revenue. Future payment
records require a payment reference, funding type and idempotency key.

Show hourly buckets and calendar-month totals with an explicit timezone. The
730-hour disk pricing convention does not mean every calendar month has 730 hours.
For partial periods, prorate fixed costs over the applicable billing interval.
Separate allocated VM costs from idle/shared platform costs; do not allocate
100% of the same host invoice to CPU and another 100% to RAM or storage.

Known node-01 cost: USD 55/month for the bundled machine. Additional storage:
USD 0.05/GB-month. Actual provider bandwidth costs and node-02 invoice must be
configured or shown as unknown; a customer sales price is not our network cost.
Mark incomplete cost estimates visibly and do not present them as exact profit.
Historical reports must retain the tariff and cost version effective at usage time.

## Node-specific pricing and environment configuration

Keep rates versioned and immutable. Suggested environment bootstrap fields:

- `GAP_PRICING_VERSION`
- `GAP_PRICE_VCPU_HOUR_USD`, `GAP_PRICE_RAM_GIB_HOUR_USD`
- `GAP_PRICE_DISK_GB_MONTH_USD`
- `GAP_PRICE_NETWORK_IN_GB_USD`, `GAP_PRICE_NETWORK_OUT_GB_USD`
- `GAP_COST_NODE_MONTH_USD`, `GAP_COST_EXTRA_DISK_GB_MONTH_USD`
- `GAP_COST_NETWORK_IN_GB_USD`, `GAP_COST_NETWORK_OUT_GB_USD`

Parse decimal amounts exactly. Preserve the deployed distinction between RAM
GiB, decimal disk/network GB, a 730-hour disk-month and one million microcredits
per USD credit. Flush current usage before changing tariffs. Reject an attempt
to reuse a tariff version with different values. Missing/invalid production
prices must not silently create free usage or reactivate shadow mode.

Environment settings bootstrap a fresh node. Existing live operator settings
remain authoritative unless an explicit versioned update is requested; a restart
must not accidentally restore an old rate. Admin/CLI updates remain available
without restarting the stack. Node-specific `.env` files never enter git and
must not be copied wholesale by the shared CI pipeline.

Public pricing, the creation estimate, the administrator console and API docs
must read the same published tariff descriptor. Remove hardcoded claims of one
global price once different node rates are supported. Persist placement/tariff
references so moving a VM cannot silently change the billed node or price.

## Client-facing node catalog

Start with the two known operator-owned nodes. Publish node identity, operator,
country/region, advertised hardware, currently allocatable resources, supported
services, current prices and relevant status. Mark advertised versus measured
values and include freshness timestamps. Both current nodes: Netcup, Germany,
8 vCPUs / 16 GB RAM. Do not advertise node-02 microVM capacity before installing
and validating its execution worker.

Uptime requires an observation history and a specified measurement window;
do not invent percentages before enough samples exist. Distinguish node process
uptime from service availability. Latency should identify its measurement origin:
browser-to-node latency is not controller-to-node latency. Bound probe frequency
and use an explicit CORS-safe public probe endpoint without customer credentials.

Fetch descriptors only from administratively trusted endpoints with bounded
responses/timeouts and SSRF protections. A catalog entry is not authority to
receive customer API tokens. Federation needs signed node identity and scoped
cross-node requests; never forward a node-01 owner bearer to arbitrary catalog URLs.

Browsing nodes is not clustering. Placement, migration, disk/snapshot movement,
recovery and rebalancing require a coordinator, admission reservations, fencing
and durable ownership transfer. A wallet must have one authoritative writer or
an equivalent transactional spending reservation protocol, never divergent copies.

## Delivery order and acceptance gates

1. Implement the agreed common account/wallet authority and resolve mail delivery; define customer ownership,
   migration and administrative authentication. No production auth cutover yet.
2. Add customer VM count enforcement, VM collection APIs and migration aliases;
   verify two agents/projects cannot bypass the default-one limit concurrently.
3. Deliver human creation/list/detail workflows and approval requests, with real
   KVM creation, quota failure, retry and deletion tests.
4. Deliver administrator inventory, drill-down, approvals, suspensions and audit;
   verify anonymous/customer access is rejected and suspension fences workloads.
5. Deliver verified-email registration and the agreed legacy migration; exercise
   code expiry/replay/rate limits and real email delivery through the chosen sender.
6. Deliver per-node configuration, public tariffs/catalog and financial reporting;
   reconcile ledger totals and identify missing costs instead of inventing profit.
7. Introduce federation placement/rebalancing after shared ownership and fencing
   are established. Test node loss, concurrent placement and interrupted migration.
8. Later: Stripe funding through hosted payment flows and verified, idempotent
   payment events. An agent may initiate a funding request; automated spending
   requires explicit owner authorization. Never accept raw card data in GAP chat/API.

Each deploy is tested on the target host before push. The same repository updates
both nodes: observe both pipelines and avoid overlapping manual stack operations
with CI. Keep deployment status distinct from source/plan completion.

## Operator assurance badges and encryption at rest

Joseph requests operator assurance badges and AES-256 encryption at rest.
Elestio's public security page lists SOC 2 Type 2 and ISO 27001:
https://elest.io/security-and-compliance

Attribute those assurance references to Elestio and link to evidence/report
access. SOC 2 Type II is an attestation report, rather than an ISO-style product
certification. The applicable entity, scope and validity must accompany verified
claims. Running GAP does not confer Elestio's assurance on an independent operator.
Distinguish operator-verified identity, declared controls and checked evidence.
Runtime security badges require checked configuration and must not be self-awarded
through an arbitrary node environment variable.

Recommended storage design: AES-256-XTS encrypted host storage volumes (LUKS2)
covering customer VM disks, hibernation memory snapshots, project databases,
objects/sites, retained data, temporary artifacts and any persistent swap.
Encrypt off-host backups independently. Per-VM/project volumes or data encryption
keys allow finer separation, revocation and controlled migration. Select a tested
storage layout instead of assuming encrypting a VM payload covers all metadata
and memory snapshots. QEMU also supports LUKS-encrypted qcow2 payloads, but its
legacy `encrypt.format=aes` is AES-128-CBC and must not be used as AES-256 support.
Reference: https://www.qemu.org/docs/master/system/images.html

Keep master wrapping keys in an operator-controlled key service separate from
the data volume; scoped data keys are released only to authorized workloads/nodes.
Use standard authenticated key wrapping/envelope encryption, with rotation,
recovery and audit. Disk access keys should not be committed, passed in process
arguments or stored unprotected next to encrypted data. Cold boot, host restart,
hibernation/resume and cross-node migration require explicit key-release paths.
Key service outages must fail closed for new unlocks without deleting data.

Encryption at rest protects offline stolen media and copies whose decryption
keys are unavailable. An authorized live host can still access decrypted workload
memory and data. It does not make an untrusted operator safe; customer-side
end-to-end encryption or separately evaluated confidential-computing protection
addresses a different threat model.

The existing GAP seed vault uses XChaCha20-Poly1305; it is not AES-256 and does
not establish coverage of all stored customer data. Current microVM image creation
does not request QEMU image encryption. Underlying host/provider encryption has
not been verified here. Do not display an active AES-256-at-rest badge until actual
coverage, backups, temp/swap paths, key handling and recovery have been validated.
Benchmark cold boot and hibernation resume with encryption; do not promise zero
latency overhead or compatibility of all existing snapshots before testing.

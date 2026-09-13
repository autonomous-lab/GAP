# Cold migration experiment

This is a disposable two-host experiment, not a supported fleet migration API.
The production project placement, routes and billing remain unchanged.

## Verified

A 1-vCPU, 1024-MiB, 8-GiB guest wrote a witness file on node01. The controller
persisted a migration fence before graceful shutdown. Restart was refused even
with a fresh controller. The stopped qcow2 disk was flattened with qemu-img
convert so it no longer depended on the source rootfs backing image.

The private bundle was streamed over SSH to node02. Its SHA256 matched. The
guest booted on node02 and its witness file matched. A subsequent fresh source
controller again refused restart. Both disposable environments and their keys
were removed. No production worker was replaced during this experiment.

## Reproducing safely

Use migration_smoke.py only in isolated runner containers with KVM, no production
configuration, and a unique disposable directory mounted at /trial. Mount the
host guest images read-only at /images and the tested microvm.py in the runner.
Set GAP_MIGRATION_SMOKE=1 explicitly. Do not mount production worker state.

1. Run the source stage. It generates a disposable identity and private bundle.
2. Require successful source completion before any target import.
3. Transfer only /trial/bundle over authenticated SSH to the target trial.
   Preserve restrictive permissions and runner ownership. This contains private
   guest keys; do not publish it or store it in CI artifacts.
4. Run target exactly once. It checks the disk digest, boots, checks the witness,
   then stops the guest. verify-target can recheck an already imported guest.
5. Run verify-source with a fresh source container after target success.
6. Confirm both test containers stopped, then remove the exact trial directories.

The experiment uses a cold boot, not a memory snapshot. The observed hosts had
identical kernels and CPU capabilities but different guest rootfs manifests.
Flattening resolved the backing-file dependency for this specific experiment;
it does not establish compatibility for arbitrary kernels or guest images.

## Remaining product work

- Durable authority-owned migration records and per-VM host placement.
- Owner authorization, destination capacity and tariff validation.
- Authenticated transfer, integrity checks and retry-safe import.
- Atomic placement handoff for capabilities, ingress, terminal and billing.
- Recovery that never permits both copies to execute. A source fence must not
  be cleared merely because a transfer request timed out.
- Dashboard action with explicit downtime and completion/failure state.

The internal fence primitive is intentionally not exposed as a public endpoint.
A shutdown failure leaves the fence in place and does not authorize target start.

## Authority preparation journal

`runtime/control/migration_journal.py` persists preparation through
`prepared -> source_fenced -> target_staged`. The private Application transport
has opt-in operator preparation/status and node-scoped attestation actions.
This switch is deliberately not wired to the production configuration loader:
preparation is not a complete migration service and must not be enabled there.

Each migration pins the customer, project, VM, source, destination and capacity
revision. Concurrent preparations for one VM are rejected. Attestations require
the correct worker, current revision and matching disk digest. Lost-response
retries return their historical result; status must be read for current state.
No preparation phase grants destination execution or changes placement/balances.
Only the explicit commit phase changes placement; it does not itself move funds.
Authority capacity changes are rejected while a preparation exists. The local
source fence also refuses resize and destroy before worker side effects.

The journal has no timeout-based unlock. Its authority handoff now continues
through source_settled -> routing_ready -> committed. The source settlement
must reference an existing central checkpoint for the correct node, project
and owner with no unpaid usage. The worker remains responsible for including
the final VM sample and durably stopping source metering before attestation.
The routing receipt is an operator assertion that dormant routes are installed.
Commit atomically changes the capacity node and grants that node project VM
accounting access. It preserves the project home, customer wallet, quota count
and historical reservations. Active retention claims block commit.
A commit response is not execution permission: target billing/policy leases
must still be obtained, and old responses never authorize a restart.
Cancellation now proceeds through cancelling -> target_discarded -> cancelled,
with target attestation required before source restoration. Committed moves
cannot be cancelled; moving back requires a new migration. Both attestations
are revision-bound, retry-safe trusted-worker receipts. These transport actions
remain disabled in production until the worker orchestration is connected.
A cancelled record remains available for audit and permits a new migration ID;
a delayed request for the cancelled migration cannot advance the new one.
The remaining worker orchestration must reconcile lifecycle/retention actions,
settle source metering, prepare destination accounting and install routes before
requesting the central handoff. None of these host actions is performed merely
by recording a journal receipt. Central preparation alone does not
stop a guest or revoke cached worker permissions. Host evidence IDs are trusted
worker assertions, not independent proof. Do not expose this preparation API to
customers or start production migrations with it.

The private control endpoint `GET /v1/vm-placements` lists the current capacity
placement per VM and any preparation target. It filters by customer and project
grant, paginates by VM ID, and never presents the preparation target as the
current host. The public fleet relay now maps `/v1/fleet/vm-placements` to this
endpoint. Account uses the placement to query and open the actual host, while
preserving project grants. Project tokens accept an explicit `node_id` only for
the project home or a host admitted by a committed migration.

## Integrated worker implementation (not activated in production)

`vm_transfer.py` implements the private storage and lifecycle operations. Enable
`migration_transfers` only after the complete coordinator, origin policy and
HTTPS routing integration are available. The public-facing node transport is
`POST /v1/fleet/migration-worker`, requiring a distinct, operator-provided
`GAP_FLEET_MIGRATION_TOKEN`; tenant and project tokens cannot use it. It only
forwards the fixed `admin/migration` worker action. Every request includes
`migration_id` and `operation`, and checks the current authority binding.

- `export` stops and fences the source, flattens the disk and records its digest.
- `read` / `write` exchange bounded 512-KiB chunks. `upload-status` resumes from
  the target's durable byte count. A replay must contain identical bytes.
- `import` validates the complete archive, disk format, digest, kernel and owner
  before attesting target staging. It creates no live catalog entry.
- `settle` takes the final source sample, durably disables source metering and
  references the acknowledged central checkpoint.
- `activate` requires committed placement and fresh target accounting, capacity
  and policy admission. It preserves guest keys and the configured original
  application origin, allocates new host ports, and installs the disk by rename.
- `discard` deletes staged target data before cancellation acknowledgement.
  `restore` requires that acknowledgement before unfencing the source. Neither
  operation rolls back a committed move.

Export, import, settlement, activation, discard and restoration run asynchronously;
poll `status`. A failed operation may be retried with the same migration ID;
historical success is never a fresh execution lease. An interrupted installation
after disk rename can resume before the catalog has been saved. Transfers check
temporary disk space, reject archive links/traversal and retain private keys only
in private worker storage. Do not attach bundles to public logs or tickets.

An activated target persists the original project policy host. Its worker uses
the configured `migration_peers` HTTPS origins and private token files to fetch
current approvals and suspension policy. Missing or revoked origin policy denies
execution instead of falling back to destination approval. The private `policy`
operation resolves project and owner from the journal and only serves them on
the original project home. This does not yet proxy node-level management routes.

Project-local budgets currently reject export because their enforcement has not
been centralized across hosts. The worker does not make an HTTPS proxy route
merely by preserving `ingress_origin`. Automatic orchestration, node-level access,
stable HTTPS forwarding, source cleanup and the dashboard migration action still
need integration and end-to-end validation before production activation.

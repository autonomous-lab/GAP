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
current host. It is not yet exposed by the public fleet relay or consumed by
the dashboard. This inventory is the next integration point for per-VM access.

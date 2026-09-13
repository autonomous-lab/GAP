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

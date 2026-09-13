# Control authority backups

Operational backup delivery, not automatic failover or zero data loss.

## Installed layout

- Node01: `/opt/gap-backup/control-backup.py`, `send.py`, public recovery key,
  dedicated SSH transfer key. Hourly `control-backup.timer` starts
  `control-backup.service` (five-minute timeout).
- Source: `/opt/app/gap-node-01/data/gap-control/state/authority.sqlite` and files
  referenced by `data/gap-control/config/control.json`.
- Node02 destination: `/var/lib/gap-backup/archives/`, account `gapbackup`.
  Its authorized key permits only the root-owned receiver: no arbitrary command,
  interactive session or forwarding. The destination host key is pinned.
- Source spool: `/var/lib/gap-control-backup/`; `latest.json` records successful
  transfer time, remote filename, size and SHA-256.
- Recovery key: `/opt/app/data/.ssh/gap-control-recovery.pem`, mode 0600, in the
  employee workspace, not either execution node. Public counterpart:
  `/opt/app/data/.ssh/gap-control-recovery.pub.pem`. Never attach or commit the
  private key. This is a separate online location, not an offline vault.

SQLite's online backup API captures committed WAL transactions consistently.
All current tables and referenced configuration files are included; old unreferenced
configs are excluded. Configuration changes during capture fail the operation.
The snapshot and manifest are encrypted with a fresh AES-256-GCM key, wrapped
using RSA-3072 OAEP SHA-256. No plaintext snapshot is written on the source.
The receiver fsyncs and returns a checksum before success is recorded.
Encryption and checksums cannot establish correctness of an already compromised
source database.

Source copies older than seven days are pruned after successful transfer;
destination copies older than thirty days after receipt of a new archive.
Both preserve at least the newest two. Failed transfers retain the source copy.
The transfer key has no remote deletion command, but can submit additional bounded
archives: storage and freshness monitoring remain necessary.

## Check and verify recovery

On node01:

```sh
systemctl status control-backup.timer
systemctl show control-backup.service -p Result -p ExecMainStatus
cat /var/lib/gap-control-backup/latest.json
journalctl -u control-backup.service --no-pager -n 20
```

Check successful completion age and the remote file, not just the active timer.
One hour is the intended interval, not a guaranteed RPO. Failures extend the
recovery gap. No email alert is enabled. Archive and uncompressed snapshot limits
are 128 MiB; exceeding them fails instead of truncating.

Retrieve the encrypted archive from node02 with existing operator SSH access.
Run the following in the workspace:

```sh
mkdir -p temp/control-backup-check
chmod 700 temp/control-backup-check
python3 GAP/scripts/control-backup.py verify \
  --archive temp/control-backup-check/archive.gapenc \
  --private-key .ssh/gap-control-recovery.pem \
  --scratch-dir temp/control-backup-check
```

The verifier checks decrypted file hashes, SQLite integrity, foreign keys,
nonnegative wallet counters, operator identity and all table counts. It never
activates an authority or replaces a live file. A private temporary directory is
removed on normal exit and exceptions. Default scratch `/dev/shm` is too small
in the workspace for the current database; the validated command above uses a
private disk directory. Protect that filesystem: unlinking is not cryptographic
erasure, and a hard kill can leave plaintext scratch files.

The archive does not contain VM disks, guest memory, worker databases or main-node
`.env` files. Recovery of the entire fleet requires those separately. Before
production restoration, fence the old authority and consumers, reconcile newer
worker checkpoints and spent credits, choose a recovery epoch, restore keys
privately and validate before allowing writes. The verifier is deliberately not
a production activation command.

## Validation and limits

A real encrypted snapshot was copied off host and restored in isolation:
24 tables and 12 configuration files passed; modified ciphertext was rejected.
The installed systemd service succeeded and its timer is active. No live database
or customer VM was changed. Python 3.11+ with SQLite serialize and cryptography
is required; hosts use cryptography 46.0.5, workspace verifier 38.0.4.

Both execution hosts share a provider. This copy does not protect against a
provider-wide loss. Add owner-controlled escrow of the recovery key before
claiming complete disaster recovery. Replication, election, fencing and actual
failover tests remain on the existing resilience card. The operational scripts
are deployed separately from GAP, requiring no Rust rebuild.

## Agreed recovery scope: SQLite on the second host

The PostgreSQL/Patroni/etcd integration is paused. The practical recovery path is
an operator-controlled restore from an off-host backup, with interruption and a
possible gap since the snapshot. This is not automatic HA or zero-loss recovery.
The PostgreSQL prototype is retained only as experimental code.

### Prepare an inactive restore

Keep the private recovery key in the workspace; do not copy it to the destination.
Download the encrypted archive from node02 and compare its SHA-256 with the
source transfer receipt when available. Use a new private parent directory:

```sh
python3 GAP/scripts/control-backup.py stage \
  --archive temp/control-backup-check/archive.gapenc \
  --private-key .ssh/gap-control-recovery.pem \
  --scratch-dir temp/control-backup-check \
  --output temp/control-backup-check/restored
```

This verifies the archive before creating `state/authority.sqlite`, `config/`
and `recovery.json`. Directories are private and files mode 0600. Existing output
paths, missing referenced config files and unsafe archive paths are rejected.
The restored configuration keeps its original permissions and feature flags:
**staging is not a write-disabled service mode**. Do not attach it to the live
network before completing the recovery gates below.

Transfer this private directory over operator SSH to a new private directory
on node02. Test the restored service with no network access, no published ports,
`/config` mounted read-only and only the copied state mounted writable. The
recovery key remains in the workspace. This checks configuration, keys, schema
and startup without allowing workers to reach the restored authority.

### Gates before a real production takeover

1. Fence the old controller. If reachable, stop its service and disable automatic
   restart/redeployment. If unreachable, use provider power-off or equivalent
   enforced isolation; an SSH timeout is not evidence it has stopped. Record the
   action and keep it fenced when the host returns.
2. Block every consumer from both controller endpoints during reconciliation.
   Inventory node gateways, billing/capacity workers, operator/account access,
   identity gateways and migration clients. Existing execution leases must
   expire. Do not merely change DNS while old connections remain usable.
3. Preserve worker databases and any surviving controller WAL/database before
   modifying them. Record snapshot time and enumerate changes after it:
   cumulative reservation consumption/unpaid amounts and reservation IDs;
   funding/refunds and request IDs; VM placement and in-flight migration state;
   capacity changes; grants, revoked credentials and suspension decisions.
4. Reconcile against durable evidence. Replay only supported idempotent
   operations using their original identifiers and matching bodies. Never add
   a credit balance manually or replay the same payment with a new identifier.
   A worker checkpoint may refer to a reservation missing from the backup;
   that needs explicit reconciliation, not a new reservation to hide the gap.
   This release does not automate reconciliation or invent a recovery epoch.
   If evidence is missing, keep affected operations/accounts closed. If the
   affected scope cannot be established, keep the whole controller closed.
5. Review credentials restored from an older snapshot: later revocations may
   have been lost. Revoke uncertain sessions and rotate controller consumer
   credentials with the corresponding configurations before restoring access.
   Reconcile outstanding migrations before permitting their execution.
6. With the old authority still fenced, install the reconciled copy under the
   destination's control config/state paths, owned by service UID 10001. Back up
   any pre-existing destination state; never overwrite it in place. Update all
   trusted controller endpoints and gateway routing, retaining the intended TLS
   identity and private access rules. An HTTPS origin on the failed node is a
   separate routing dependency; restoring the database does not repair it.
7. Verify identity, integrity, wallet/reservation consistency and a controlled
   consumer before reopening operations. Retarget the backup source/service to
   node02 and copy encrypted archives to a healthy independent host. Once new
   writes are accepted, never roll back to the older snapshot: it would erase
   those writes. Preserve the previous data for investigation only.

No automated command bypasses these gates. Takeover needs an incident-specific
record of the fencing action, reconciliation evidence, changed endpoints and
reopening decision. There is no guaranteed recovery time or data-loss bound.

### Restore drill evidence

A real off-host archive was staged and the actual SQLite control service started
on node02 in a disposable container with `--network none` and no published ports.
The health request succeeded with access signing, reservations and capacity
configured. All 24 tables and 12 configuration files verified. The transfer
checksum matched; overwriting an existing stage and archive traversal were
rejected. The exercise did not call mutation endpoints or connect production
consumers. Its private restored copies were removed afterward.

This proves backup-to-service startup on the second host. It does not prove
reconciliation of a real incident, gateway takeover, or recovery of guest disks.

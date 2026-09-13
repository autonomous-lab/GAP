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

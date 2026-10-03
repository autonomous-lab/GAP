# MicroVM encryption in the worker

Opt-in setting inside `runner.json`'s `hypervisor` object:

```json
{"disk_keyring":"/config/disk-keys.json"}
```

The read-only keyring must be outside the hypervisor state directory, owned by the
worker and mode 0600. Example structure (generate real keys securely, never copy
this value):

```json
{"active":"v1","keys":{"v1":"<64 lowercase hex characters from 32 random bytes>"}}
```

Never commit a populated keyring. Keep an independent off-host recovery copy.
Trusted migration hosts must have the same operator key version installed before
moving a VM. Unrelated fleets must use different keys. Loss of a referenced key
means loss of the corresponding VM data. Do not remove old keys while VMs,
retained disks, migration archives or backups still reference them.

Each VM derives its own passphrase using HMAC-SHA256, binding the master key to
project, owner, VM identity and a random 256-bit salt. The passphrase is supplied
through an inherited memfd, not command-line text, a guest disk, or a transfer
archive. Disk metadata retains only the version and salt. New disks use qcow2 LUKS
with AES-256-XTS. Missing keys fail closed; enabling the option rejects legacy
unencrypted VM starts rather than silently downgrading.

## Seed and SSH identity

For encrypted VMs, `seed.ext4` and the guest SSH host private key are sealed
with AES-256-GCM keys derived separately from the VM disk key. Existing clear
seed files are migrated at worker startup, or on the next start/export. The worker rebuilds seed
images in tmpfs and passes the decrypted image to QEMU through an inherited
anonymous memfd; it does not create a plaintext seed image on persistent
storage. Cold-transfer archives contain only `seed.ext4.enc` and
`ssh_host_ed25519_key.enc` for encrypted VMs. Public keys, `authorized_keys`
and runtime routing metadata are not secret and remain readable by the worker.

## Hibernation

QEMU 10.1.5 aborts in `qcow2_co_encdec` when internal `savevm` writes unaligned VM
state into an encrypted qcow2. Encrypted VMs therefore use stopped QEMU migration
to a private local Unix socket, with memory encrypted as an authenticated AES-256-GCM
stream in `memory.enc`. Each stream has a fresh nonce prefix, ordered chunks,
length authentication and a mandatory authenticated final record. No plaintext
memory file is created. Restore holds the guest paused until the entire stream
has authenticated, admission is valid and the consumed-state transition is durable.
The current disk remains unchanged while hibernated. The unencrypted legacy path
continues to use internal snapshots.

The fleet fast-snapshot mode uses `GAP_FAST_SNAPSHOT_ENABLED=1` and pinned
QEMU 11.1.50; `GAP_FAST_SNAPSHOT_VM_IDS` remains available for single-VM tests.
It saves RAM as a
seekable `mapped-ram` file and resumes with `postcopy-ram`/`userfaultfd` so
the guest can run before every page is loaded. This `memory.fast` file is
**not encrypted or authenticated**. It is mode 0600 in the worker-only state
directory and is removed only after postcopy finishes. Never enable this mode
for a VM whose RAM may contain secrets without accepting that exposure.
The VM disk, seed and host SSH private key remain encrypted. A lost or modified
RAM checkpoint may prevent resume; there is no confidentiality or integrity
claim for this experimental checkpoint.

New qcow2 LUKS disks use `encrypt.iter-time=50` ms instead of QEMU's 2000 ms
default. GAP's per-VM passphrase is already an HMAC-derived 256-bit random
secret, not a human password; the shorter PBKDF improves startup without making
practical offline guessing of that secret feasible. Existing disks retain
their original KDF until they are converted offline. Conversion with
`reencode_disk_kdf.py` preserves the original disk as
`disk.kdf-original.qcow2` for rollback but flattens the backing image and can
increase physical storage. Test and plan capacity before converting a fleet.

Cold migration flattens directly to an encrypted standalone qcow2. It does not
copy memory checkpoints, and retains the established cold-move semantics. Destination
validation requires the key and rejects incompatible images before activation.
Resize also requires the key. Disk accounting already includes memory.enc.

## Rotation and recovery

To rotate the master used for *new* VMs, add a new independent version to every
trusted migration host and recovery copy, then change `active`. Existing VMs
continue to require their original version; this is not re-encryption of existing
VMs and does not revoke a compromised old key. Re-encrypting existing VMs and
formal key-service audit integration are separate work; do not claim them delivered.
Keyrings are read on each operation. Replace files atomically with preserved mode
and ownership. A running QEMU retains its key until it exits.

## Worker databases

The deployment also uses the worker disk keyring to encrypt every persistent
SQLite database under `/data` with SQLCipher. This includes the job catalog,
credit ledger, fleet-capacity catalog and existing SQLite recovery copies. A
separate HMAC-derived passphrase is bound to each database purpose. Existing
plaintext databases migrate atomically before the worker initializes its
hypervisor or accepts requests. A missing key, invalid keyring, wrong purpose or
damaged database stops startup.

Each encrypted database has a `<database>.key-id` sidecar containing only its
keyring version. Keep this mode-0600 file beside the database in backups and
restores. Changing `active` applies to databases created afterwards; existing
databases continue using their recorded version. Do not remove an old keyring
entry while any database or backup references it.

Create a live, transactionally consistent worker snapshot inside the state
volume with:

```bash
docker compose -f runtime/compose/deploy.yml --project-directory . exec \
  compose-runner python3 worker_backup.py --output /data/backups/NAME
```

The command writes SQLCipher databases, their `.key-id` files and a checksummed
manifest into a new mode-0700 directory. It refuses to overwrite a snapshot.
Copy the completed directory off-host together with an independently protected
copy of every referenced keyring version. A database without its sidecar or key
cannot be restored.

## Protection boundary

The standard mode protects VM disk payloads, retained hibernation memory, seed images and guest
SSH host private keys and worker SQLite contents against acquisition of those
files without the keyring. qcow2 metadata, public seed metadata, SQLite filenames
and sizes, serial logs (when enabled), host swap, host logs and a complete host
image including the keyring are NOT covered.
Fast-snapshot RAM files are plaintext and outside that protection boundary.
Root on a running host can obtain keys or guest memory. No host-wide encryption
badge is justified. Authenticated memory encryption does not add disk integrity to
AES-XTS. Transfer transport authentication and existing archive digests still apply.

## Verification

`python3 -m unittest test_sqlite_crypto test_disk_crypto test_memory_crypto test_seed_crypto test_microvm test_vm_transfer`
checks database migration, purpose binding, key-version retention, missing keys,
seed sealing and memory corruption/truncation.
Run `GAP_TEST_ENCRYPTION=1 python3 encryption_kvm_test.py` only in an isolated
container with /dev/kvm, the proper KVM group and read-only /images. It creates and
cleans its own VM/catalog. It checks real guest disk writes, memory-preserving
hibernate/resume, encrypted flattening, resize and boot after transfer. It does
not itself validate the authority's complete cross-node migration workflow.

References: https://www.qemu.org/docs/master/system/qemu-block-drivers.html
and https://www.qemu.org/docs/master/system/secrets.html

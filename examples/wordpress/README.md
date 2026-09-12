# WordPress on a GAP MicroVM

This example creates **a new VM** in an existing, operator-approved project with
sufficient credits and quota (1 vCPU, 1 GiB RAM, 8 GiB disk). It does not adopt,
replace or delete another VM. Requires Python 3 standard library only and a GAP
node/worker supporting `/vm/readiness` and explicit Compose `vm_id` selectors.

```sh
export GAP_TOKEN=your_owner_bearer
python3 examples/wordpress/deploy.py --project prj_YOUR_PROJECT \
  --email you@example.com --title 'My WordPress' \
  --state /absolute/private/path/wordpress-state.json
```

The scripts send `User-Agent: GAP-WordPress/1.0`: the hosted Cloudflare edge
can reject Python's default User-Agent with HTTP 403/1010 before GAP sees the
request. Set `GAP_NODE` or `--node` for another HTTPS node. Obtain identity, project,
MicroVM approval and credits first using [AGENTS.md](../../AGENTS.md).
The script creates a stopped VM, starts it, waits up to 180 seconds for guest
control/Docker, installs MariaDB and WordPress, saves visitor Basic Auth,
publishes guest port 8001 and verifies HTTP access. No public TCP slot is used.
Ingress stays disabled during installation, so the anonymous setup wizard is
never published. The installation uses WP-CLI `--skip-email`.

The final output shows the URL and absolute path of a mode-0600 checkpoint with
separate visitor and WordPress administrator credentials. **Keep it private**:
it contains passwords and base64 release files, although never the GAP bearer.
Do not commit, attach or paste it. The repository example contains no secrets.
Run only one deployment process per checkpoint at a time.

Rerun the same command with the **same checkpoint** after a lost response. Saved
request bodies and IDs recover the same jobs. Failed/interrupted mutations are
not blindly replayed: inspect the reported job through the owner API first.
The script preserves the VM on failure. If it has since hibernated/stopped,
resume/start it explicitly before retrying. This bootstrap checkpoint is not an
application update tool: later updates require a new complete release and new
request ID, targeting the saved `vm_id`.

To recheck the installation without redeploying:

```sh
python3 examples/wordpress/verify.py --state /absolute/private/path/wordpress-state.json
```

Checks: unauthenticated access challenges, authenticated homepage, one actual
JavaScript asset, `/wp-json/`, and administrator login with a session cookie.
Redirects are limited to the exact application origin and prefix; no credentials
are forwarded to another site. Responses and cookies are not printed.

## Why the configuration differs from root-domain WordPress

GAP removes `/apps/{project-or-vm}/` before proxying. `config.php` sets WordPress's
full public URL, restores that prefix in `REQUEST_URI` and recognizes the fixed
HTTPS deployment origin. Apache rewrite rules use `/` inside the guest. GAP
Basic Auth and WordPress login are separate. WordPress application passwords
in the Authorization header cannot pass through GAP Basic Auth; use a verified
custom domain for those clients. A custom domain also avoids shared-origin
browser storage and does not require GAP visitor authentication.

The installation container uses UID 33 to match the Debian Apache image's volume
ownership. The Dockerfiles explicitly make application directories traversable
and scripts/configuration readable: GAP release files are private by default,
so do not assume a non-root container can read a release bind mount. Secrets
are supplied at runtime, never copied into the images. Two named volumes retain the database and WordPress files across
releases. A healthy bootstrap service ensures Compose `--wait` includes completed
installation; it then only sleeps. Deleting the VM with data deletion deletes
these volumes. This example does not provide backups or disaster recovery.

The image tags match the reported working stack: `wordpress:php8.3-apache`,
`mariadb:11.4`, `wordpress:cli-php8.3`. Tags can change; pin tested image digests
for reproducible releases. One GiB is the tested starting allocation, not a
capacity guarantee for plugins or traffic. Measure before resizing.

References: [official WordPress image](https://github.com/docker-library/docs/blob/master/wordpress/README.md),
[WP-CLI installation](https://developer.wordpress.org/cli/commands/core/install/).

## Acceptance test

`runtime/compose/wordpress_integration.py` is an opt-in test for an isolated
container with Python, QEMU/KVM, OpenSSH client, e2fsprogs, OpenSSL, Caddy and the
candidate GAP binary. Mount **only** the guest image directory read-only at
`/images`, and make this repository available at `/repo` (or set `GAP_TEST_REPO`).
Run from `runtime/compose` with `GAP_TEST_WORDPRESS=1` and
`GAP_TEST_BINARY=/path/to/candidate/gap`. Allocate 4 GiB container RAM and 3 CPUs.
It creates a private temporary node, runner, Caddy and TLS frontend, two VM slots,
and its own credentials. It tests a cold boot without ingress, an additional VM
while the default stays stopped, query/body selector conflicts, the complete
WordPress script and replay of its checkpoint. VMs and state are removed in
`finally`. Never mount a production VM catalog or secret directory into it.
The TLS test frontend reproduces nginx's admission contract; GAP admission,
Caddy, QEMU, Docker, MariaDB and WordPress execute their real implementations.

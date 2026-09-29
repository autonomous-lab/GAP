# Anonymous free MicroVM — implementation contract

This is the implementation and rollout record. Nodes 1, 2 and 3 each serve
the public SSH pilot on TCP 2121; node 1 is the central admission authority.
Each node has its own dedicated free guest image and a configured active cap
of 16, a pilot value rather than a fleet-wide hard limit. The public entry page
is `/free-vm` on each node and displays that node's direct SSH hostname.

## User journey

The Railway-style entry point should lead with one copyable command,
`ssh -p 2121 free@<node-host>`, and state the limits next to it: 1 vCPU,
1 GiB RAM, one active hour, and 24 further hours to claim the stopped VM.
The first terminal screen should display the VM's exact remaining active time,
preview URL, Basic Auth username/password, and claim URL. Show the same
information in a small machine-readable JSON document inside the guest for
agents. The claim page must explain that the VM remains the same VM after
claiming; it must not silently copy data into a different project.
The SSH gateway prints the private English welcome panel only after the guest
shell is ready. It uses restrained terminal color when a PTY is present, with
plain output for SSH exec commands. An interactive Bash prompt then recomputes
the remaining minutes and seconds from the server-issued deadline after each
command without adding an extra status line; SSH exec commands retain their
normal behavior.

## Authority and lifecycle

The control node must atomically reserve a unique anonymous identity and
project before the worker provisions any VM. Store the reservation, deadlines,
claim-token hash and abuse-control key in its durable state. The worker is not
a second source of truth. Use server time, not a guest-reported clock.
The reservation API is runner-only and remains disabled unless
`GAP_FREE_VM_ENABLED=1`, a secret `GAP_FREE_VM_ABUSE_KEY`, and an explicit
per-node `GAP_FREE_VM_MAX_ACTIVE` are configured. The cap is not a universal
hardcoded VM limit; it must be set from measured spare capacity.

| State | Condition | Allowed operations |
| --- | --- | --- |
| Active | now < creation + 1 hour | SSH, host-filtered public web egress (legacy guests: package proxy), Basic Auth preview, claim |
| Claimable | 1 hour <= now < creation + 25 hours | claim only; VM stopped, no preview or guest egress; the original SSH key can retrieve the claim link and remaining claim time |
| Claimed | email verified and claim token consumed before expiry | owner-managed VM under normal billing/quota policy |
| Expired | unclaimed after 25 hours | revoke access, securely destroy VM and project data |

At the active deadline, revoke the SSH/HTTP/egress leases before asking QEMU
to stop; the lease check must fail closed even if the cleanup process is down.
At the claim deadline, destroy with an idempotent worker job and only then
acknowledge `finish-free-vm` to node 1 so it releases the IP and SSH key.
Expiry alone must not release them. Retry failed deletion and surface it to operators.
After successful deletion and fleet release, the same IP and SSH key may start
a new trial. If cleanup is pending, the SSH gateway must say so rather than
misreporting a capacity limit. A different key on the reserved IP must never
receive the original claim link.
An Ed25519 or RSA (2048–8192 bit) SSH public key is the anonymous trial
identity; RSA signatures must use SHA-2. Reconnecting with the
same key returns to the same VM. No email or account is required to start the
trial. Claim uses the one-use link from the terminal. The link leads to GAP's
account creation/login flow only when the visitor chooses to keep the VM.
Account creation and claim require a verified email challenge; only the
initial SSH trial is anonymous. The claim must bind the *existing* anonymous identity and project to that
account, so the worker keeps the same owner DID, VM ID, disk and preview route;
copying files into a second VM is not an acceptable substitute. The claim
token must be consumed once, bound to that project, and never exposed in
referrer headers or third-party page assets. A claimed VM
may need billing activation before it can resume; explain this on the page.

## Abuse and network controls

- One unclaimed anonymous reservation per validated public IP and SSH public
  key across the fleet, plus an explicit per-node active capacity cap. A reconnect from a new IP
  binds that IP to the same trial; it cannot open another trial concurrently.
  The SSH gateway must derive the source IP from the TCP peer, never from a
  client-supplied header. Normalize IPv4-mapped IPv6 before hashing. Hash or rotate IP keys
  rather than retaining raw addresses unnecessarily. Rate-limit handshakes,
  reject password login and SSH forwarding, and cap concurrent SSH sessions.
- Node 1 is the single-writer authority for IP and SSH-key reservations across
  the fleet. Its SQLite transaction must reject cross-node races; nodes fail
  closed if the authority cannot be reached. All three entry nodes use it.
- The v3 image has a normal IPv4 default route and DNS/HTTPS egress. QEMU slirp
  starts paused; the host CPU broker must install its per-VM cgroup firewall
  before continuing. The firewall permits only Docker DNS, selected public
  DNS resolvers and TCP 80/443; it rejects private/link-local/metadata
  destinations, SMTP, arbitrary ports and IPv6, with a connection rate and
  10-GiB outbound byte quota per VM start. An unavailable firewall fails
  closed. The older v1/v2 images keep their `restrict=on` package proxy unless
  a stopped v2 VM is explicitly opted into the new host-filtered path. A
  claimed trial remains filtered; approved paid VMs use standard egress.
- Preview must use the existing GAP Basic Auth at the edge, with unique
  generated credentials. Do not attach custom domains to unclaimed VMs;
  otherwise the custom-domain path can bypass the preview password. Expired
  preview routes must fail closed even if the edge cache is stale.
  The preview is accessible from any visitor IP with valid Basic Auth,
  including a different IP family from the SSH connection. Source IP still
  limits anonymous VM reservations, not preview access.
- Use a dedicated Debian guest image with apt and preinstalled Docker, Python,
  Node, common Linux tools, OpenCode and the supported package clients. Free
  OpenCode models are third-party services and may be rate-limited or removed.
  Never replace
  the backing image used by existing paid VMs. Image integrity and version
  must be pinned, and the image build must be reproducible.

## Validation and remaining rollout gates

The v3 network pilot passed a live host-cgroup test on all three nodes: Docker
DNS and HTTPS succeeded while metadata, private destinations and SMTP were
blocked. The immutable Debian v3 image is installed on all three nodes, and
new anonymous VMs select it. The 28 worker/gateway tests pass on each node.
Joseph's existing claimed v2 VM was switched to the same direct-network path
without rebasing its encrypted disk; `apt update`, Docker Hub and a keyless
OpenCode `big-pickle` prompt succeeded. The other free OpenCode model tested
returned an upstream error, so it is not a guaranteed GAP allowance.
The first public SSH trial of the v3 image on node 2 also passed: the guest had
DNS, HTTPS, `apt update`, Docker Hub execution and a keyless OpenCode response;
its port-8080 preview returned 401 without Basic Auth and 200 with it. A second
key from the same IP was denied. This test revealed and fixed a stale local
IP reservation after an older VM was claimed; only active or claimable
unclaimed trials now hold an IP, matching the fleet authority.

The older pilot passed real Ed25519 SSH sessions on nodes 1 and 3, a 3072-bit
RSA SSH session on node 2, and a cross-node same-IP denial. It also passed
reconnection after a
worker/image upgrade, Docker Hub and GHCR image pulls, Docker execution,
apt/npm/pip/Go/Composer package traffic and denied arbitrary egress. Unit
coverage verifies preview admission with Basic Auth from a different IP, 401
without credentials, and 403 after the free hour. A verified
email claim attached the existing project and VM ID to an account; the worker
stopped that VM and rejected its old anonymous SSH key. The Debian v2 guest
includes `php-curl` for Composer. Unit tests cover deadline stop/deletion and
central reservation races. These tests do **not** constitute a 25-hour live
expiry observation or a load test of the pilot capacity. Keep the active cap
under observation before raising capacity further. The active cap is an upper
bound, not a guaranteed number of runnable VMs: host CPU, RAM, swap and VM
admission checks still apply.

Remaining observations before increasing the cap: a live one-hour stop and
25-hour unclaimed deletion, sustained concurrent VM/package-proxy load,
and measured guest memory, image cache, disk I/O and swap pressure alongside
paying workloads. The automated deadline and deletion-retry tests already pass.

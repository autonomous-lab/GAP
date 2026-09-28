# Anonymous free MicroVM — implementation contract

This is the implementation and rollout record. Node 2 is an intentionally small
public pilot; node 1 is its central admission authority. Other nodes must not
open port 2121 until their images, measured capacity and abuse controls are
validated. The node-2 active cap is 2, a pilot value rather than a fleet-wide
hard limit. The public entry page is `/free-vm` on node 2.

## User journey

The Railway-style entry point should lead with one copyable command,
`ssh -p 2121 free@<node-host>`, and state the limits next to it: 1 vCPU,
1 GiB RAM, one active hour, and 24 further hours to claim the stopped VM.
The first terminal screen should display the VM's exact remaining active time,
preview URL, Basic Auth username/password, and claim URL. Show the same
information in a small machine-readable JSON document inside the guest for
agents. The claim page must explain that the VM remains the same VM after
claiming; it must not silently copy data into a different project.

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
| Active | now < creation + 1 hour | SSH, package proxy, private preview, claim |
| Claimable | 1 hour <= now < creation + 25 hours | claim only; VM stopped, no preview or guest egress |
| Claimed | email verified and claim token consumed before expiry | owner-managed VM under normal billing/quota policy |
| Expired | unclaimed after 25 hours | revoke access, securely destroy VM and project data |

At the active deadline, revoke the SSH/HTTP/egress leases before asking QEMU
to stop; the lease check must fail closed even if the cleanup process is down.
At the claim deadline, destroy with an idempotent worker job and only then
acknowledge `finish-free-vm` to node 1 so it releases the IP and SSH key.
Expiry alone must not release them. Retry failed deletion and surface it to operators.
The SSH public key is the anonymous trial identity: reconnecting with the
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
  closed if the authority cannot be reached. The initial pilot may expose a
  single entry node, but its admission still goes through node 1.
- Keep QEMU `restrict=on`. Provide outbound access only through an authenticated
  per-VM package proxy or controlled mirrors for Debian apt, npm, PyPI, Go
  modules, Composer, Docker Hub and GHCR. DNS and redirects must be resolved
  against an explicit policy at every hop; deny private, link-local, metadata,
  raw-IP, arbitrary-port and general CONNECT destinations. Apply bandwidth,
  request, download-size and total egress limits. Public registry pulls only
  unless private credentials get a separate security design.
- Preview must use the existing GAP Basic Auth at the edge, with unique
  generated credentials. Do not attach custom domains to unclaimed VMs;
  otherwise the custom-domain path can bypass the preview password. Expired
  preview routes must fail closed even if the edge cache is stale.
- Use a dedicated Debian guest image with apt and preinstalled Docker, Python,
  Node, common Linux tools, and the supported package clients. Never replace
  the backing image used by existing paid VMs. Image integrity and version
  must be pinned, and the image build must be reproducible.

## Validation and remaining rollout gates

The node-2 pilot has passed a real Ed25519 SSH session, reconnection after a
worker/image upgrade, Docker Hub and GHCR image pulls, Docker execution,
apt/npm/pip/Go/Composer package traffic, denied arbitrary egress, and private
preview checks (401 without Basic Auth; 403 from a different IP). A verified
email claim attached the existing project and VM ID to an account; the worker
stopped that VM and rejected its old anonymous SSH key. The Debian v2 guest
includes `php-curl` for Composer. Unit tests cover deadline stop/deletion and
central reservation races. These tests do **not** constitute a 25-hour live
expiry observation or a load test of the pilot capacity. Keep the active cap
small and monitor it before expanding to other nodes.

1. Unit tests for state boundaries, SSH-key reconnect, idempotent reservation
   and claim, restart hydration, simultaneous SSH arrivals, abuse quotas and
   deletion retries. The first SSH connection must never ask for an email.
   Test the one-VM-per-IP rule across IPv4, IPv6, IPv4-mapped IPv6, and a
   reconnect from a different network.
2. Network tests proving allowed package and public image pulls work while
   arbitrary outbound, scanning, private addresses and redirects are denied.
3. End-to-end test on a non-production node: fresh SSH connection, toolchain,
   Docker build/run, Basic Auth preview, stop at one hour, claim during grace,
   expiry/deletion of an unclaimed VM, and persistence after restart.
4. Capacity and resource accounting test: include guest memory, proxy, SSH
   gateway, image cache, disk I/O and swap pressure. Set a separate anonymous
   reservation budget so the offer cannot evict paying workloads.
5. Only after passing these gates: publish port 2121, enable the landing page,
   observe a small pilot, then roll out to the other nodes.

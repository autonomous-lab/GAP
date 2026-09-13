# Moving a MicroVM between hosts

A move is a cold restart: the guest stops, its disk and SSH keys are transferred,
then it starts on the destination if it was running. Process memory is not moved.
The project ID, VM ID and existing application HTTPS URL are preserved. Public
SSH and TCP/UDP addresses and port numbers change; retrieve them after the move.

The original HTTPS host keeps its certificate and visitor authentication and
proxies over authenticated TLS to the destination. It must remain available.
This is not high availability or automatic rebalancing. Existing custom domains
continue through their original edge. A permanent loss of the source before its
export is complete cannot be repaired without a separate disk backup.

## Account dashboard

In Account, choose **Move** next to a MicroVM, select the destination, review its
rates and accept the interruption and endpoint changes. Progress, failures,
**Retry**, and pre-commit **Cancel move** appear under **MicroVM moves**.
Refresh the fleet after completion to open the destination dashboard.

Visitor credentials remain on the original HTTPS host. Use **Endpoints / SSH →
Visitor settings** from Account to manage those credentials. The destination's
HTTP credentials are used for the private proxy hop and must not be replaced as
if they were the original visitor password. The destination dashboard hides them.

## API

Use an operator-account client credential with owner/operator project rights.
Viewer credentials cannot initiate, retry or cancel a move. The destination must
already approve this owner for MicroVM hosting. The coordinator provisions the
existing project on the destination after the central placement commit.

1. Read `GET /v1/fleet/nodes` and select an available destination tariff.
2. Send `POST /v1/fleet/migrations` with:

```json
{
  "action": "start",
  "request_id": "a-unique-request-id",
  "project_id": "prj_...",
  "vm_id": "vm_...",
  "target_node": "node-02",
  "target_tariff": {
    "version": "copy-the-current-version",
    "vcpu_hour": 10000,
    "gib_ram_hour": 10000,
    "gb_disk_month": 100000,
    "gb_in": 10000,
    "gb_out": 10000
  },
  "confirm_downtime": true
}
```

The tariff values above are illustrative: copy the entire current `tariff`
object from the selected node. Changed rates require fresh confirmation.
Reuse the identical request ID and body if the initial response is lost.

3. Poll `GET /v1/fleet/migrations?migration_id=move_...`. An unfiltered GET lists
   up to 100 recent moves authorized for the caller. No hop password or node
   credential is returned. Temporary transport failures are retried; an authority
   rate limit pauses the copy before retrying the same chunk. Received bytes persist.
4. On a recoverable failure, POST `{"action":"resume","migration_id":"move_..."}`.
   Before commit, POST `{"action":"cancel","migration_id":"move_..."}` to remove
   the target copy and restore the source. A committed move must be completed or
   moved back with a new migration; it is never rolled back automatically.
5. Read `GET /v1/fleet/vm-placements` for the current host. A project token accepts
   `node_id` for a host authorized by placement. Requests that would operate on
   the old source return `vm_migrated`; use the destination API/dashboard.

## Recovery and accounting

The source is fenced before it stops. The destination disk is validated outside
its live catalog. Ordered journal receipts protect placement, capacity and
billing handoff. Uploads resume at the destination's durable byte offset and
replayed chunks must match. Project quotas are not duplicated, and the final
source usage checkpoint precedes destination accounting.

A lost network connection does not release the source fence. Coordinator restart
resumes queued/running work; a failed job remains available for explicit retry or
cancellation. Cancellation removes the staged target before source restoration.
Successful moves remove the old source disk and temporary transfer archives;
a small fenced source catalog entry retains the original route identity.

The coordinator checks destination approval, kernel/CPU compatibility, memory,
temporary disk capacity and tariff confirmation before export. Project-local
budgets are not supported across hosts: a non-null budget rejects export, and
setting one on a migrated project is refused until central budget enforcement
exists. Shared account credit and capacity quotas remain enforced.

## Operator activation

All nodes require the same tested worker/control protocol. Configure a distinct
`GAP_FLEET_MIGRATION_TOKEN` on each node. This credential authorizes only the
fixed migration service endpoint, not arbitrary worker administration.

Each worker's private configuration enables `migration_transfers` and provides
`migration_peers`, keyed by node ID, containing a trusted HTTPS `origin` and
`token_file`. Never place the credentials in a client script or ticket.
The control configuration enables `allow_migration_journal`, existing reservation
and capacity enforcement, and `migration_peers` with the same origin/token-file
mapping. Use distinct filenames such as `migration-node-01.token`; never overwrite
the existing `node_token_files` credentials used for fleet billing. Project signing and node pricing sources are required. Activate the
coordinator last, after both nodes and workers are ready. Each worker
`hypervisor.public_network.hostname` must resolve to its own public TCP address;
do not reuse the original node hostname on a destination node.

The migration does not require a private inter-host network. TLS validation is
mandatory; no redirect, caller-selected upstream URL or anonymous app route is
used for the transfer or HTTP proxy.

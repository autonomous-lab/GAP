# GAP node operations

## Inventory

| Node | SSH host | Checkout | Public origin |
|---|---|---|---|
| gap-node-01 | root@159.195.122.180 | /opt/app/gap-node-01 | https://gap.geta.team |
| gap-node-02 | root@159.195.123.24 | /opt/app/gap-node-02 | https://gap-node-02-u3.vm.elestio.app |

Node 02 hostname: `cicd-gap-2-u3`. Observed capacity: 8 vCPUs, approximately
16 GiB RAM, 196 GiB root filesystem; `/dev/kvm` is present. The existing operator
SSH key is authorized on both hosts. From the operator workspace:

```sh
ssh -i /opt/app/data/.ssh/id_ed25519 root@159.195.123.24
```

The private key stays in the operator workspace. Never copy it into a node or
repository. Each node keeps its own live secrets in its checkout's `.env`.
Node 02 uses Elestio nginx with its assigned HTTPS hostname, forwarding to
`172.17.0.1:8080`. Its `GAP_PUBLIC_URL` must name node 02, not node 01's origin.

Both nodes are configured in CI/CD against the same GAP repository. Every push
can rebuild and redeploy both stacks. Validate changes on the target host before
pushing, and check the health of both nodes after the automatic deployments.

## Verification email transport

Both hosts run the existing `elestio-postfix` container. The provisioning script
is `/opt/elestio/startPostfix.sh` (case-sensitive). Use its
`RELAYHOST_USERNAME` as the node's authorized envelope and From address:

| Node | Sender |
|---|---|
| gap-node-01 | cicd-gap-u3.vm.elestio.app@vm.elestio.app |
| gap-node-02 | cicd-gap-2-u3.vm.elestio.app@vm.elestio.app |

The local relay is published at `172.17.0.1:25`, forwarding to container port
587. SMTP EHLO succeeded on both hosts; neither advertised STARTTLS on this
local endpoint. GAP should use the local relay, leaving upstream authentication
inside Postfix. Never copy the script's relay password into source, logs or public
node metadata. Do not expose this local SMTP listener publicly. Container access
and actual message delivery still require integration testing; EHLO alone does
not establish recipient delivery. Both deployed nodes now enable `GAP_EMAIL_VERIFICATION_REQUIRED=1`; their
`/v1/registration` policy and `/signup` page have been checked publicly.
Run `python3 scripts/configure-elestio-smtp.py` from the checkout to configure
the sender, host and port without exposing upstream credentials. This command
does not activate registration or restart the node. Validate delivery and retain
`data/gap-node/registration.sqlite` with the node backup before enabling.

## Fresh-node initialization and recovery

The initial node-02 deployment failed at service startup: the function sandbox
had no `SANDBOX_TOKEN`, and realtime had no `REALTIME_SECRET`. Those container
variables come from `GAP_FUNCTION_SANDBOX_TOKEN` and `GAP_REALTIME_SECRET` in `.env`.
ClickHouse was healthy and its data does not need to be reset.

After configuring node identity, master key, operator credentials and ClickHouse
credentials in `.env`, run from the checkout:

```sh
python3 scripts/init-runtime-secrets.py
python3 scripts/deploy-check.py
docker compose config --quiet
docker compose up -d --build
docker compose ps
```

The initializer creates only missing or empty runtime secrets, preserves existing
values, rejects duplicate definitions, and writes newly updated `.env` files with
mode 0600. It prints key names only. Do not run concurrent initialization or edit
`.env` while it runs. Compose refuses missing runtime secrets before deployment.
Reusing existing values preserves issued realtime tokens and worker authentication.
An interrupted first deployment may leave no node/edge images; use `--build`.

## Cluster preparation boundary

These are currently independent GAP stacks. Deploying the second stack does not
replicate projects, identities, balances, files or microVMs. Node 02 is available
for future cluster placement and rebalancing work; it has not been joined to a
shared scheduler or billing ledger, and no microVM worker is configured there yet.
Keep node-01 project storage and its prepaid wallet authoritative until an explicit
cluster design and migration implement shared ownership, routing, fencing and
storage movement. Never copy a live ledger independently to both nodes and charge
against both copies.

## Individual administrator origin

Node 01 reserves `https://gap-node-01-u3.vm.elestio.app/admin` for the Cloud
operator console. The client origin remains `https://gap.geta.team`.
Both Rust and nginx exclude tenant application, static-site and realtime routes
from the administrator origin. The administrator allowlist is configured in the
host `.env`; no password or browser session belongs in this repository.

Node 02 currently uses its Elestio hostname as its client origin. Do not assign
that same origin to the administrator console: a separate origin is required.
The initial console inventory is node-local, not a completed fleet aggregate.

Validation: `GAP_TEST_BINARY=/absolute/path/to/gap python3 scripts/test_admin_http.py`
starts a disposable node and a loopback-only SMTP sink. It tests first enrollment,
password/code authentication, one-use challenges, origin isolation, CSRF,
logout, resource inventory and live approval application. No external email is sent.

# Verified client IP for MicroVM preview access

Free MicroVM previews use an IP allowlist: the latest SSH source IP, the latest
authenticated dashboard browser IP, and at most three user-managed addresses.
Only `/apps/{project}/` preview routes are restricted. Verified custom domains
remain public; conventional MicroVMs keep their visitor-credential policy.

The host Caddy instance must overwrite `X-GAP-Verified-Client-IP` in **every**
`reverse_proxy` block with `{client_ip}`. It must also set global
`servers { trusted_proxies static ...; trusted_proxies_strict }`, trusting the
current Cloudflare CIDRs. On nodes 2 and 3, additionally trust only the public
IP of the node-1 administration proxy (`159.195.122.180/32`). Never trust
arbitrary `X-Forwarded-For` or `CF-Connecting-IP` values from a direct client.

The node-1 nginx `/nodes/node-02/` and `/nodes/node-03/` proxy locations send
their Caddy-verified IP as the sole `X-Forwarded-For` value. The destination
Caddy then produces a new verified header for its local nginx and GAP node.
The nginx VM admission subrequest passes that value as `X-GAP-Client-IP`.
Management requests carry it to the GAP node only with an nginx-injected edge
token; direct calls to the node cannot forge the verified-IP header. The token
is never forwarded to the guest application backend.
Keep the Caddy configurations in sync across all three host checkouts when
changing edge routing. Validate candidates with `caddy validate` and check a
direct request with forged forwarding headers still receives HTTP 403.

Human account sessions last 48 hours in a `Secure; HttpOnly; SameSite=Strict`
cookie. The browser must not put that bearer in `localStorage`.

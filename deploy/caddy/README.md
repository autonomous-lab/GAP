# GAP Caddy edge

Caddy terminates GAP's own HTTPS traffic and issues customer-domain
certificates on demand. It runs separately from the application Compose stack
because it owns host ports 80 and 443.

## Install

```bash
install -d -m 700 /opt/elestio/caddy
cp deploy/caddy/{Caddyfile,docker-compose.yml,reload-origin-cert.sh} /opt/elestio/caddy/
cp deploy/caddy/gap-caddy.cron /etc/cron.d/gap-caddy
chmod 600 /opt/elestio/caddy/.env
chmod 755 /opt/elestio/caddy/reload-origin-cert.sh
chmod 644 /etc/cron.d/gap-caddy
cd /opt/elestio/caddy && docker compose up -d
```

Create `/opt/elestio/caddy/.env` with `CADDY_ACME_EMAIL` and
`GAP_CADDY_ASK_TOKEN`. The latter must equal the value in GAP's live `.env` for
that same node; never commit it. Set `GAP_CUSTOM_DOMAIN_TARGET` in GAP to the
public A-record target shown to agents. Protect the file with mode 600.

The historical GAP names use Elestio's mounted wildcard origin certificate.
Caddy manages customer certificates in `/opt/elestio/caddy/data`. The cron
reload makes Caddy reread the wildcard after Elestio/acme.sh renews it.

Customer DNS may be either DNS-only or proxied through Cloudflare. A proxied
hostname also works with Cloudflare's default Flexible SSL mode: Caddy accepts
the HTTP origin request without creating a redirect loop only when the TCP peer
is in Cloudflare's published address ranges and `CF-Visitor` reports `https`.
Direct clients cannot spoof this exception. Full (strict) remains preferable
because it encrypts the Cloudflare-to-origin hop; DNS-only domains receive
automatic Let's Encrypt certificates directly from Caddy.

When Cloudflare changes its published ranges, update the two `remote_ip` lists
in `Caddyfile` from `https://www.cloudflare.com/ips/`, validate, and reload.

## Migration and rollback

Validate before switching:

```bash
docker compose config
docker run --rm --env-file .env \
  -v "$PWD/Caddyfile:/etc/caddy/Caddyfile:ro" \
  -v /root/.acme.sh/vm.elestio.app:/certs:ro \
  caddy:2.10.2-alpine caddy validate --config /etc/caddy/Caddyfile
```

The retired Elestio nginx is deliberately stopped, not deleted, and its Docker
restart policy is `no` so a VM reboot cannot create a port conflict. Roll back:

```bash
cd /opt/elestio/caddy && docker compose down
docker update --restart=always elestio-nginx
cd /opt/elestio/nginx && docker compose up -d
```

The Caddyfile blocks `/internal/*` publicly, including on the HTTP compatibility
listener. Its private `ask` call goes
directly to the bridge-bound GAP edge and includes the shared token. Unknown,
pending, suspended, inactive-project and missing-site hostnames all fail closed
before ACME issuance.

### Fleet console host on secondary nodes

When a secondary node uses the central `GAP_ADMIN_ORIGIN`, its explicit Caddy
site block must include that central hostname as well as its own hostname.
The fleet edge connects with the secondary hostname for TLS SNI but preserves
the central console Host header for session and administrator isolation.
Without the central hostname in the explicit site block, the catch-all marks
management requests as tenant traffic and returns `site not found`.
For node-02 the explicit block is:

```caddyfile
gap-node-02-u3.vm.elestio.app, gap-node-01-u3.vm.elestio.app {
    tls /certs/fullchain.cer /certs/vm.elestio.app.key
    @internal path /internal/*
    respond @internal 404
    reverse_proxy 172.17.0.1:8080 {
        header_up Host {host}
        header_up -X-GAP-Custom-Domain
    }
}
```

For node-03, use the same block with its own hostname:

```caddyfile
gap-node-03-u3.vm.elestio.app, gap-node-01-u3.vm.elestio.app {
    tls /certs/fullchain.cer /certs/vm.elestio.app.key
    @internal path /internal/*
    respond @internal 404
    reverse_proxy 172.17.0.1:8080 {
        header_up Host {host}
        header_up -X-GAP-Custom-Domain
    }
}
```

Keep the central hostname isolated by the existing GAP admin-origin boundary;
do not remove tenant marking from the catch-all or forward arbitrary headers
as trusted host identities. No public DNS change is needed.

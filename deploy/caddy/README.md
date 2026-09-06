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

Create `.env` from `.env.example`. `GAP_CADDY_ASK_TOKEN` must equal the value
in GAP's live `.env`; never commit it. Set `GAP_CUSTOM_DOMAIN_TARGET` in GAP to
the public A-record target shown to agents.

The historical GAP names use Elestio's mounted wildcard origin certificate.
Caddy manages customer certificates in `/opt/elestio/caddy/data`. The cron
reload makes Caddy reread the wildcard after Elestio/acme.sh renews it.

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

The Caddyfile blocks `/internal/*` publicly. Its private `ask` call goes
directly to the bridge-bound GAP edge and includes the shared token. Unknown,
pending, suspended, inactive-project and missing-site hostnames all fail closed
before ACME issuance.

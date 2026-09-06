#!/bin/sh
set -eu

# The two historical GAP names use Elestio's wildcard origin certificate.
# acme.sh renews the mounted files; reloading makes Caddy reread them without
# interrupting connections. Customer-domain certificates are managed directly
# by Caddy and need no external reload.
docker exec gap-caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile

#!/bin/sh
set -eu
case "${GAP_ADMIN_ORIGIN:-}" in
  '') GAP_ADMIN_HOST=disabled-admin.invalid ;;
  https://*) GAP_ADMIN_HOST=${GAP_ADMIN_ORIGIN#https://}; GAP_ADMIN_HOST=${GAP_ADMIN_HOST%/} ;;
  *) echo 'Invalid administrator origin' >&2; exit 1 ;;
esac
case "$GAP_ADMIN_HOST" in
  ''|*[!a-zA-Z0-9.-]*) echo 'Administrator origin must use a DNS hostname without a port' >&2; exit 1 ;;
esac
GAP_VM_EDGE_TOKEN=${GAP_VM_EDGE_TOKEN:-}
case "$GAP_VM_EDGE_TOKEN" in
  *[!a-f0-9]*) echo 'VM edge token must be hexadecimal' >&2; exit 1 ;;
esac
GAP_FLEET_NODE02_HOST=${GAP_FLEET_NODE02_HOST:-}
case "$GAP_FLEET_NODE02_HOST" in
  *[!a-zA-Z0-9.-]*) echo 'Fleet management target must be a DNS hostname' >&2; exit 1 ;;
esac
export GAP_ADMIN_HOST GAP_VM_EDGE_TOKEN GAP_FLEET_NODE02_HOST
envsubst '${GAP_ADMIN_HOST} ${GAP_VM_EDGE_TOKEN} ${GAP_FLEET_NODE02_HOST}' < /etc/nginx/gap.conf.template > /tmp/gap-nginx.conf
exec nginx -c /tmp/gap-nginx.conf -g 'daemon off;'

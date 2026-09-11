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
export GAP_ADMIN_HOST
envsubst '${GAP_ADMIN_HOST}' < /etc/nginx/gap.conf.template > /tmp/gap-nginx.conf
exec nginx -c /tmp/gap-nginx.conf -g 'daemon off;'

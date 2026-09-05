#!/bin/sh
set -eu
chown node:node /data
exec su-exec node node /opt/realtime/server.mjs

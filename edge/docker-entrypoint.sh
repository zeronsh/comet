#!/bin/sh
# Docker entrypoint: the production Worker under local workerd, state on /data.
set -eu
exec npx wrangler dev \
  --config wrangler.selfhost.jsonc \
  --ip 0.0.0.0 \
  --port 8787 \
  --local \
  --persist-to /data \
  --var "AUTH_MODE:${AUTH_MODE:-none}" \
  --var "SELFHOST_USER_ID:${SELFHOST_USER_ID:-local}" \
  --var "SELFHOST_ORG_ID:${SELFHOST_ORG_ID:-local}"

# Zeron edge — self-host image.
# The same Worker + Durable Object + R2 code as production, run under local
# workerd (via wrangler) with rooms and blobs persisted to /data.
#
#   docker build -t zeron-edge .
#   docker run --rm -p 8787:8787 -v zeron-edge-data:/data zeron-edge
#
# See docs/SELFHOST.md.
FROM node:22-bookworm-slim

WORKDIR /app

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

COPY package.json package-lock.json ./
RUN npm ci

COPY tsconfig.json wrangler.selfhost.jsonc docker-entrypoint.sh ./
COPY src ./src

RUN chmod +x docker-entrypoint.sh

ENV AUTH_MODE=none \
    SELFHOST_USER_ID=local \
    SELFHOST_ORG_ID=local \
    npm_config_update_notifier=false

EXPOSE 8787
VOLUME ["/data"]

ENTRYPOINT ["./docker-entrypoint.sh"]

# Self-hosting the edge

Run the same `edge/` Worker that backs `edge.zeron.sh` on your own machine:
local workerd via `wrangler dev`, Durable Object and R2 state on disk, no
Cloudflare account, no WorkOS. Every device you point at it lands in one
shared workspace, so you get multi-device sync without an account.

## Quick start

```bash
docker compose -f docker-compose.selfhost.yml up --build
curl -s localhost:8787/health
# {"ok":true,"auth":"none","userId":"local","orgId":"local"}
```

Point an engine at it. Setting `ZERON_WORKOS_CLIENT_ID` to the empty string
turns the built-in production login off; because `ZERON_EDGE_URL` names an
edge explicitly, the engine probes its `/health`, sees `auth: "none"`, adopts
the edge's identity and enables sync. No `zeron login`. (A dev-mode engine
with no `ZERON_EDGE_URL` never touches the network — that behaviour is
unchanged.)

```bash
ZERON_EDGE_URL=http://localhost:8787 ZERON_WORKOS_CLIENT_ID= zeron headless
```

To make it stick across reboots, install the daemon with those variables in
the environment — `zeron daemon install` bakes `ZERON_EDGE_URL` and
`ZERON_WORKOS_CLIENT_ID` into the service unit:

```bash
ZERON_EDGE_URL=https://zeron.example.net ZERON_WORKOS_CLIENT_ID= zeron daemon install
```

The desktop app reads the same two variables.

## What `AUTH_MODE=none` means

There is no bearer check. Every request is treated as the single fixed user
and org configured on the edge (`SELFHOST_USER_ID` / `SELFHOST_ORG_ID`,
default `local` / `local`), which is Zeron's one-user-many-devices model with
the identity supplied by the server instead of a login.

That also means **anyone who can reach the port is that user**. Put the edge
on a private network — a LAN, Tailscale, or behind a reverse proxy that does
its own authentication. Do not expose port 8787 to the public internet.

Compared with `AUTH_MODE=dev` (which already existed for local development),
`none` moves the identity to the edge: devices no longer need a matching
`ZERON_EDGE_TOKEN=user@org`, they just need the URL. An explicit
`ZERON_EDGE_TOKEN` is ignored against a `none` edge, because the edge will
only authorize rooms under its own org anyway.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `AUTH_MODE` | `none` | Leave as `none` for self-host. `dev` and `workos` behave as in `wrangler.jsonc`. |
| `SELFHOST_USER_ID` | `local` | The user every caller becomes. |
| `SELFHOST_ORG_ID` | `local` | The org every caller becomes. Changing it after first use starts a fresh workspace. |

State lives in the `zeron-edge-data` Docker volume (`/data` in the
container): Durable Object SQLite for rooms and registries, and the R2-backed
object store for attachments and backups. Back it up like any other volume.

### Without Docker

```bash
cd edge
npm ci
npm run dev:selfhost          # wrangler dev, persisted to ./data, on 0.0.0.0:8787
```

### Updates

Clients look for release artifacts on the edge they are pointed at
(`/releases/manifest.json`). A self-hosted edge has none, so `zeron update`
and the in-app auto-update log a warning and do nothing; update the binary
the way you installed it. Publishing your own builds into the `RELEASES`
bucket makes the normal flow work again.

### TLS

workerd serves plain HTTP. For anything beyond one machine, terminate TLS
in front of it (Caddy, nginx, Tailscale Serve) and give clients the `https://`
URL. WebSocket upgrades must be passed through.

## Keeping `wrangler.selfhost.jsonc` current

The self-host config is a copy of `wrangler.jsonc` minus the Cloudflare
account, routes and production vars. When a new Durable Object class or
migration is added to `wrangler.jsonc`, add it here too — a class missing
from the self-host bindings is a room the Worker cannot route to.

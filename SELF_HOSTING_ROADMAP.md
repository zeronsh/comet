# OSS-first web installation roadmap

Status: recommended direction, approved for further planning; not implemented.

## Goal

Install on your own machine and use the browser UI without mandatory WorkOS,
Cloudflare, or a maintainer-operated relay. Keep the existing cloud integration
optional. The browser remains a client: execution and persistence belong to the
native backend, even when both run on the same physical machine.

## Recommended default

Ship a native release with version-matched web assets. One startup command should
start the backend and authenticated web gateway, show the local URL, and support
explicit browser pairing. This is a proposed experience, not an available command.

Browser -> authenticated same-origin RPC -> user-owned native engine and storage.

Prefer a binary plus bundled assets first. Docker can follow for users who want
execution inside a container; it needs explicit workspace/state mounts, user IDs,
provider credentials and tooling. Do not require privileged containers or access
to the Docker socket.

## Authentication without mandatory WorkOS

WorkOS currently supplies identity and provider sessions to the optional cloud
mode. Ordinary users of an operator-managed deployment do not supply API keys;
independent operators currently need their own identity/relay configuration.
The current browser cloud deployment admits one configured owner, not arbitrary
multi-user signup.

For direct mode, propose explicit, trusted-owner pairing:

- Generate a high-entropy, short-lived, one-use code through a local CLI action.
- Submit it in a POST body, not a URL, logs or localStorage; consume atomically.
- Issue an opaque HttpOnly session cookie, with Secure and host-only scope for
  HTTPS; use an explicit, constrained loopback exception for local HTTP.
- Preserve Host/Origin validation, CSRF protection, rate and size limits, expiry,
  logout/revocation and closure of authenticated WebSockets.
- Do not use development bearer authentication, an unauthenticated first-visitor
  setup page, or expose native IPC directly to the Internet.
- Pairing grants native-like authority over the selected host profile: terminal
  commands and file operations execute as the backend OS user. Explain this.

Provider login for coding agents is separate from browser-to-backend pairing.
Existing synced profiles must not silently migrate or impersonate local profiles.

## Exposure options

1. Localhost: simplest default, no external account or networking setup.
2. SSH forwarding: remote access for users who already have SSH, without a new
   maintainer-operated service.
3. User-chosen private network/VPN: optional convenience for multiple devices.
4. User-owned HTTPS reverse proxy or tunnel: explicit external origin and secure
   forwarding configuration, including HTTP and WebSocket routes.
5. Optional cloud/self-hosted relay: advanced multi-machine connectivity, not a
   prerequisite for using the application.

No universal NAT traversal is promised without a reachable path, VPN, tunnel or
relay. Removing maintainer service costs does not remove user hardware/network
costs or agent-provider subscriptions. Do not promise third-party services will
remain free.

Serve the UI and browser API at the same origin. Preserve COOP/COEP required by
the shared-memory web runtime. Avoid wildcard CORS and unvalidated proxy headers.

## Reuse and gaps

The active development repository contains a local engine profile, a restricted
loopback web gateway, and a portable typed RPC client. These are reusable pieces,
not proof that direct mode already works. The legacy gateway was deliberately
excluded from the published WIP snapshot and must be reviewed before reuse.

The current shared browser UI uses DeviceRoom binary framing and browser-session
routes. The older gateway uses text RPC and a different authentication API. Add
an explicit direct transport/bootstrap mode while retaining one shared AppState
and Shell, rather than restoring a parallel legacy UI state tree.

The old gateway restricts operations and has limits unsuitable for full parity.
Intentionally authorize trusted-owner operations and verify terminals, files/Git,
projects, queues, input responses and attachment upload/read-back. Reconcile
attachment chunk sizes with bounded transport limits. Never simply disable the
policy or remove backpressure.

Single-host persistence can remain local. Start with separate server URLs rather
than promising cloud-style device discovery and shared history without a relay.

## Preview security

Untrusted project previews must not run under the authenticated application
origin. Reuse native discovery/proxy logic, but separately implement and test
capability authorization, constrained targets, expiry/revocation, cookie stripping
and HTTP/WebSocket forwarding on isolated origins. Different ports alone do not
isolate cookies. Remote embedded previews may require dedicated hostname and TLS
configuration; an external-preview fallback must be labeled as reduced parity.

## Delivery stages

1. Specify direct-mode bootstrap, single-owner pairing and profile boundaries.
2. Package matched backend/web assets and one-command service startup.
3. Connect the actual shared UI through direct RPC and test full owner workflows.
4. Document localhost/SSH installation, upgrades, persistent data and recovery.
5. Add opt-in HTTPS/private-network/tunnel exposure and isolation tests.
6. Address embedded previews and optional advanced multi-host/cloud workflows.
7. Add Docker packaging after the native installation path is reliable.

Open decisions: initial LAN/phone access requirements, embedded-preview release
requirements, session persistence across restart, and future multi-user roles.

## Published work status

The existing WIP application snapshot was pushed to Gratenes/zeron on branch
wip/browser-remote-parity (initial commit 04415cf). Its locked browser build passed;
exact native build completion remains unverified after validation timeouts. This
roadmap does not claim direct mode is implemented or that the branch is PR-ready.

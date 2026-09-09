# Project previews

Run an HTTP development server in a project and open a Browser tab in that
session. The empty tab lists its running services. **Open** navigates to
`http://<device>.<project>.localhost:7331`; additional services receive persistent
names such as `<device>.<project>-api.localhost:7331`.

The daemon owns this listener, discovery and peer connections. Closing a browser
tab does not stop discovery or change an address. Account teardown cancels the
proxy, discovery, signaling, and existing peer streams before the next profile
starts. A busy proxy port produces an inline error instead of changing the URL.

## Discovery and identities

On Linux, the scanner joins the current user's `/proc` process cwd, ancestry,
creation time and socket descriptors to listening TCP sockets. On macOS it uses
`lsof` and `ps` for equivalent metadata. Only loopback-reachable listeners whose
cwd belongs to a known local project are probed. The deepest matching project
wins. Unrelated listeners and non-HTTP services are excluded. HTTP probes are
bounded and run every two seconds, using HEAD and accepting valid HTTP status
responses (including authentication and application errors).

Zeron terminal/task/agent descendants are marked as Zeron-owned. Framework
commands identify Vite, Next.js, Astro, Miniflare and Node servers; otherwise the
list uses a generic HTTP label. Before a local backend connection, the daemon
rechecks the listener's process identity and cwd to reject stale port reuse.

The profile's `previews.json` stores project and service IDs and hostname labels
separately from observed PIDs and ports. Command identities omit common port
options. Device-name collisions get persisted suffixes. The first service keeps
the project hostname; additional services get descriptive suffixes. Two
indistinguishable instances of the same command in the same cwd get separate
slots; their individual identities cannot be inferred across simultaneous
restarts without additional application-provided identity.

`WatchPreviews` resolves the session's current cwd on each catalog or workspace
change. On the viewing device it selects either the local services or the
advertised services for the session's owning device. Changing the active
checkout therefore changes the list without copying paths or entering ports.

## HTTP and stream transport

On macOS 14 and later, WebKit receives a per-domain HTTP CONNECT configuration
for preview hostnames, avoiding older macOS DNS behavior without system changes.
The CONNECT endpoint admits only known preview names at the proxy port, and its
tunnels share bounded limits and account cancellation. Other website traffic
retains normal routing.

The loopback proxy validates the Host against its catalog, then opens an opaque
service ID through a transport-independent multiplexer. Locally, framed streams
travel over a socket pair. Remotely, the identical frames travel over one ordered,
reliable WebRTC DataChannel. Remote metadata never authorizes an arbitrary TCP
address: the hosting device resolves only its own live service IDs.

Frames have a one-byte kind, a big-endian 32-bit stream ID, and a bounded payload.
Kinds are OPEN, DATA, END, CANCEL, WS_OPEN, WS_DATA, WS_CLOSE, READY and CREDIT.
Socket transport prefixes frames with a big-endian 32-bit length; DataChannel
messages already provide framing. Peers allocate opposite stream-ID parity.
DATA payloads are at most 8 KiB. Each stream has a 64 KiB receive window; connection
queues and the SCTP send buffer are bounded. Up to 64 streams share a connection.
END half-closes; CANCEL tears down both directions. Graceful shutdown drains the
last buffered bytes, whereas dropping an unfinished request cancels promptly.

Hyper streams request and response bodies. The proxy removes hop-by-hop headers,
preserves Host and Origin together (including Next.js Server Actions), sets
X-Forwarded-Host, and rewrites absolute localhost redirects. Set-Cookie and other
end-to-end headers remain intact. WebSocket upgrades retain their byte stream,
subprotocol and close handshake, allowing Vite HMR through the same URL.

## Presence and P2P

The Worker authenticates the existing access token, verifies the organization
claim, and derives a PreviewRoom name from the verified organization and user.
Clients cannot select another user's room. The coordinator stamps device sender
identity and accepts only bounded service catalogs and pairing SDP. Gathered ICE
candidates are included in SDP, along with the DTLS fingerprint. Binary frames
and arbitrary proxy protocol messages are rejected.

Presence has a heartbeat lease, hibernation-safe catalog storage and periodic
credential refresh. Disconnects clear advertised routes and peer connections.
Only the lexicographically lower device creates offers, avoiding simultaneous
offer collisions. DataChannels are opened lazily when a preview is requested.
DTLS authenticates the fingerprint exchanged through the authenticated signaling
connection. Application requests, response bodies and WebSocket messages never
pass through the Durable Object.

The initial transport uses host candidates and public STUN, without a TURN
service or a cloud byte-relay fallback. Networks that prohibit direct ICE
connectivity return a bounded connection error. Deploy the Worker with its v4
PreviewRoom migration to enable cross-device coordination; local previews work
independently of edge availability. macOS and Linux currently provide discovery.

## Validation

`cargo test --locked -p zeron-preview` covers real process/cwd isolation, non-HTTP
exclusion, live disappearance, persistent aliases, port changes, concurrent
streams, slow readers, cancellation, large bodies, streaming HTTP headers and
redirects, WebSocket traffic, and a real WebRTC pair in both directions.

`npm --prefix edge test` includes real workerd tests for room isolation,
organization authorization, stamped signaling, disconnect cleanup and binary
traffic rejection. CI runs networking tests on Linux and macOS.

Build `cargo build -p zeron-ui --example preview-fixture --features browser-fixture`.
Run the fixture with an output directory, an available display and `VITE_BINARY`
pointing to an installed `vite/bin/vite.js`. It starts real Vite/API processes in
an isolated project, discovers them through daemon RPC and waits for a native
click on Vite's Open button. It then verifies HMR, disappearance and a port-change
restart while capturing the native UI. Screenshots/videos belong in PR user
attachments, not the repository.

The opt-in `coordinator` integration test connects two authenticated clients to a
local Worker, advertises a service, pairs over SDP/ICE, then transfers a 4 MiB
HTTP response through the remote hostname. Run it with
`ZERON_PREVIEW_TEST_EDGE=http://127.0.0.1:27641 cargo test -p zeron-preview --test coordinator -- --ignored`.

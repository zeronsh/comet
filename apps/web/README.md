# Zeron web client

The browser client renders the shared `zeron_ui` application and connects to a signed-in remote device through the edge browser-session and device-room APIs.

## Build

Use the pinned GPUI dependencies in `Cargo.toml` to build for `wasm32-unknown-unknown`. The browser uses the authenticated edge routes; it does not use a local IPC or loopback gateway.

## Tests

Run the lifecycle crate's Rust tests and the edge browser-session/device discovery workerd tests before publishing assets or deploying the edge worker.

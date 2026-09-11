//! Compiles the real browser modules without GPUI or a native engine runtime.
#[cfg(not(target_arch = "wasm32"))]


#[path = "../../src/browser_session.rs"]
pub mod browser_session;
#[path = "../../../../crates/ui/src/state/connection.rs"]
pub mod engine_connection;
#[path = "../../src/rpc/connection.rs"]
pub mod browser_connection;
#[cfg(not(target_arch = "wasm32"))]

#[path = "../../src/rpc.rs"]
pub mod rpc;
#[cfg(not(target_arch = "wasm32"))]

#[path = "../../src/session.rs"]
pub mod session;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod connection_tests;

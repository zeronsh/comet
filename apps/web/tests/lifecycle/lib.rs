//! Compiles the real browser modules without GPUI or a native engine runtime.
#[path = "../../src/browser_session.rs"]
pub mod browser_session;
#[path = "../../../../crates/ui/src/state/connection.rs"]
pub mod engine_connection;
#[path = "../../src/rpc/connection.rs"]
pub mod browser_connection;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod connection_tests;

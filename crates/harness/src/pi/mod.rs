//! Pi harness support: model catalog and a native Pi driver that talks
//! to `pi --mode rpc` directly instead of through the `pi-acp` ACP adapter.
//!
//! The native driver mirrors bb's approach of using Pi's SDK — we drive
//! pi's RPC mode (the same protocol the SDK uses) directly from Rust.

pub mod catalog;
pub mod native;

pub use native::PiNativeHarness;

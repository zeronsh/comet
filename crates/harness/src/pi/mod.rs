//! Native Pi harness over the documented `pi --mode rpc` JSONL protocol.

// Retained only for the legacy `AcpHarness::pi` compatibility constructor.
pub mod catalog;
pub mod native;

pub use native::PiNativeHarness;

//! Pi harness support: model catalog, session paths, and (eventually)
//! a native Pi driver that talks to `pi --mode rpc` directly instead of
//! through the `pi-acp` ACP adapter.
//!
//! Today only the catalog is ported — the ACP adapter still drives runs.
//! The catalog reads `models-store.json` from the Pi agent data dir so the
//! picker reflects the user's configured providers and extensions without
//! spawning a process.

pub mod catalog;

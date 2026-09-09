//! Project-scoped HTTP discovery, stable local routing, and authenticated peers.
//! Application bytes use bounded multiplexed streams; edge signaling never
//! transports preview requests or response bodies.
pub mod catalog;
pub mod discovery;
pub mod mux;
pub mod peer;
pub mod proxy;
pub mod signaling;

pub mod service;
pub use service::PreviewService;

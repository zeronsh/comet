//! The authenticated device channel over the relay (RFC 0001 §10, plan ES-11).
//!
//! The relay DO only routes bytes. For an enrolled profile every RPC byte
//! that crosses it is wrapped by a Noise XX session between the two devices'
//! vault identities ([`zeron_crypto::channel`]); the relay, the edge, and
//! anyone holding the edge's storage see handshake and ciphertext only.
//!
//! Wire shape, on top of the ordinary `{s, k, to?, from?}` frame header:
//! - kind [`CHANNEL_KIND`] for every channel frame;
//! - stream id [`CHANNEL_HS1`] / [`CHANNEL_HS2`] / [`CHANNEL_HS3`] carries the
//!   three Noise handshake messages (client → host, host → client, client →
//!   host);
//! - stream id [`CHANNEL_DATA`] carries one sealed RPC line per frame;
//! - stream id [`CHANNEL_ERROR`] (host → client) carries `{"error": code}` and
//!   ends the client's link: [`CHANNEL_REQUIRED`] when a plaintext frame
//!   reached an enrolled host, [`CHANNEL_REJECTED`] when the handshake or a
//!   sealed frame failed verification or the peer is not an active member.
//!
//! Who decides membership is the [`ChannelAuthority`]: the engine implements
//! it over its vault (device id, X25519 static, vault + generation scope,
//! and the active-member lookup). The rpc crate never sees key material
//! beyond the [`ChannelIdentity`] it is handed for one handshake.

use std::sync::{Arc, Mutex};

use serde::Deserialize;
use zeron_crypto::channel::{Channel, ChannelIdentity, ChannelScope, PeerIdentity};

use crate::{RpcError, RpcService};

pub const CHANNEL_KIND: &str = "chan";
pub const CHANNEL_HS1: &str = "hs1";
pub const CHANNEL_HS2: &str = "hs2";
pub const CHANNEL_HS3: &str = "hs3";
pub const CHANNEL_DATA: &str = "rpc";
pub const CHANNEL_ERROR: &str = "err";

/// Error codes on [`CHANNEL_ERROR`] frames.
pub const CHANNEL_REQUIRED: &str = "encrypted_channel_required";
pub const CHANNEL_REJECTED: &str = "channel_rejected";
pub const CHANNEL_UNSUPPORTED: &str = "channel_unsupported";

/// This device's side of a channel: identity plus the vault scope both
/// peers must share (prologue material).
pub struct ChannelLocal {
    pub identity: ChannelIdentity,
    pub scope: ChannelScope,
}

/// Membership authority for the device channel — implemented by the engine
/// over its vault. Every method is called synchronously and must not block.
pub trait ChannelAuthority: Send + Sync + 'static {
    /// True once this profile is enrolled in a vault: from then on the host
    /// refuses plaintext relay frames and the client dials only through the
    /// channel. Never falls back.
    fn required(&self) -> bool;

    /// This device's channel identity and scope, or a human-readable reason
    /// the channel cannot be established right now (locked store, not an
    /// active member, verification failure).
    fn local(&self) -> Result<ChannelLocal, String>;

    /// The membership check on a handshake peer: true only for an ACTIVE
    /// member of the same vault whose published encryption key is
    /// `peer.static_key` and whose device id is `peer.device_id`, and which
    /// is not this device itself. Re-checked on every inbound sealed frame
    /// so a revocation that lands locally ends the session.
    fn accept(&self, peer: &PeerIdentity) -> bool;
}

/// The host relay's channel configuration: the authority and the service
/// served to channel-authenticated peers (the plaintext service stays
/// gated for enrolled profiles).
pub struct ChannelHost {
    pub authority: Arc<dyn ChannelAuthority>,
    pub service: Arc<dyn RpcService>,
}

/// An established channel shared between the inbound loop and the outbound
/// pump. Sealing and opening are short synchronous operations.
pub(crate) type SharedChannel = Arc<Mutex<Channel>>;

pub(crate) fn shared(channel: Channel) -> SharedChannel {
    Arc::new(Mutex::new(channel))
}

pub(crate) fn seal(channel: &SharedChannel, text: &str) -> Result<Vec<u8>, RpcError> {
    channel
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .seal(text.as_bytes())
        .map_err(|e| RpcError::Transport(format!("device channel seal: {e}")))
}

pub(crate) fn open(channel: &SharedChannel, sealed: &[u8]) -> Result<String, RpcError> {
    let bytes = channel
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .open(sealed)
        .map_err(|e| RpcError::Transport(format!("device channel open: {e}")))?;
    String::from_utf8(bytes)
        .map_err(|_| RpcError::Transport("device channel: non-UTF-8 RPC frame".into()))
}

pub(crate) fn peer_of(channel: &SharedChannel) -> PeerIdentity {
    *channel
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .peer()
}

pub(crate) fn error_payload(code: &str) -> Vec<u8> {
    serde_json::json!({ "error": code })
        .to_string()
        .into_bytes()
}

pub(crate) fn error_code(payload: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Code {
        error: String,
    }
    serde_json::from_slice::<Code>(payload)
        .map(|c| c.error)
        .unwrap_or_else(|_| "channel error".into())
}

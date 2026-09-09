//! Authenticated device channel (RFC 0001 §10; plan G1): one fixed Noise
//! profile, `Noise_XX_25519_AESGCM_SHA256`, run over the untrusted device
//! relay. Both peers authenticate with their vault X25519 identity (the same
//! static key membership publishes as each device's encryption key) and are
//! ACCEPTED only when the caller's membership lookup says that static key
//! belongs to an active member whose device id matches the id sent inside
//! the handshake. The relay's connection ids are routing, never identity.
//!
//! The prologue binds the vault and storage generation so a transcript from
//! another vault cannot be replayed here. After the handshake the
//! [`Channel`] wraps application bytes with directional keys, Noise's
//! per-direction nonces, chunking for messages beyond Noise's 64 KiB limit,
//! and an explicit message budget after which the session must be
//! re-established (no unbounded key use).
//!
//! What this does NOT decide: which RPC methods a peer may call, or that the
//! peer is the device the relay was dialed by — every approved member is a
//! full-trust peer in v1 (RFC D3), so mutual membership is the authorization.

use crate::CryptoError;
use std::fmt;
use zeroize::Zeroizing;

const PATTERN: &str = "Noise_XX_25519_AESGCM_SHA256";
const PROLOGUE_DOMAIN: &[u8] = b"zeron/device-channel/v1\0";
/// Noise's hard cap per message; chunks stay below it.
const NOISE_MAX: usize = 65535;
const TAG_LEN: usize = 16;
/// Plaintext bytes per chunk: Noise max minus the tag minus the 1-byte
/// continuation flag.
const CHUNK_PLAINTEXT: usize = NOISE_MAX - TAG_LEN - 1;
/// Messages per direction before the session is retired (RFC §10 "explicit
/// rekey limits"): far below Noise's nonce space, comfortably above any
/// realistic remote-control session.
pub const MAX_MESSAGES_PER_DIRECTION: u64 = 1 << 32;
/// Application frame cap (a large file chunk is ~1 MiB base64).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelError {
    Crypto(CryptoError),
    /// The Noise library refused the input (malformed, wrong order, or
    /// authentication failure of a handshake/transport message).
    Handshake,
    /// The peer's static key or device id is not an active member.
    PeerRejected,
    /// A handshake message arrived out of sequence.
    WrongState,
    /// Frame or chunk limits exceeded.
    SizeLimitExceeded,
    /// The per-direction message budget is spent; reconnect.
    Exhausted,
    /// Chunked frame reassembly failed (truncated / trailing bytes).
    Malformed,
}

impl fmt::Display for ChannelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ChannelError {}
impl From<CryptoError> for ChannelError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}
impl From<snow::Error> for ChannelError {
    fn from(_: snow::Error) -> Self {
        Self::Handshake
    }
}

/// This device's channel identity: its vault device id and X25519 static.
pub struct ChannelIdentity {
    device_id: [u8; 16],
    static_key: Zeroizing<[u8; 32]>,
}

impl ChannelIdentity {
    pub fn new(device_id: [u8; 16], static_key: &[u8]) -> Result<Self, ChannelError> {
        let key: [u8; 32] = static_key
            .try_into()
            .map_err(|_| ChannelError::Crypto(CryptoError::InvalidKeyLength))?;
        Ok(Self {
            device_id,
            static_key: Zeroizing::new(key),
        })
    }

    pub fn device_id(&self) -> &[u8; 16] {
        &self.device_id
    }

    /// The X25519 public key membership lists for this device.
    pub fn public_key(&self) -> [u8; 32] {
        crate::hpke::HpkePrivateKey::from_bytes(self.static_key.as_ref())
            .map(|key| *key.public_key().as_bytes())
            .unwrap_or([0; 32])
    }
}

impl fmt::Debug for ChannelIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChannelIdentity([REDACTED])")
    }
}

/// The vault scope both peers must share (prologue material).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelScope {
    pub vault_id: [u8; 16],
    pub generation: [u8; 16],
}

impl ChannelScope {
    fn prologue(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PROLOGUE_DOMAIN.len() + 32);
        out.extend_from_slice(PROLOGUE_DOMAIN);
        out.extend_from_slice(&self.vault_id);
        out.extend_from_slice(&self.generation);
        out
    }
}

/// Which side of the handshake this device plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

/// The peer this side authenticated: its device id (from the encrypted
/// handshake payload) and static key (from the Noise transcript). The
/// caller MUST check both against verified membership before trusting the
/// channel (see [`Handshake::finish`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pub device_id: [u8; 16],
    pub static_key: [u8; 32],
}

/// An in-progress Noise XX handshake.
pub struct Handshake {
    state: snow::HandshakeState,
    role: Role,
    local_device_id: [u8; 16],
    peer: Option<PeerIdentity>,
    step: u8,
}

impl fmt::Debug for Handshake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Handshake({:?}, step {})", self.role, self.step)
    }
}

fn build(
    identity: &ChannelIdentity,
    scope: &ChannelScope,
    role: Role,
) -> Result<snow::HandshakeState, ChannelError> {
    let params: snow::params::NoiseParams = PATTERN.parse().map_err(|_| ChannelError::Handshake)?;
    // snow borrows the prologue and private key only until `build_*`
    // consumes the builder, so both live on this frame.
    let prologue = scope.prologue();
    let builder = snow::Builder::new(params)
        .local_private_key(identity.static_key.as_ref())?
        .prologue(&prologue)?;
    Ok(match role {
        Role::Initiator => builder.build_initiator()?,
        Role::Responder => builder.build_responder()?,
    })
}

impl Handshake {
    /// Start as the initiator; returns the first message (`-> e`).
    pub fn initiate(
        identity: &ChannelIdentity,
        scope: &ChannelScope,
    ) -> Result<(Self, Vec<u8>), ChannelError> {
        let mut state = build(identity, scope, Role::Initiator)?;
        let mut buffer = vec![0u8; NOISE_MAX];
        let length = state.write_message(&[], &mut buffer)?;
        buffer.truncate(length);
        Ok((
            Self {
                state,
                role: Role::Initiator,
                local_device_id: identity.device_id,
                peer: None,
                step: 1,
            },
            buffer,
        ))
    }

    /// Start as the responder with the initiator's first message; returns
    /// the second message (`<- e, ee, s, es` carrying our device id).
    pub fn respond(
        identity: &ChannelIdentity,
        scope: &ChannelScope,
        first: &[u8],
    ) -> Result<(Self, Vec<u8>), ChannelError> {
        if first.len() > NOISE_MAX {
            return Err(ChannelError::SizeLimitExceeded);
        }
        let mut state = build(identity, scope, Role::Responder)?;
        let mut payload = vec![0u8; NOISE_MAX];
        let read = state.read_message(first, &mut payload)?;
        if read != 0 {
            return Err(ChannelError::Handshake);
        }
        let mut buffer = vec![0u8; NOISE_MAX];
        let length = state.write_message(&identity.device_id, &mut buffer)?;
        buffer.truncate(length);
        Ok((
            Self {
                state,
                role: Role::Responder,
                local_device_id: identity.device_id,
                peer: None,
                step: 2,
            },
            buffer,
        ))
    }

    /// Initiator: consume the responder's message; returns the third
    /// message (`-> s, se` carrying our device id) and the responder's
    /// identity for the membership check.
    pub fn initiator_step(
        &mut self,
        second: &[u8],
    ) -> Result<(Vec<u8>, PeerIdentity), ChannelError> {
        if self.role != Role::Initiator || self.step != 1 {
            return Err(ChannelError::WrongState);
        }
        if second.len() > NOISE_MAX {
            return Err(ChannelError::SizeLimitExceeded);
        }
        let mut payload = vec![0u8; NOISE_MAX];
        let read = self.state.read_message(second, &mut payload)?;
        let device_id: [u8; 16] = payload[..read]
            .try_into()
            .map_err(|_| ChannelError::Handshake)?;
        let static_key: [u8; 32] = self
            .state
            .get_remote_static()
            .and_then(|key| key.try_into().ok())
            .ok_or(ChannelError::Handshake)?;
        let mut buffer = vec![0u8; NOISE_MAX];
        let length = self
            .state
            .write_message(&self.local_device_id, &mut buffer)?;
        buffer.truncate(length);
        let peer = PeerIdentity {
            device_id,
            static_key,
        };
        self.peer = Some(peer);
        self.step = 3;
        Ok((buffer, peer))
    }

    /// Responder: consume the initiator's third message; returns the
    /// initiator's identity for the membership check.
    pub fn responder_step(&mut self, third: &[u8]) -> Result<PeerIdentity, ChannelError> {
        if self.role != Role::Responder || self.step != 2 {
            return Err(ChannelError::WrongState);
        }
        if third.len() > NOISE_MAX {
            return Err(ChannelError::SizeLimitExceeded);
        }
        let mut payload = vec![0u8; NOISE_MAX];
        let read = self.state.read_message(third, &mut payload)?;
        let device_id: [u8; 16] = payload[..read]
            .try_into()
            .map_err(|_| ChannelError::Handshake)?;
        let static_key: [u8; 32] = self
            .state
            .get_remote_static()
            .and_then(|key| key.try_into().ok())
            .ok_or(ChannelError::Handshake)?;
        let peer = PeerIdentity {
            device_id,
            static_key,
        };
        self.peer = Some(peer);
        self.step = 3;
        Ok(peer)
    }

    /// The authenticated peer (once the transcript has revealed it).
    pub fn peer(&self) -> Option<PeerIdentity> {
        self.peer
    }

    /// Complete the handshake. `accept` is the caller's membership check on
    /// the peer identity — return `false` for anything but an ACTIVE member
    /// whose published encryption key equals `static_key` and whose device
    /// id equals `device_id`. A rejected peer yields no channel.
    pub fn finish(
        self,
        accept: impl FnOnce(&PeerIdentity) -> bool,
    ) -> Result<Channel, ChannelError> {
        if !self.state.is_handshake_finished() || self.step != 3 {
            return Err(ChannelError::WrongState);
        }
        let peer = self.peer.ok_or(ChannelError::WrongState)?;
        if !accept(&peer) {
            return Err(ChannelError::PeerRejected);
        }
        let transport = self.state.into_transport_mode()?;
        Ok(Channel {
            transport,
            peer,
            sent: 0,
            received: 0,
        })
    }
}

/// An established channel: encrypt/decrypt application frames.
pub struct Channel {
    transport: snow::TransportState,
    peer: PeerIdentity,
    sent: u64,
    received: u64,
}

impl fmt::Debug for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Channel(sent {}, received {})",
            self.sent, self.received
        )
    }
}

impl Channel {
    pub fn peer(&self) -> &PeerIdentity {
        &self.peer
    }

    pub fn sent(&self) -> u64 {
        self.sent
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    /// Seal one application frame. Output: a sequence of Noise messages,
    /// each `u16 BE length || ciphertext`; the first plaintext byte of every
    /// chunk is a continuation flag (1 = more chunks follow).
    pub fn seal(&mut self, frame: &[u8]) -> Result<Vec<u8>, ChannelError> {
        if frame.len() > MAX_FRAME_BYTES {
            return Err(ChannelError::SizeLimitExceeded);
        }
        let chunks = frame.chunks(CHUNK_PLAINTEXT).count().max(1) as u64;
        if self.sent.saturating_add(chunks) > MAX_MESSAGES_PER_DIRECTION {
            return Err(ChannelError::Exhausted);
        }
        let mut out = Vec::with_capacity(frame.len() + 64);
        let mut plaintext = Zeroizing::new(vec![0u8; NOISE_MAX]);
        let mut buffer = vec![0u8; NOISE_MAX];
        let mut pieces: Vec<&[u8]> = frame.chunks(CHUNK_PLAINTEXT).collect();
        if pieces.is_empty() {
            pieces.push(&[]);
        }
        let last = pieces.len() - 1;
        for (index, piece) in pieces.iter().enumerate() {
            plaintext[0] = u8::from(index != last);
            plaintext[1..1 + piece.len()].copy_from_slice(piece);
            let length = self
                .transport
                .write_message(&plaintext[..1 + piece.len()], &mut buffer)?;
            self.sent += 1;
            out.extend_from_slice(&(length as u16).to_be_bytes());
            out.extend_from_slice(&buffer[..length]);
        }
        Ok(out)
    }

    /// Open one sealed frame produced by the peer's [`Self::seal`].
    pub fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>, ChannelError> {
        if sealed.len() > MAX_FRAME_BYTES + (MAX_FRAME_BYTES / CHUNK_PLAINTEXT + 1) * (TAG_LEN + 3)
        {
            return Err(ChannelError::SizeLimitExceeded);
        }
        let mut out = Vec::new();
        let mut cursor = 0usize;
        let mut buffer = vec![0u8; NOISE_MAX];
        loop {
            if sealed.len() < cursor + 2 {
                return Err(ChannelError::Malformed);
            }
            let length = u16::from_be_bytes([sealed[cursor], sealed[cursor + 1]]) as usize;
            cursor += 2;
            if length < TAG_LEN + 1 || sealed.len() < cursor + length {
                return Err(ChannelError::Malformed);
            }
            if self.received >= MAX_MESSAGES_PER_DIRECTION {
                return Err(ChannelError::Exhausted);
            }
            let read = self
                .transport
                .read_message(&sealed[cursor..cursor + length], &mut buffer)?;
            self.received += 1;
            cursor += length;
            if read == 0 {
                return Err(ChannelError::Malformed);
            }
            out.extend_from_slice(&buffer[1..read]);
            if out.len() > MAX_FRAME_BYTES {
                return Err(ChannelError::SizeLimitExceeded);
            }
            match buffer[0] {
                0 => break,
                1 => continue,
                _ => return Err(ChannelError::Malformed),
            }
        }
        if cursor != sealed.len() {
            return Err(ChannelError::Malformed);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(tag: u8) -> ChannelIdentity {
        ChannelIdentity::new([tag; 16], &[tag ^ 0x5a; 32]).unwrap()
    }

    fn scope() -> ChannelScope {
        ChannelScope {
            vault_id: [1; 16],
            generation: [2; 16],
        }
    }

    fn establish(
        initiator: &ChannelIdentity,
        responder: &ChannelIdentity,
        scope_a: &ChannelScope,
        scope_b: &ChannelScope,
    ) -> Result<(Channel, Channel), ChannelError> {
        let (mut a, m1) = Handshake::initiate(initiator, scope_a)?;
        let (mut b, m2) = Handshake::respond(responder, scope_b, &m1)?;
        let (m3, responder_seen) = a.initiator_step(&m2)?;
        assert_eq!(responder_seen.device_id, *responder.device_id());
        assert_eq!(responder_seen.static_key, responder.public_key());
        let initiator_seen = b.responder_step(&m3)?;
        assert_eq!(initiator_seen.device_id, *initiator.device_id());
        assert_eq!(initiator_seen.static_key, initiator.public_key());
        let responder_public = responder.public_key();
        let initiator_public = initiator.public_key();
        let channel_a = a.finish(|peer| peer.static_key == responder_public)?;
        let channel_b = b.finish(|peer| peer.static_key == initiator_public)?;
        Ok((channel_a, channel_b))
    }

    #[test]
    fn handshake_authenticates_both_statics_and_carries_frames_both_ways() {
        let (mut a, mut b) =
            establish(&identity(0x11), &identity(0x22), &scope(), &scope()).unwrap();
        let sealed = a.seal(b"hello from the phone").unwrap();
        assert!(!sealed.windows(5).any(|w| w == b"hello"));
        assert_eq!(b.open(&sealed).unwrap(), b"hello from the phone");
        let reply = b.seal(b"").unwrap();
        assert_eq!(a.open(&reply).unwrap(), b"");
        // Replay and tampering are rejected; the channel stays usable.
        assert!(b.open(&sealed).is_err());
        let mut damaged = a.seal(b"x").unwrap();
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert!(b.open(&damaged).is_err());
        assert_eq!(a.sent(), 2);
        assert_eq!(
            b.received(),
            1,
            "a rejected frame is not counted as received"
        );
    }

    #[test]
    fn large_frames_chunk_and_reassemble_in_order() {
        let (mut a, mut b) =
            establish(&identity(0x11), &identity(0x22), &scope(), &scope()).unwrap();
        let big: Vec<u8> = (0..(3 * NOISE_MAX + 17)).map(|i| (i % 251) as u8).collect();
        let sealed = a.seal(&big).unwrap();
        assert!(sealed.len() > big.len());
        assert_eq!(b.open(&sealed).unwrap(), big);
        // Truncated / reordered chunk streams fail closed.
        assert!(b.open(&sealed[..sealed.len() - 5]).is_err());
        assert!(matches!(
            a.seal(&vec![0u8; MAX_FRAME_BYTES + 1]),
            Err(ChannelError::SizeLimitExceeded)
        ));
    }

    #[test]
    fn membership_check_and_prologue_gate_the_channel() {
        let initiator = identity(0x11);
        let responder = identity(0x22);
        // A responder the initiator's membership does not list: no channel.
        let (mut a, m1) = Handshake::initiate(&initiator, &scope()).unwrap();
        let (mut b, m2) = Handshake::respond(&responder, &scope(), &m1).unwrap();
        let (m3, _) = a.initiator_step(&m2).unwrap();
        b.responder_step(&m3).unwrap();
        assert!(matches!(
            a.finish(|_| false),
            Err(ChannelError::PeerRejected)
        ));
        assert!(b.finish(|_| true).is_ok());
        // Different vault scope (prologue) breaks the transcript.
        let other = ChannelScope {
            vault_id: [9; 16],
            generation: [2; 16],
        };
        assert!(establish(&identity(0x11), &identity(0x22), &scope(), &other).is_err());
        // Out-of-order steps are refused.
        let (mut a, _) = Handshake::initiate(&initiator, &scope()).unwrap();
        assert!(matches!(
            a.responder_step(&[0; 48]),
            Err(ChannelError::WrongState)
        ));
        assert!(matches!(
            a.initiator_step(&[0; 10]),
            Err(ChannelError::Handshake)
        ));
        assert_eq!(format!("{initiator:?}"), "ChannelIdentity([REDACTED])");
    }
}

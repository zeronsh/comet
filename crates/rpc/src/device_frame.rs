//! Portable DeviceRoom binary framing shared by native relays and browser clients.
//!
//! A frame is `uleb128(header_len) || UTF-8 JSON header || payload`. The
//! Worker routes using `to`/`from`; a browser client sends only `{s,k}`.

use serde::{Deserialize, Serialize};

use crate::RpcError;

/// Relay control frames have a leading space for parity with the Worker.
pub const RELAY_KIND: &str = " relay";
pub const RPC_KIND: &str = "rpc";
/// End-to-end proof that the relay can reach the remote backend host.
pub const ECHO_KIND: &str = "echo";
/// Durable Object hibernation-safe text keepalive request and response.
pub const PING_TEXT: &str = "ping";
pub const PONG_TEXT: &str = "pong";
/// The native and browser transports send both keepalive forms at this cadence.
pub const PING_INTERVAL_MS: i32 = 10_000;
/// Client-to-edge half-open transport deadline.
pub const SILENCE_LEASE_MS: i32 = 25_000;
/// Edge pongs alone do not prove the edge-to-host leg; require host echo after one succeeds.
pub const ECHO_DEADLINE_MS: i32 = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFrameHeader {
    pub s: String,
    pub k: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

impl DeviceFrameHeader {
    pub fn new(s: impl Into<String>, k: impl Into<String>) -> Self {
        Self {
            s: s.into(),
            k: k.into(),
            to: None,
            from: None,
        }
    }

    pub fn with_to(mut self, conn_id: impl Into<String>) -> Self {
        self.to = Some(conn_id.into());
        self
    }
}

pub fn encode_device_frame(
    header: &DeviceFrameHeader,
    payload: &[u8],
) -> Result<Vec<u8>, RpcError> {
    let json = serde_json::to_vec(header)
        .map_err(|error| RpcError::Transport(format!("encode frame header: {error}")))?;
    let mut output = Vec::with_capacity(json.len() + payload.len() + 5);
    let mut length = json.len();
    loop {
        let mut byte = (length & 0x7f) as u8;
        length >>= 7;
        if length != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if length == 0 {
            break;
        }
    }
    output.extend_from_slice(&json);
    output.extend_from_slice(payload);
    Ok(output)
}

pub fn decode_device_frame(bytes: &[u8]) -> Result<(DeviceFrameHeader, Vec<u8>), RpcError> {
    let bad = |message: &str| RpcError::Transport(format!("device frame: {message}"));
    let mut offset = 0usize;
    let mut length = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(offset).ok_or_else(|| bad("truncated uleb128"))?;
        offset += 1;
        if shift >= 32 || (shift == 28 && byte & 0x70 != 0) {
            return Err(bad("uleb128 overflow"));
        }
        length |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    let end = offset
        .checked_add(length)
        .ok_or_else(|| bad("header length overflow"))?;
    let header: DeviceFrameHeader = serde_json::from_slice(
        bytes
            .get(offset..end)
            .ok_or_else(|| bad("truncated header"))?,
    )
    .map_err(|error| bad(&format!("bad header JSON: {error}")))?;
    Ok((header, bytes[end..].to_vec()))
}

pub fn relay_error_code(payload: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct RelayError {
        error: String,
    }
    serde_json::from_slice::<RelayError>(payload)
        .ok()
        .map(|error| error.error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_client_frame_has_no_routing_and_round_trips() {
        let header = DeviceFrameHeader::new(RPC_KIND, RPC_KIND);
        let bytes = encode_device_frame(&header, b"{\"id\":1}").unwrap();
        assert_eq!(bytes[0], 21); // `{"s":"rpc","k":"rpc"}`
        assert_eq!(
            decode_device_frame(&bytes).unwrap(),
            (header, b"{\"id\":1}".to_vec())
        );
    }

    #[test]
    fn golden_ts_wire_vectors_are_byte_identical() {
        let frame = encode_device_frame(&DeviceFrameHeader::new("a", RPC_KIND), &[1, 2]).unwrap();
        assert_eq!(frame, b"\x13{\"s\":\"a\",\"k\":\"rpc\"}\x01\x02");
        let routed =
            encode_device_frame(&DeviceFrameHeader::new("s1", "term").with_to("c9"), b"x").unwrap();
        assert_eq!(routed, b"\x1f{\"s\":\"s1\",\"k\":\"term\",\"to\":\"c9\"}x");
    }

    #[test]
    fn malformed_frames_fail_closed() {
        assert!(decode_device_frame(&[]).is_err());
        assert!(decode_device_frame(&[0x80, 0x80, 0x80, 0x80, 0x80]).is_err());
        assert!(decode_device_frame(&[0x95, 0x80, 0x80, 0x80, 0x10]).is_err());
        assert!(decode_device_frame(&[3, b'{', b'}']).is_err());
    }
}

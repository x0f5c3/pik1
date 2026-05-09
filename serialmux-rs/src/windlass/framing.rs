//! Tunnel-link framing codecs.
//!
//! Three codecs are provided:
//!
//! * [`TunnelCodec`] — encodes / decodes the **CDC tunnel** format:
//!   `[ch_id:u8][raw Klipper frame]`.  Used on both exporter and host sides
//!   when reading from / writing to the USB CDC ACM link.
//!
//! * [`PayloadTunnelCodec`] — encodes / decodes the **smart-proxy tunnel**
//!   format: `[ch_id:u8][payload_len:u8][payload:payload_len bytes]`.  Used
//!   in smart-proxy mode where the Klipper wire protocol is terminated at each
//!   end and only the decoded payload bytes cross the CDC link.
//!
//! * [`KlipperFramer`] — decodes a **raw Klipper serial stream** into
//!   individual complete Klipper frames (5–64 bytes, sync-byte terminated).
//!   Used by the exporter when reading from a UART and by the host when
//!   reading commands back from Klipper via the Unix socket.

use bytes::{Buf, BufMut, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

// ─────────────────────────────────────────────────────────────────────────────
// Klipper wire-protocol constants
// ─────────────────────────────────────────────────────────────────────────────

/// Minimum Klipper frame size: 2 header bytes + 3 trailer bytes.
pub const MESSAGE_LENGTH_MIN: usize = 5;
/// Maximum Klipper frame size (Klipper protocol hard limit).
pub const MESSAGE_LENGTH_MAX: usize = 64;
/// Klipper sync / end-of-frame byte.
pub const MESSAGE_VALUE_SYNC: u8 = 0x7E;

// ─────────────────────────────────────────────────────────────────────────────
// TunnelFrame
// ─────────────────────────────────────────────────────────────────────────────

/// A single tunnel packet: one complete Klipper frame tagged with a channel ID.
#[derive(Debug, Clone)]
pub struct TunnelFrame {
    /// Channel index (0–255).
    pub ch_id: u8,
    /// Complete raw Klipper frame (length byte at `[0]`, sync byte at `[end]`).
    pub frame: bytes::Bytes,
}

// ─────────────────────────────────────────────────────────────────────────────
// TunnelCodec
// ─────────────────────────────────────────────────────────────────────────────

/// `tokio_util` codec for the CDC tunnel link.
///
/// ## Wire layout
///
/// ```text
/// [ ch_id : u8 ][ klipper_frame : frame_len bytes ]
/// ```
///
/// `klipper_frame[0]` is the Klipper frame length (5–64).  The codec reads
/// that byte first to know how many bytes constitute the complete frame, so
/// it never needs a separate length prefix on the tunnel wire.
///
/// ## Resync
///
/// If `klipper_frame[0]` is outside the valid range (5–64), the codec
/// discards the `ch_id` byte and re-tries from the next byte.  The
/// Klipper sync byte (`0x7E`) at the end of every frame guarantees that
/// any corruption is quickly redetected.
#[derive(Default)]
pub struct TunnelCodec;

impl Decoder for TunnelCodec {
    type Item = TunnelFrame;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            // Need at least ch_id (1) + Klipper frame length byte (1).
            if src.len() < 2 {
                return Ok(None);
            }

            let ch_id = src[0];
            let frame_len = src[1] as usize;

            if frame_len < MESSAGE_LENGTH_MIN || frame_len > MESSAGE_LENGTH_MAX {
                // Bad length — skip the ch_id byte and re-try.
                src.advance(1);
                continue;
            }

            // Need ch_id (1) + full Klipper frame.
            let total = 1 + frame_len;
            if src.len() < total {
                src.reserve(total - src.len());
                return Ok(None);
            }

            if src[total - 1] != MESSAGE_VALUE_SYNC {
                // Bad trailing sync byte — skip one byte and re-try.
                src.advance(1);
                continue;
            }

            src.advance(1); // consume ch_id
            let frame = src.split_to(frame_len).freeze();
            return Ok(Some(TunnelFrame { ch_id, frame }));
        }
    }
}

impl Encoder<TunnelFrame> for TunnelCodec {
    type Error = io::Error;

    fn encode(&mut self, item: TunnelFrame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.reserve(1 + item.frame.len());
        dst.put_u8(item.ch_id);
        dst.extend_from_slice(&item.frame);
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PayloadTunnelFrame
// ─────────────────────────────────────────────────────────────────────────────

/// Channel IDs reserved for the smart-proxy control channel.
pub const CTRL_CH: u8 = 0xFF;
/// Control payload type: dictionary fragment bytes.
pub const CTRL_DICT_FRAG: u8 = 0x01;
/// Control payload type: dictionary transfer complete (no additional bytes).
pub const CTRL_DICT_DONE: u8 = 0x02;
/// Maximum dictionary bytes per [`CTRL_DICT_FRAG`] payload.
///
/// A `DICT_FRAG` control payload is `[ type:1 ][ ch_id:1 ][ dict_bytes:n ]`.
/// The outer [`PayloadTunnelCodec`] limits the total payload to 255 bytes, so
/// `n ≤ 255 − 2 = 253`.
pub const DICT_FRAG_MAX: usize = 253;

/// A smart-proxy tunnel packet: decoded Klipper payload bytes tagged with a
/// channel ID.
///
/// Wire format: `[ch_id:u8][payload_len:u8][payload:payload_len bytes]`
///
/// `payload` is the inner Klipper message — the variable-length content that
/// sits between the two-byte frame header and the three-byte trailer in the
/// on-wire Klipper format.  The CDC link carries *only* these bytes; no CRC,
/// sequence number, or sync byte is present.
///
/// `ch_id = 0xFF` ([`CTRL_CH`]) is reserved for the control channel (dictionary
/// forwarding and other future control messages).
#[derive(Debug, Clone)]
pub struct PayloadTunnelFrame {
    /// Channel index (0–254 for MCU data; 255 = control).
    pub ch_id: u8,
    /// Decoded Klipper payload (0–255 bytes).
    pub payload: bytes::Bytes,
}

// ─────────────────────────────────────────────────────────────────────────────
// PayloadTunnelCodec
// ─────────────────────────────────────────────────────────────────────────────

/// `tokio_util` codec for the smart-proxy CDC tunnel link.
///
/// ## Wire layout
///
/// ```text
/// [ ch_id : u8 ][ payload_len : u8 ][ payload : payload_len bytes ]
/// ```
///
/// `payload` is the decoded Klipper message bytes — no Klipper wire framing
/// (no length byte, no seq byte, no CRC, no sync byte).  Klipper's transport
/// sessions are terminated at each end of the CDC link, so the link only
/// carries clean application payloads.
///
/// A zero-length payload is valid (used e.g. for keep-alive or as a DICT_DONE
/// control message where the type byte alone constitutes the payload).
#[derive(Default)]
pub struct PayloadTunnelCodec;

impl Decoder for PayloadTunnelCodec {
    type Item = PayloadTunnelFrame;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Need at least ch_id (1) + payload_len (1).
        if src.len() < 2 {
            return Ok(None);
        }
        let ch_id = src[0];
        let payload_len = src[1] as usize;
        let total = 2 + payload_len;
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(None);
        }
        src.advance(2); // consume ch_id + payload_len
        let payload = src.split_to(payload_len).freeze();
        Ok(Some(PayloadTunnelFrame { ch_id, payload }))
    }
}

impl Encoder<PayloadTunnelFrame> for PayloadTunnelCodec {
    type Error = io::Error;

    fn encode(&mut self, item: PayloadTunnelFrame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let len = item.payload.len();
        if len > 255 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "smart-proxy payload {} bytes exceeds 255-byte limit",
                    len
                ),
            ));
        }
        dst.reserve(2 + len);
        dst.put_u8(item.ch_id);
        dst.put_u8(len as u8);
        dst.extend_from_slice(&item.payload);
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// KlipperFramer
// ─────────────────────────────────────────────────────────────────────────────

/// `tokio_util` codec that splits a raw Klipper serial byte stream into
/// individual complete Klipper frames.
///
/// ## Frame structure
///
/// ```text
/// [ len : u8 ][ seq : u8 ][ payload… ][ crc_hi : u8 ][ crc_lo : u8 ][ 0x7E ]
/// ```
///
/// The `len` byte is the total frame length including header and trailer.
/// Minimum is 5, maximum is 64.
///
/// ## Sync and resync
///
/// The codec uses a simple two-phase strategy:
///
/// 1. **Unsynced**: scan for the `0x7E` sync byte; once found, transition to
///    synced state.
/// 2. **Synced**: read the first non-sync byte as the frame length; collect
///    that many bytes; validate that the last byte is `0x7E`.  On any
///    inconsistency, fall back to unsynced state.
///
/// This mirrors the resync logic in the Klipper MCU firmware and the
/// `windlass` transport crate.
pub struct KlipperFramer {
    synced: bool,
}

impl Default for KlipperFramer {
    fn default() -> Self {
        KlipperFramer { synced: false }
    }
}

impl KlipperFramer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Decoder for KlipperFramer {
    type Item = bytes::Bytes;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            if src.is_empty() {
                return Ok(None);
            }

            if !self.synced {
                // Scan for the 0x7E sync byte.
                match src.iter().position(|&b| b == MESSAGE_VALUE_SYNC) {
                    Some(pos) => {
                        src.advance(pos + 1);
                        self.synced = true;
                    }
                    None => {
                        src.clear();
                        return Ok(None);
                    }
                }
                continue;
            }

            // Skip idle sync bytes.
            if src[0] == MESSAGE_VALUE_SYNC {
                src.advance(1);
                continue;
            }

            let frame_len = src[0] as usize;
            if frame_len < MESSAGE_LENGTH_MIN || frame_len > MESSAGE_LENGTH_MAX {
                // Invalid length byte — re-sync.
                self.synced = false;
                src.advance(1);
                continue;
            }

            if src.len() < frame_len {
                src.reserve(frame_len - src.len());
                return Ok(None);
            }

            // Validate the trailing sync byte.
            if src[frame_len - 1] != MESSAGE_VALUE_SYNC {
                // Malformed frame — re-sync.
                self.synced = false;
                src.advance(1);
                continue;
            }

            let frame = src.split_to(frame_len).freeze();
            return Ok(Some(frame));
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(payload: &[u8]) -> Vec<u8> {
        // len + seq + payload + crc_hi + crc_lo + sync
        let len = 2 + payload.len() + 3;
        assert!(len >= MESSAGE_LENGTH_MIN && len <= MESSAGE_LENGTH_MAX);
        let mut f = vec![len as u8, 0x10u8]; // len + seq (dest 0x10)
        f.extend_from_slice(payload);
        // Minimal fake CRC (zeroes) for framing tests.
        f.extend_from_slice(&[0x00, 0x00, MESSAGE_VALUE_SYNC]);
        f
    }

    // ── KlipperFramer ────────────────────────────────────────────────────────

    #[test]
    fn framer_parses_single_frame_after_sync() {
        let mut src = BytesMut::new();
        src.extend_from_slice(&[MESSAGE_VALUE_SYNC]); // initial sync
        let frame = make_frame(&[0x01, 0x02]);
        src.extend_from_slice(&frame);

        let mut codec = KlipperFramer::new();
        let result = codec.decode(&mut src).unwrap();
        assert!(result.is_some());
        let got = result.unwrap();
        assert_eq!(got.as_ref(), frame.as_slice());
    }

    #[test]
    fn framer_skips_idle_syncs() {
        let mut src = BytesMut::new();
        // Three idle syncs, then a real frame.
        src.extend_from_slice(&[
            MESSAGE_VALUE_SYNC,
            MESSAGE_VALUE_SYNC,
            MESSAGE_VALUE_SYNC,
        ]);
        let frame = make_frame(&[0xAA]);
        src.extend_from_slice(&frame);

        let mut codec = KlipperFramer::new();
        // First call: consume the first sync to become synced, skip idles,
        // then parse the frame.
        let result = codec.decode(&mut src).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().as_ref(), frame.as_slice());
    }

    #[test]
    fn framer_resyncs_on_bad_length() {
        let mut src = BytesMut::new();
        src.extend_from_slice(&[MESSAGE_VALUE_SYNC]); // sync
        src.extend_from_slice(&[0x00]); // invalid length (< 5)
        // Valid frame follows.
        let frame = make_frame(&[0x05]);
        src.extend_from_slice(&[MESSAGE_VALUE_SYNC]); // resync sync
        src.extend_from_slice(&frame);

        let mut codec = KlipperFramer::new();
        // Should skip the bad byte, resync, and return the valid frame.
        let result = codec.decode(&mut src).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().as_ref(), frame.as_slice());
    }

    #[test]
    fn framer_returns_none_on_partial_frame() {
        let frame = make_frame(&[0x01, 0x02, 0x03]);
        let partial = &frame[..frame.len() - 1]; // drop the last byte
        let mut src = BytesMut::new();
        src.extend_from_slice(&[MESSAGE_VALUE_SYNC]); // sync
        src.extend_from_slice(partial);

        let mut codec = KlipperFramer::new();
        let result = codec.decode(&mut src).unwrap();
        assert!(result.is_none());
    }

    // ── TunnelCodec ──────────────────────────────────────────────────────────

    #[test]
    fn tunnel_roundtrip() {
        let frame = make_frame(&[0x11, 0x22]);
        let tf = TunnelFrame {
            ch_id: 3,
            frame: bytes::Bytes::copy_from_slice(&frame),
        };

        // Encode.
        let mut buf = BytesMut::new();
        let mut enc = TunnelCodec;
        enc.encode(tf.clone(), &mut buf).unwrap();

        // Decode.
        let mut dec = TunnelCodec;
        let got = dec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(got.ch_id, 3);
        assert_eq!(got.frame.as_ref(), frame.as_slice());
        assert!(buf.is_empty());
    }

    #[test]
    fn tunnel_skips_bad_length_byte() {
        // Build a buffer: [ch=0][bad_len=2][ch=1][valid_frame...]
        let frame = make_frame(&[0xBB]);
        let mut buf = BytesMut::new();
        buf.put_u8(0); // ch_id
        buf.put_u8(2); // length 2 < MESSAGE_LENGTH_MIN → skip
        buf.put_u8(1); // ch_id of valid frame
        buf.extend_from_slice(&frame);

        let mut dec = TunnelCodec;
        let got = dec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(got.ch_id, 1);
        assert_eq!(got.frame.as_ref(), frame.as_slice());
    }

    #[test]
    fn tunnel_returns_none_when_incomplete() {
        let frame = make_frame(&[0x01]);
        // Drop the last two bytes of the Klipper frame.
        let partial = &frame[..frame.len() - 2];
        let mut buf = BytesMut::new();
        buf.put_u8(7);
        buf.extend_from_slice(partial);

        let mut dec = TunnelCodec;
        assert!(dec.decode(&mut buf).unwrap().is_none());
    }

    // ── PayloadTunnelCodec ───────────────────────────────────────────────────

    #[test]
    fn payload_tunnel_roundtrip() {
        let payload = bytes::Bytes::from_static(&[0x01, 0x00, 0x28]);
        let pf = PayloadTunnelFrame { ch_id: 5, payload: payload.clone() };

        let mut buf = BytesMut::new();
        let mut enc = PayloadTunnelCodec;
        enc.encode(pf, &mut buf).unwrap();

        // Wire: [ch_id=5][len=3][0x01,0x00,0x28]
        assert_eq!(buf.len(), 5);
        assert_eq!(buf[0], 5);
        assert_eq!(buf[1], 3);

        let mut dec = PayloadTunnelCodec;
        let got = dec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(got.ch_id, 5);
        assert_eq!(got.payload.as_ref(), payload.as_ref());
        assert!(buf.is_empty());
    }

    #[test]
    fn payload_tunnel_zero_length_payload() {
        let pf = PayloadTunnelFrame {
            ch_id: 0xFF,
            payload: bytes::Bytes::new(),
        };
        let mut buf = BytesMut::new();
        PayloadTunnelCodec.encode(pf, &mut buf).unwrap();
        assert_eq!(buf.len(), 2); // ch_id + len=0
        assert_eq!(buf[0], 0xFF);
        assert_eq!(buf[1], 0x00);

        let got = PayloadTunnelCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(got.ch_id, 0xFF);
        assert!(got.payload.is_empty());
    }

    #[test]
    fn payload_tunnel_returns_none_when_incomplete() {
        // Only ch_id byte present, no length byte yet.
        let mut buf = BytesMut::new();
        buf.put_u8(0x02); // ch_id only
        assert!(PayloadTunnelCodec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn payload_tunnel_returns_none_when_payload_incomplete() {
        let mut buf = BytesMut::new();
        buf.put_u8(0x01); // ch_id
        buf.put_u8(10);   // claims 10 bytes
        buf.extend_from_slice(&[0xAA; 5]); // only 5 bytes present
        assert!(PayloadTunnelCodec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn payload_tunnel_rejects_oversized_payload() {
        let pf = PayloadTunnelFrame {
            ch_id: 0,
            payload: bytes::Bytes::from(vec![0u8; 256]),
        };
        let mut buf = BytesMut::new();
        assert!(PayloadTunnelCodec.encode(pf, &mut buf).is_err());
    }
}

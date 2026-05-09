//! Smart-proxy dictionary bootstrap helper.
//!
//! The low-level transport and VLQ helpers now come directly from the upstream
//! [`windlass`] crate, so this module only retains the exporter-specific
//! `identify` / `identify_response` exchange used to obtain the raw compressed
//! MCU dictionary bytes.
//!
//! Once `windlass::McuConnection::raw_dictionary_bytes()` exists upstream, this
//! helper can disappear and the smart exporter can switch to
//! `windlass::McuConnection::connect`.

use std::{io, time::Duration};

use tokio::time::timeout;
use windlass::{Transport, TransportReceiver, encode_vlq_int, parse_vlq_int};

const CMD_IDENTIFY: u32 = 1;
const CMD_IDENTIFY_RESPONSE: u32 = 0;
const IDENTIFY_CHUNK_SIZE: u32 = 40;
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(5);

fn build_identify_request(offset: u32) -> Vec<u8> {
    let mut req = Vec::new();
    encode_vlq_int(&mut req, CMD_IDENTIFY);
    encode_vlq_int(&mut req, offset);
    encode_vlq_int(&mut req, IDENTIFY_CHUNK_SIZE);
    req
}

fn parse_identify_response(payload: &[u8], expected_offset: u32) -> Option<Vec<u8>> {
    let mut data = payload;
    let cmd = parse_vlq_int(&mut data).ok()?;
    if cmd != CMD_IDENTIFY_RESPONSE {
        return None;
    }

    let offset = parse_vlq_int(&mut data).ok()?;
    let data_len = parse_vlq_int(&mut data).ok()? as usize;
    if offset != expected_offset || data.len() < data_len {
        return None;
    }

    Some(data[..data_len].to_vec())
}

/// Fetch the MCU's compressed data dictionary via the Klipper `identify`
/// exchange.
///
/// Sends `identify` requests (cmd=1, increasing offsets) via `transport` and
/// collects `identify_response` payloads (cmd=0) from `payload_rx` until the
/// full dictionary is assembled. Other payloads that arrive during the
/// exchange are ignored.
pub async fn fetch_dictionary(
    transport: &Transport,
    payload_rx: &mut TransportReceiver,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut dict = Vec::new();
    let mut offset = 0u32;

    loop {
        let req = build_identify_request(offset);
        transport.send(&req)?;

        let chunk = 'wait: loop {
            let payload = timeout(IDENTIFY_TIMEOUT, payload_rx.recv())
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timeout waiting for identify_response",
                    )
                })?
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "transport closed")
                })??;

            if let Some(chunk) = parse_identify_response(&payload, offset) {
                break 'wait chunk;
            }
        };

        let chunk_len = chunk.len() as u32;
        dict.extend_from_slice(&chunk);
        offset += chunk_len;

        if chunk_len < IDENTIFY_CHUNK_SIZE {
            return Ok(dict);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_identify_request_encodes_expected_fields() {
        let req = build_identify_request(123);
        let mut data = req.as_slice();

        assert_eq!(parse_vlq_int(&mut data).unwrap(), CMD_IDENTIFY);
        assert_eq!(parse_vlq_int(&mut data).unwrap(), 123);
        assert_eq!(parse_vlq_int(&mut data).unwrap(), IDENTIFY_CHUNK_SIZE);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_identify_response_accepts_matching_chunk() {
        let mut payload = Vec::new();
        encode_vlq_int(&mut payload, CMD_IDENTIFY_RESPONSE);
        encode_vlq_int(&mut payload, 40);
        encode_vlq_int(&mut payload, 3);
        payload.extend_from_slice(&[1, 2, 3]);

        assert_eq!(parse_identify_response(&payload, 40), Some(vec![1, 2, 3]));
    }

    #[test]
    fn parse_identify_response_rejects_wrong_offset() {
        let mut payload = Vec::new();
        encode_vlq_int(&mut payload, CMD_IDENTIFY_RESPONSE);
        encode_vlq_int(&mut payload, 41);
        encode_vlq_int(&mut payload, 1);
        payload.push(9);

        assert!(parse_identify_response(&payload, 40).is_none());
    }

    #[test]
    fn parse_identify_response_rejects_non_identify_messages() {
        let mut payload = Vec::new();
        encode_vlq_int(&mut payload, 99);
        encode_vlq_int(&mut payload, 0);

        assert!(parse_identify_response(&payload, 0).is_none());
    }
}

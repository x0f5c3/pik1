//! Smart-proxy host mode — runs on the Raspberry Pi / BTT CB1.
//!
//! Unlike the transparent-relay host ([`crate::windlass::host`]), the smart
//! host uses an [`anchor`]-based virtual MCU to terminate Klipper's transport
//! session on the Pi side.  The CDC link carries only decoded payload bytes.
//!
//! # Startup sequence per channel
//!
//! 1. Listen on the CDC control channel (`ch_id = 0xFF`) for the MCU's
//!    compressed dictionary forwarded by the exporter (DICT_FRAG / DICT_DONE
//!    control frames).
//! 2. Bind the Unix socket at the configured path.
//! 3. When the dictionary is ready, accept Klipper connections in a loop.
//!    For each connection:
//!    a. Create an `anchor::Transport<ProxyConfig>` virtual MCU.
//!    b. Enter the relay loop (single tokio task per connection):
//!       - Bytes from the Klipper socket → `transport.receive(…)`.
//!       - Pending `identify_response`s built in `dispatch` →
//!         `transport.encode_frame(…)`.
//!       - MCU response payloads from CDC → `transport.encode_frame(…)`.
//!
//! # `identify` handling
//!
//! When Klipper sends `identify` (cmd=1), `ProxyConfig::dispatch` intercepts
//! it, looks up the correct 40-byte chunk of the real MCU dictionary, builds
//! an `identify_response` payload, and pushes it into `pending_responses`.
//! After `transport.receive` returns the relay loop drains the queue via
//! `transport.encode_frame`.  All other commands are forwarded to the
//! exporter.
//!
//! # Tunnel wire format
//!
//! ```text
//! [ ch_id : u8 ][ payload_len : u8 ][ payload : payload_len bytes ]
//! ```
//!
//! Control channel (`ch_id = 0xFF`):
//! ```text
//! DICT_FRAG: [ 0xFF ][ len ][ 0x01 ][ dict_bytes… ]
//! DICT_DONE: [ 0xFF ][ 0x01 ][ 0x02 ]
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use anchor::OutputBuffer as _;
use bytes::Bytes;
use futures::SinkExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::select;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::watch;
use tokio_util::codec::FramedWrite;

use crate::windlass::McuSpec;
use crate::windlass::async_serial::open_serial;
use crate::windlass::framing::{
    CTRL_CH, CTRL_DICT_DONE, CTRL_DICT_FRAG, PayloadTunnelCodec, PayloadTunnelFrame,
};
use crate::windlass::mcu_transport::encode_vlq;
use crate::windlass::prepare_socket_path;

// ─────────────────────────────────────────────────────────────────────────────
// anchor Config implementation
// ─────────────────────────────────────────────────────────────────────────────

/// Zero-sized marker type that drives `anchor::Transport` as a proxy for the
/// real MCU.
struct ProxyConfig;

/// Context passed to [`ProxyConfig::dispatch`] on every call to
/// `anchor::Transport::receive`.
struct ProxyContext<'c> {
    /// The real MCU's compressed dictionary (used to answer `identify`).
    dictionary: &'c [u8],
    /// Sender for forwarding decoded command payloads to the exporter.
    to_exporter: &'c UnboundedSender<Vec<u8>>,
    /// Queue of `identify_response` payloads built during dispatch.
    /// Drained by the relay loop after `receive` returns.
    pending_responses: &'c mut Vec<Vec<u8>>,
}

impl anchor::ShutdownState for ProxyContext<'_> {
    fn is_shutdown(&self) -> bool {
        false
    }
}

/// `TransportOutput` that forwards anchor's framed output bytes to a tokio
/// mpsc channel connected to the Klipper socket writer task.
struct ChannelOutput(UnboundedSender<Vec<u8>>);

impl anchor::TransportOutput for ChannelOutput {
    type Output = Vec<u8>;

    fn output(&self, f: impl FnOnce(&mut Vec<u8>)) {
        let mut buf = Vec::new();
        f(&mut buf);
        if !buf.is_empty() {
            let _ = self.0.send(buf);
        }
    }
}

/// Klipper fixed command ID for `identify`.
const CMD_IDENTIFY: u16 = 1;

impl anchor::transport::Config for ProxyConfig {
    type TransportOutput = ChannelOutput;
    type Context<'c> = ProxyContext<'c>;

    fn dispatch<'c>(
        cmd: u16,
        frame: &mut &[u8],
        ctx: &mut ProxyContext<'c>,
    ) -> Result<(), anchor::encoding::ReadError> {
        if cmd == CMD_IDENTIFY {
            // identify(offset: u32, count: u8)
            let offset = <u32 as anchor::encoding::Readable>::read(frame)?;
            let count = <u8 as anchor::encoding::Readable>::read(frame)? as usize;

            let dict = ctx.dictionary;
            let start = offset as usize;
            let end = (start + count).min(dict.len());
            let chunk = if start <= dict.len() { &dict[start..end] } else { &[] };

            // Build identify_response payload:
            // [cmd=0 VLQ][offset VLQ][data_len VLQ][data bytes]
            let mut resp = Vec::new();
            encode_vlq(&mut resp, 0);                // cmd = 0 (identify_response)
            encode_vlq(&mut resp, offset);           // offset
            encode_vlq(&mut resp, chunk.len() as u32);
            resp.extend_from_slice(chunk);
            ctx.pending_responses.push(resp);
        } else {
            // Forward all other commands to the exporter.
            let mut payload = Vec::new();
            encode_vlq(&mut payload, cmd as u32);
            payload.extend_from_slice(frame);
            *frame = &[]; // mark frame as fully consumed
            let _ = ctx.to_exporter.send(payload);
        }
        Ok(())
    }
}

/// Static config reference required by `anchor::Transport::new`.
static PROXY_CONFIG: ProxyConfig = ProxyConfig;

// ─────────────────────────────────────────────────────────────────────────────
// run_smart_host
// ─────────────────────────────────────────────────────────────────────────────

/// Run the smart-proxy host event loop.
///
/// `link_device` is the USB CDC gadget path (`/dev/ttyGS0`).
/// `channels` lists every MCU Unix socket endpoint to expose to Klipper.
pub async fn run_smart_host(
    link_device: String,
    channels: Vec<McuSpec>,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("windlass-bridge smart host: opening USB link {}", link_device);

    let usb = open_serial(&link_device, 0)?;
    let (usb_read, usb_write) = tokio::io::split(usb);

    // Shared USB writer task (serialises writes from all channel tasks).
    let (usb_out_tx, mut usb_out_rx) = mpsc::unbounded_channel::<PayloadTunnelFrame>();

    // Control channel for dictionary fragments from the exporter.
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Watch channel: broadcasts the dictionary to all per-channel accept tasks
    // once the exporter has sent DICT_DONE.
    let (dict_watch_tx, dict_watch_rx) =
        watch::channel::<Option<Arc<Vec<u8>>>>(None);

    // Per-channel: slot for routing CDC MCU payloads to the active connection.
    let mut mcu_payload_slots: HashMap<
        u8,
        Arc<tokio::sync::Mutex<Option<UnboundedSender<Vec<u8>>>>>,
    > = HashMap::new();

    for ch in &channels {
        let ch_id = ch.ch_id;
        prepare_socket_path(&ch.path)?;
        eprintln!(
            "windlass-bridge smart host: ch{} binding Unix socket {}",
            ch_id, ch.path
        );
        mcu_payload_slots.insert(
            ch_id,
            Arc::new(tokio::sync::Mutex::new(None)),
        );
    }

    // Task: dictionary gatherer.
    // Accumulates DICT_FRAG payloads and publishes the result via dict_watch_tx
    // once DICT_DONE is received.
    tokio::spawn(async move {
        let dict = gather_dictionary(ctrl_rx).await;
        let _ = dict_watch_tx.send(Some(Arc::new(dict)));
    });

    // Spawn per-channel accept tasks.
    for ch in channels {
        let ch_id = ch.ch_id;
        let socket_path = ch.path.clone();
        let listener = match UnixListener::bind(&socket_path) {
            Ok(l) => l,
            Err(e) => {
                // socket was already bound in the first loop via
                // prepare_socket_path; if that raced, just skip.
                eprintln!(
                    "windlass-bridge smart host: ch{} bind error ({}): {}",
                    ch_id, socket_path, e
                );
                continue;
            }
        };
        let slot = Arc::clone(&mcu_payload_slots[&ch_id]);
        let usb_out = usb_out_tx.clone();
        let mut dict_rx = dict_watch_rx.clone();

        tokio::spawn(async move {
            // Wait until the dictionary is available.
            let dictionary = loop {
                if let Some(ref d) = *dict_rx.borrow() {
                    break Arc::clone(d);
                }
                if dict_rx.changed().await.is_err() {
                    // Sender dropped (should not happen).
                    return;
                }
                if let Some(ref d) = *dict_rx.borrow() {
                    break Arc::clone(d);
                }
            };

            eprintln!(
                "windlass-bridge smart host: ch{} ready (dictionary {} bytes)",
                ch_id,
                dictionary.len()
            );

            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        eprintln!(
                            "windlass-bridge smart host: ch{} Klipper connected",
                            ch_id
                        );
                        handle_klipper_smart_connection(
                            ch_id,
                            stream,
                            usb_out.clone(),
                            Arc::clone(&slot),
                            Arc::clone(&dictionary),
                        )
                        .await;
                        eprintln!(
                            "windlass-bridge smart host: ch{} Klipper disconnected",
                            ch_id
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "windlass-bridge smart host: ch{} accept error: {}",
                            ch_id, e
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        });
    }

    // Task: USB writer.
    tokio::spawn(async move {
        let mut framed_w = FramedWrite::new(usb_write, PayloadTunnelCodec);
        while let Some(pf) = usb_out_rx.recv().await {
            if let Err(e) = framed_w.send(pf).await {
                eprintln!("windlass-bridge smart host: USB write error: {}", e);
                break;
            }
        }
    });

    // Main loop: USB reader + demux.
    use futures::StreamExt;
    use tokio_util::codec::FramedRead;

    let mut framed_r = FramedRead::new(usb_read, PayloadTunnelCodec);
    loop {
        match framed_r.next().await {
            Some(Ok(pf)) => {
                if pf.ch_id == CTRL_CH {
                    // Forward control frames to the dictionary gatherer task.
                    let _ = ctrl_tx.send(pf.payload.to_vec());
                } else if let Some(slot) = mcu_payload_slots.get(&pf.ch_id) {
                    let guard = slot.lock().await;
                    if let Some(tx) = guard.as_ref() {
                        let _ = tx.send(pf.payload.to_vec());
                    }
                    // If no active Klipper connection, the frame is dropped —
                    // Klipper will reconnect and re-request the missing state.
                } else {
                    eprintln!(
                        "windlass-bridge smart host: received frame for unknown ch{}",
                        pf.ch_id
                    );
                }
            }
            Some(Err(e)) => {
                eprintln!("windlass-bridge smart host: USB read error: {}", e);
                break;
            }
            None => {
                eprintln!("windlass-bridge smart host: USB link closed");
                break;
            }
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Dictionary gatherer
// ─────────────────────────────────────────────────────────────────────────────

/// Accumulate `DICT_FRAG` control payloads and return the assembled dictionary
/// when `DICT_DONE` is received.
async fn gather_dictionary(mut ctrl_rx: mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<u8> {
    let mut dict: Vec<u8> = Vec::new();
    while let Some(payload) = ctrl_rx.recv().await {
        if payload.is_empty() {
            continue;
        }
        match payload[0] {
            CTRL_DICT_FRAG => {
                dict.extend_from_slice(&payload[1..]);
            }
            CTRL_DICT_DONE => {
                eprintln!(
                    "windlass-bridge smart host: dictionary ready ({} bytes)",
                    dict.len()
                );
                return dict;
            }
            other => {
                eprintln!(
                    "windlass-bridge smart host: unknown control type 0x{:02X}, ignoring",
                    other
                );
            }
        }
    }
    // Control channel closed without DICT_DONE — return whatever we have.
    dict
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-connection handler
// ─────────────────────────────────────────────────────────────────────────────

/// Handle a single Klipper connection on a channel's Unix socket.
///
/// Drives a single `anchor::Transport<ProxyConfig>` in a select loop that
/// simultaneously reads bytes from the Klipper socket and MCU response
/// payloads from the CDC link.  Both `receive` and `encode_frame` are always
/// called from this single task, which avoids any concurrent access to the
/// anchor transport.
async fn handle_klipper_smart_connection(
    ch_id: u8,
    stream: UnixStream,
    usb_out: UnboundedSender<PayloadTunnelFrame>,
    mcu_payload_slot: Arc<tokio::sync::Mutex<Option<UnboundedSender<Vec<u8>>>>>,
    dictionary: Arc<Vec<u8>>,
) {
    // anchor output → socket writer.
    let (to_klipper_tx, mut to_klipper_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // CDC MCU payloads → relay loop.
    let (from_mcu_tx, mut from_mcu_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    // Decoded command payloads → USB writer (forwarded to exporter).
    let (to_exporter_tx, mut to_exporter_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Register the MCU-payload slot so the USB demux routes frames here.
    {
        let mut guard = mcu_payload_slot.lock().await;
        *guard = Some(from_mcu_tx);
    }

    // Create the anchor virtual MCU transport.
    let transport = anchor::Transport::<ProxyConfig>::new(
        &PROXY_CONFIG,
        ChannelOutput(to_klipper_tx),
    );

    let (sock_read, mut sock_write) = tokio::io::split(stream);
    let mut sock_read = tokio::io::BufReader::new(sock_read);

    // Task: anchor output → Klipper socket.
    let write_task = tokio::spawn(async move {
        while let Some(bytes) = to_klipper_rx.recv().await {
            if sock_write.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    // Task: decoded commands → USB (to exporter).
    let usb_out_clone = usb_out.clone();
    let cmd_fwd_task = tokio::spawn(async move {
        while let Some(payload) = to_exporter_rx.recv().await {
            let frame = PayloadTunnelFrame {
                ch_id,
                payload: Bytes::from(payload),
            };
            if usb_out_clone.send(frame).is_err() {
                break;
            }
        }
    });

    // Relay loop (single task — serial access to `transport`).
    let mut input_buf: Vec<u8> = Vec::new();
    let mut read_buf = [0u8; 256];
    let mut pending_responses: Vec<Vec<u8>> = Vec::new();

    'relay: loop {
        select! {
            // Bytes from the Klipper socket.
            result = sock_read.read(&mut read_buf) => {
                match result {
                    Ok(0) | Err(_) => break 'relay,
                    Ok(n) => {
                        input_buf.extend_from_slice(&read_buf[..n]);
                        {
                            let ctx = ProxyContext {
                                dictionary: &dictionary,
                                to_exporter: &to_exporter_tx,
                                pending_responses: &mut pending_responses,
                            };
                            transport.receive(&mut input_buf, ctx);
                        }
                        // Drain pending identify_response frames.
                        for resp in pending_responses.drain(..) {
                            transport.encode_frame(|out| out.output(&resp));
                        }
                    }
                }
            }

            // MCU response payload from CDC.
            payload = from_mcu_rx.recv() => {
                match payload {
                    Some(p) => {
                        transport.encode_frame(|out| out.output(&p));
                    }
                    None => break 'relay,
                }
            }
        }
    }

    // Clean up.
    write_task.abort();
    cmd_fwd_task.abort();
    {
        let mut guard = mcu_payload_slot.lock().await;
        *guard = None;
    }
}

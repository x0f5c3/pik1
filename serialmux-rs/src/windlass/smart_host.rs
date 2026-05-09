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
//!    control frames).  Each frame carries the MCU `ch_id` so the host
//!    maintains a separate dictionary for every channel.
//! 2. Bind the Unix socket at the configured path.
//! 3. When the dictionary for a given channel is ready, accept Klipper
//!    connections in a loop.  For each connection:
//!    a. Create an `anchor::Transport<ProxyConfig>` virtual MCU.
//!    b. Enter the relay loop (single tokio task per connection):
//!       - Bytes from the Klipper socket → `transport.receive(…)`.
//!       - Pending `identify_response`s built in `dispatch` →
//!         `transport.encode_frame(…)`.
//!       - MCU response payloads from CDC → `transport.encode_frame(…)`.
//!
//! # `identify` handling
//!
//! When Klipper sends `identify` (cmd=1), `ProxyConfig::dispatch_raw` routes it
//! into `dispatch`, which looks up the correct 40-byte chunk of the real MCU
//! dictionary, builds
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
//! DICT_FRAG: [ 0xFF ][ len ][ 0x01 ][ mcu_ch_id ][ dict_bytes… ]
//! DICT_DONE: [ 0xFF ][ 0x02 ][ 0x02 ][ mcu_ch_id ]
//! ```
//! The `mcu_ch_id` byte routes each fragment to the correct per-channel
//! dictionary accumulator so multi-MCU setups work correctly.

use std::collections::HashMap;
use std::sync::Arc;

use anchor::{
    Config, OutputBuffer as _, ReadError, Readable, ShutdownState, Transport, TransportOutput,
    Writable as _,
};
use bytes::Bytes;
use futures::SinkExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::select;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::watch;
use tokio_util::codec::FramedWrite;

use crate::windlass::async_serial::open_serial;
use crate::windlass::framing::{
    PayloadTunnelCodec, PayloadTunnelFrame, CTRL_CH, CTRL_DICT_DONE, CTRL_DICT_FRAG,
};
use crate::windlass::prepare_socket_path;
use crate::windlass::McuSpec;

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

impl ShutdownState for ProxyContext<'_> {
    fn is_shutdown(&self) -> bool {
        false
    }
}

/// `TransportOutput` that forwards anchor's framed output bytes to a tokio
/// mpsc channel connected to the Klipper socket writer task.
struct ChannelOutput(UnboundedSender<Vec<u8>>);

impl TransportOutput for ChannelOutput {
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

impl Config for ProxyConfig {
    type TransportOutput = ChannelOutput;
    type Context<'c> = ProxyContext<'c>;

    fn dispatch_raw<'c>(frame: &mut &[u8], ctx: &mut ProxyContext<'c>) -> Result<(), ReadError> {
        let mut probe = *frame;
        let cmd = <u16 as Readable>::read(&mut probe)?;
        if cmd == CMD_IDENTIFY {
            let cmd = <u16 as Readable>::read(frame)?;
            return Self::dispatch(cmd, frame, ctx);
        }

        // Forward non-identify commands unchanged (including cmd VLQ).
        let _ = ctx.to_exporter.send(frame.to_vec());
        *frame = &[];
        Ok(())
    }

    fn dispatch<'c>(
        cmd: u16,
        frame: &mut &[u8],
        ctx: &mut ProxyContext<'c>,
    ) -> Result<(), ReadError> {
        if cmd != CMD_IDENTIFY {
            // Non-identify commands are handled in dispatch_raw.
            return Ok(());
        }

        // identify(offset: u32, count: u8)
        let offset = <u32 as Readable>::read(frame)?;
        let count = <u8 as Readable>::read(frame)? as usize;

        let dict = ctx.dictionary;
        let start = offset as usize;
        let end = (start + count).min(dict.len());
        let chunk = if start <= dict.len() {
            &dict[start..end]
        } else {
            &[]
        };

        // Build identify_response payload:
        // [cmd=0 VLQ][offset VLQ][data_len VLQ][data bytes]
        // Uses the root-level anchor::Writable re-export to write
        // VLQ-encoded integers into a Vec<u8> OutputBuffer.
        let mut resp = Vec::new();
        (0u32).write(&mut resp); // cmd = 0 (identify_response)
        offset.write(&mut resp); // offset
        (chunk.len() as u32).write(&mut resp); // data_len
        resp.extend_from_slice(chunk);
        ctx.pending_responses.push(resp);
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
    tracing::info!(device = %link_device, "windlass-bridge smart host: opening USB link");

    let usb = open_serial(&link_device, 0)?;
    let (usb_read, usb_write) = tokio::io::split(usb);

    // Shared USB writer task (serialises writes from all channel tasks).
    let (usb_out_tx, mut usb_out_rx) = mpsc::unbounded_channel::<PayloadTunnelFrame>();

    // Control channel for dictionary fragments from the exporter.
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Per-channel watch channels: broadcast the dictionary to the matching
    // accept task once the exporter has sent DICT_DONE for that channel.
    // Using separate watches (keyed by ch_id) so channels are truly independent
    // and a multi-MCU exporter can deliver dictionaries in any order.
    let mut dict_watch_txs: HashMap<u8, watch::Sender<Option<Arc<Vec<u8>>>>> = HashMap::new();
    let mut dict_watch_rxs: HashMap<u8, watch::Receiver<Option<Arc<Vec<u8>>>>> = HashMap::new();
    for ch in &channels {
        let (tx, rx) = watch::channel::<Option<Arc<Vec<u8>>>>(None);
        dict_watch_txs.insert(ch.ch_id, tx);
        dict_watch_rxs.insert(ch.ch_id, rx);
    }

    // Per-channel: slot for routing CDC MCU payloads to the active connection.
    let mut mcu_payload_slots: HashMap<
        u8,
        Arc<tokio::sync::Mutex<Option<UnboundedSender<Vec<u8>>>>>,
    > = HashMap::new();

    for ch in &channels {
        let ch_id = ch.ch_id;
        prepare_socket_path(&ch.path)?;
        tracing::info!(ch_id, socket = %ch.path, "windlass-bridge smart host: binding Unix socket");
        mcu_payload_slots.insert(ch_id, Arc::new(tokio::sync::Mutex::new(None)));
    }

    // Task: dictionary gatherer.
    // Routes DICT_FRAG/DICT_DONE control frames to the appropriate per-channel
    // watch sender based on the `mcu_ch_id` embedded in each control payload.
    tokio::spawn(async move {
        gather_dictionary(ctrl_rx, dict_watch_txs).await;
    });

    // Spawn per-channel accept tasks.
    for ch in channels {
        let ch_id = ch.ch_id;
        let socket_path = ch.path.clone();
        let listener = match UnixListener::bind(&socket_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(ch_id, socket = %socket_path, err = %e, "windlass-bridge smart host: bind error");
                return Err(e.into());
            }
        };
        let slot = Arc::clone(&mcu_payload_slots[&ch_id]);
        let usb_out = usb_out_tx.clone();
        let mut dict_rx = dict_watch_rxs
            .remove(&ch_id)
            .expect("dict_watch_rx must exist for every channel");

        tokio::spawn(async move {
            // Wait until this channel's dictionary is available.
            // Check the current value before awaiting so we do not miss a
            // dictionary that was already published before this task started
            // waiting on the watch receiver.
            let dictionary = loop {
                if let Some(ref d) = *dict_rx.borrow() {
                    break Arc::clone(d);
                }
                if dict_rx.changed().await.is_err() {
                    // Sender dropped — should not happen in normal operation.
                    return;
                }
            };

            tracing::info!(ch_id, bytes = dictionary.len(), "windlass-bridge smart host: channel ready");

            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        tracing::info!(ch_id, "windlass-bridge smart host: Klipper connected");
                        handle_klipper_smart_connection(
                            ch_id,
                            stream,
                            usb_out.clone(),
                            Arc::clone(&slot),
                            Arc::clone(&dictionary),
                        )
                        .await;
                        tracing::info!(ch_id, "windlass-bridge smart host: Klipper disconnected");
                    }
                    Err(e) => {
                        tracing::error!(ch_id, err = %e, "windlass-bridge smart host: accept error");
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
                tracing::error!(err = %e, "windlass-bridge smart host: USB write error");
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
                    tracing::warn!(ch_id = pf.ch_id, "windlass-bridge smart host: received frame for unknown channel");
                }
            }
            Some(Err(e)) => {
                tracing::error!(err = %e, "windlass-bridge smart host: USB read error");
                break;
            }
            None => {
                tracing::info!("windlass-bridge smart host: USB link closed");
                break;
            }
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Dictionary gatherer
// ─────────────────────────────────────────────────────────────────────────────

/// Accumulate per-channel `DICT_FRAG` control payloads and publish each
/// channel's complete dictionary via its watch sender when `DICT_DONE` arrives.
///
/// Each control payload now includes the MCU `ch_id` as the second byte so
/// the gatherer can route fragments to the correct per-channel accumulator:
///
/// ```text
/// DICT_FRAG payload: [ CTRL_DICT_FRAG ][ ch_id ][ dict_bytes… ]
/// DICT_DONE payload: [ CTRL_DICT_DONE ][ ch_id ]
/// ```
async fn gather_dictionary(
    mut ctrl_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    watch_txs: HashMap<u8, watch::Sender<Option<Arc<Vec<u8>>>>>,
) {
    // Per-channel dictionary accumulation buffers.
    let mut dicts: HashMap<u8, Vec<u8>> = HashMap::new();

    while let Some(payload) = ctrl_rx.recv().await {
        // Every valid control payload has at least a type byte and a ch_id byte.
        if payload.len() < 2 {
            tracing::warn!("windlass-bridge smart host: control payload too short, ignoring");
            continue;
        }
        let ctrl_type = payload[0];
        let mcu_ch_id = payload[1];

        match ctrl_type {
            CTRL_DICT_FRAG => {
                dicts
                    .entry(mcu_ch_id)
                    .or_default()
                    .extend_from_slice(&payload[2..]);
            }
            CTRL_DICT_DONE => {
                let dict = dicts.remove(&mcu_ch_id).unwrap_or_default();
                if dict.is_empty() {
                    tracing::warn!(ch_id = mcu_ch_id,
                        "windlass-bridge smart host: DICT_DONE received with no preceding DICT_FRAG frames");
                }
                tracing::info!(ch_id = mcu_ch_id, bytes = dict.len(),
                    "windlass-bridge smart host: dictionary ready");
                if let Some(tx) = watch_txs.get(&mcu_ch_id) {
                    let _ = tx.send(Some(Arc::new(dict)));
                } else {
                    tracing::warn!(ch_id = mcu_ch_id,
                        "windlass-bridge smart host: DICT_DONE for unknown channel, ignoring");
                }
            }
            other => {
                tracing::warn!(ctrl_type = other,
                    "windlass-bridge smart host: unknown control type, ignoring");
            }
        }
    }
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
    let transport = Transport::<ProxyConfig>::new(&PROXY_CONFIG, ChannelOutput(to_klipper_tx));

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

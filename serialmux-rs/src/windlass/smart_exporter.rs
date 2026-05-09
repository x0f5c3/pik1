//! Smart-proxy exporter mode — runs on the K1 / K1C SoC.
//!
//! Unlike the transparent-relay exporter ([`crate::windlass::exporter`]), the
//! smart exporter terminates the MCU's Klipper transport session (CRC-16,
//! ACK/NAK, sequence numbers) and forwards only the decoded **payload bytes**
//! over the CDC link.  This removes all Klipper wire-protocol overhead from
//! the CDC tunnel and enables the host to answer `identify` locally.
//!
//! # Startup sequence per channel
//!
//! 1. Open the UART and connect a [`windlass::McuConnection`].
//! 2. Read the raw compressed dictionary bytes from
//!    [`windlass::McuConnection::raw_dictionary_bytes`].
//! 3. Reopen the UART and connect a [`windlass::Transport`] for relay mode.
//! 4. Forward the dictionary to the host over the CDC control channel
//!    (`ch_id = 0xFF`) as `DICT_FRAG` frames followed by a `DICT_DONE` frame.
//!    Every frame includes the MCU's `ch_id` so the host can maintain separate
//!    dictionaries for each channel.
//! 5. Enter the relay loop:
//!    - MCU payload received → forward as `[ch_id][len][payload]` over CDC.
//!    - Command payload received from CDC → send to MCU via `windlass::Transport`.
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
//! The `mcu_ch_id` byte identifies which MCU channel the dictionary (or
//! completion marker) belongs to, allowing the host to maintain a separate
//! per-channel dictionary for multi-MCU setups.

use bytes::Bytes;
use futures::SinkExt;
use tokio::io::split;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::codec::FramedWrite;
use windlass::{McuConnection, Transport};

use crate::windlass::async_serial::open_serial;
use crate::windlass::framing::{
    PayloadTunnelCodec, PayloadTunnelFrame, CTRL_CH, CTRL_DICT_DONE, CTRL_DICT_FRAG, DICT_FRAG_MAX,
};
use crate::windlass::McuSpec;

/// Run the smart-proxy exporter event loop.
///
/// `link_device` is the USB CDC ACM device path (`/dev/ttyACMn`).
/// `channels` lists every MCU UART to bridge.
///
/// This function runs until the process is killed or a fatal I/O error occurs.
pub async fn run_smart_exporter(
    link_device: String,
    channels: Vec<McuSpec>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(device = %link_device, "windlass-bridge smart exporter: opening USB link");

    // Open the USB CDC device (baud=0 → skip cfsetspeed).
    let usb = open_serial(&link_device, 0)?;
    let (usb_read, usb_write) = split(usb);

    // Single writer task serialises all per-channel writes to the USB link.
    let (usb_tx, mut usb_rx) = mpsc::unbounded_channel::<PayloadTunnelFrame>();

    // Per-channel sender: delivers command payloads received FROM the USB to
    // the UART write side for that channel.
    let mut uart_cmd_txs: std::collections::HashMap<u8, UnboundedSender<Vec<u8>>> =
        std::collections::HashMap::new();

    for ch in channels {
        let ch_id = ch.ch_id;
        tracing::info!(ch_id, path = %ch.path, baud = ch.baud, "windlass-bridge smart exporter: opening UART");

        let uart = match open_serial(&ch.path, ch.baud) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(ch_id, path = %ch.path, err = %e, "windlass-bridge smart exporter: cannot open UART");
                return Err(e.into());
            }
        };

        // Fetch the MCU dictionary with the high-level McuConnection API.
        tracing::info!(ch_id, "windlass-bridge smart exporter: fetching MCU dictionary");
        let dictionary = match McuConnection::connect(uart).await {
            Ok(conn) => {
                let dict = conn.raw_dictionary_bytes().to_vec();
                conn.close().await;
                tracing::info!(ch_id, bytes = dict.len(), "windlass-bridge smart exporter: dictionary fetched");
                dict
            }
            Err(e) => {
                tracing::error!(ch_id, err = %e, "windlass-bridge smart exporter: dictionary fetch failed");
                return Err(e.into());
            }
        };

        // Reopen UART for low-level relay transport.
        let uart = match open_serial(&ch.path, ch.baud) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(ch_id, path = %ch.path, err = %e, "windlass-bridge smart exporter: cannot reopen UART");
                return Err(e.into());
            }
        };
        let (transport, mut payload_rx) = Transport::connect(uart).await;

        // Send the dictionary to the host over the control channel,
        // tagged with this channel's ch_id so the host routes it correctly.
        send_dictionary(&usb_tx, ch_id, &dictionary)?;

        // Per-channel mpsc for host→MCU command payloads.
        let (uart_cmd_tx, mut uart_cmd_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        uart_cmd_txs.insert(ch_id, uart_cmd_tx);

        let usb_tx_clone = usb_tx.clone();

        // Task: MCU → USB.
        // Each decoded MCU payload is forwarded as a PayloadTunnelFrame.
        tokio::spawn(async move {
            loop {
                match payload_rx.recv().await {
                    Some(Ok(payload)) => {
                        let frame = PayloadTunnelFrame {
                            ch_id,
                            payload: Bytes::from(payload),
                        };
                        if usb_tx_clone.send(frame).is_err() {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::error!(ch_id, err = %e, "windlass-bridge smart exporter: MCU transport error");
                        break;
                    }
                    None => {
                        tracing::warn!(ch_id, "windlass-bridge smart exporter: MCU transport closed");
                        break;
                    }
                }
            }
        });

        // Task: USB → MCU.
        // Command payloads from the host are sent to the MCU via windlass::Transport.
        tokio::spawn(async move {
            while let Some(payload) = uart_cmd_rx.recv().await {
                if let Err(e) = transport.send(&payload) {
                    tracing::error!(ch_id, err = %e, "windlass-bridge smart exporter: MCU send error");
                    break;
                }
            }
        });
    }

    // Task: USB writer.
    tokio::spawn(async move {
        let mut framed_w = FramedWrite::new(usb_write, PayloadTunnelCodec);
        while let Some(pf) = usb_rx.recv().await {
            if let Err(e) = framed_w.send(pf).await {
                tracing::error!(err = %e, "windlass-bridge smart exporter: USB write error");
                break;
            }
        }
    });

    // Main loop: USB reader + demux.
    // Read PayloadTunnelFrames from the USB link and route command payloads to
    // the matching UART channel.
    use futures::StreamExt;
    use tokio_util::codec::FramedRead;

    let mut framed_r = FramedRead::new(usb_read, PayloadTunnelCodec);
    loop {
        match framed_r.next().await {
            Some(Ok(pf)) => {
                if pf.ch_id == CTRL_CH {
                    // Control frames from host to exporter — currently unused.
                    continue;
                }
                if let Some(tx) = uart_cmd_txs.get(&pf.ch_id) {
                    let _ = tx.send(pf.payload.to_vec());
                } else {
                    tracing::warn!(ch_id = pf.ch_id, "windlass-bridge smart exporter: received frame for unknown channel");
                }
            }
            Some(Err(e)) => {
                tracing::error!(err = %e, "windlass-bridge smart exporter: USB read error");
                break;
            }
            None => {
                tracing::info!("windlass-bridge smart exporter: USB link closed");
                break;
            }
        }
    }

    Ok(())
}

/// Encode and send the MCU dictionary over the control channel.
///
/// Every control frame includes `ch_id` so the host can maintain separate
/// dictionaries for each MCU channel in multi-MCU setups.
///
/// The dictionary is split into fragments of up to [`DICT_FRAG_MAX`] bytes
/// each, sent as `DICT_FRAG` control frames.  A final `DICT_DONE` frame
/// signals completion to the host.
///
/// ## Control payload layout
///
/// ```text
/// DICT_FRAG: [ CTRL_DICT_FRAG ][ ch_id ][ dict_bytes… ]
/// DICT_DONE: [ CTRL_DICT_DONE ][ ch_id ]
/// ```
fn send_dictionary(
    usb_tx: &UnboundedSender<PayloadTunnelFrame>,
    ch_id: u8,
    dictionary: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    for chunk in dictionary.chunks(DICT_FRAG_MAX) {
        // payload = [ type ][ ch_id ][ dict_bytes… ]
        let mut payload = Vec::with_capacity(2 + chunk.len());
        payload.push(CTRL_DICT_FRAG);
        payload.push(ch_id);
        payload.extend_from_slice(chunk);
        usb_tx
            .send(PayloadTunnelFrame {
                ch_id: CTRL_CH,
                payload: Bytes::from(payload),
            })
            .map_err(|_| "USB writer task closed")?;
    }
    // Signal end of dictionary for this channel.
    // Use a stack array and copy_from_slice to avoid a heap allocation for
    // this 2-byte frame.
    let done_payload: [u8; 2] = [CTRL_DICT_DONE, ch_id];
    usb_tx
        .send(PayloadTunnelFrame {
            ch_id: CTRL_CH,
            payload: Bytes::copy_from_slice(&done_payload),
        })
        .map_err(|_| "USB writer task closed")?;
    Ok(())
}

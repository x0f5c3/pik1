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
//! 1. Open the UART and connect a [`McuTransport`].
//! 2. Perform the `identify`/`identify_response` exchange to obtain the MCU's
//!    compressed data dictionary.
//! 3. Forward the dictionary to the host over the CDC control channel
//!    (`ch_id = 0xFF`) as `DICT_FRAG` frames followed by a `DICT_DONE` frame.
//! 4. Enter the relay loop:
//!    - MCU payload received → forward as `[ch_id][len][payload]` over CDC.
//!    - Command payload received from CDC → send to MCU via `McuTransport`.
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

use bytes::Bytes;
use futures::SinkExt;
use tokio::io::{split};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::codec::{FramedWrite};

use crate::windlass::McuSpec;
use crate::windlass::async_serial::open_serial;
use crate::windlass::framing::{
    CTRL_CH, CTRL_DICT_DONE, CTRL_DICT_FRAG, DICT_FRAG_MAX, PayloadTunnelCodec, PayloadTunnelFrame,
};
use crate::windlass::mcu_transport::{McuTransport, fetch_dictionary};

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
    eprintln!(
        "windlass-bridge smart exporter: opening USB link {}",
        link_device
    );

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
        eprintln!(
            "windlass-bridge smart exporter: ch{} opening UART {} @ {} baud",
            ch_id, ch.path, ch.baud
        );

        let uart = match open_serial(&ch.path, ch.baud) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "windlass-bridge smart exporter: ch{} cannot open {}: {}",
                    ch_id, ch.path, e
                );
                return Err(e.into());
            }
        };

        let (transport, mut payload_rx) = McuTransport::connect(uart).await;

        // Fetch the MCU dictionary before entering relay mode.
        eprintln!(
            "windlass-bridge smart exporter: ch{} fetching MCU dictionary…",
            ch_id
        );
        let dictionary = match fetch_dictionary(&transport, &mut payload_rx).await {
            Ok(d) => {
                eprintln!(
                    "windlass-bridge smart exporter: ch{} dictionary {} bytes",
                    ch_id,
                    d.len()
                );
                d
            }
            Err(e) => {
                eprintln!(
                    "windlass-bridge smart exporter: ch{} dictionary fetch failed: {}",
                    ch_id, e
                );
                return Err(e);
            }
        };

        // Send the dictionary to the host over the control channel.
        send_dictionary(&usb_tx, &dictionary)?;

        // Per-channel mpsc for host→MCU command payloads.
        let (uart_cmd_tx, mut uart_cmd_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        uart_cmd_txs.insert(ch_id, uart_cmd_tx);

        let usb_tx_clone = usb_tx.clone();

        // Task: MCU → USB.
        // Each decoded MCU payload is forwarded as a PayloadTunnelFrame.
        tokio::spawn(async move {
            loop {
                match payload_rx.recv().await {
                    Some(payload) => {
                        let frame = PayloadTunnelFrame {
                            ch_id,
                            payload: Bytes::from(payload),
                        };
                        if usb_tx_clone.send(frame).is_err() {
                            break;
                        }
                    }
                    None => {
                        eprintln!(
                            "windlass-bridge smart exporter: ch{} MCU transport closed",
                            ch_id
                        );
                        break;
                    }
                }
            }
        });

        // Task: USB → MCU.
        // Command payloads from the host are sent to the MCU via McuTransport.
        tokio::spawn(async move {
            while let Some(payload) = uart_cmd_rx.recv().await {
                transport.send(payload);
            }
        });
    }

    // Task: USB writer.
    tokio::spawn(async move {
        let mut framed_w = FramedWrite::new(usb_write, PayloadTunnelCodec);
        while let Some(pf) = usb_rx.recv().await {
            if let Err(e) = framed_w.send(pf).await {
                eprintln!("windlass-bridge smart exporter: USB write error: {}", e);
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
                    eprintln!(
                        "windlass-bridge smart exporter: received frame for unknown ch{}",
                        pf.ch_id
                    );
                }
            }
            Some(Err(e)) => {
                eprintln!("windlass-bridge smart exporter: USB read error: {}", e);
                break;
            }
            None => {
                eprintln!("windlass-bridge smart exporter: USB link closed");
                break;
            }
        }
    }

    Ok(())
}

/// Encode and send the MCU dictionary over the control channel.
///
/// The dictionary is split into fragments of up to [`DICT_FRAG_MAX`] bytes
/// each, sent as `DICT_FRAG` control frames.  A final `DICT_DONE` frame
/// signals completion to the host.
fn send_dictionary(
    usb_tx: &UnboundedSender<PayloadTunnelFrame>,
    dictionary: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    for chunk in dictionary.chunks(DICT_FRAG_MAX) {
        let mut payload = Vec::with_capacity(1 + chunk.len());
        payload.push(CTRL_DICT_FRAG);
        payload.extend_from_slice(chunk);
        usb_tx
            .send(PayloadTunnelFrame {
                ch_id: CTRL_CH,
                payload: Bytes::from(payload),
            })
            .map_err(|_| "USB writer task closed")?;
    }
    // Signal end of dictionary.
    usb_tx
        .send(PayloadTunnelFrame {
            ch_id: CTRL_CH,
            payload: Bytes::from_static(&[CTRL_DICT_DONE]),
        })
        .map_err(|_| "USB writer task closed")?;
    Ok(())
}

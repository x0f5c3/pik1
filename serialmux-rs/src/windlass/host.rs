//! Host mode — runs on the Raspberry Pi / BTT CB1.
//!
//! For each MCU channel the host:
//! 1. Binds a Unix domain socket at the configured path.
//! 2. Waits for Klipper to connect to that socket.
//! 3. Relays Klipper frames bidirectionally between the socket and the USB
//!    CDC link.
//!
//! The Unix socket path is placed at the user-supplied location (typically
//! `/tmp/klipper_mcuN`).  Set `serial: /tmp/klipper_mcu0` in `printer.cfg`:
//!
//! ```ini
//! [mcu]
//! serial: /tmp/klipper_mcu0
//! restart_method: command
//!
//! [mcu nozzle_mcu]
//! serial: /tmp/klipper_mcu1
//! restart_method: command
//! ```
//!
//! Klipper will automatically reconnect on restart, so the host loop accepts
//! new connections in a loop after each disconnect.
//!
//! # Architecture
//!
//! ```text
//! Klipper ──► UnixListener ──► KlipperFramer ──► TunnelFrame → USB writer
//!                               (read from socket)
//!
//! USB reader ──► TunnelFrame ──► demux ──► raw bytes → socket write
//! ```
//!
//! For transport-terminating behavior, use [`crate::windlass::smart_host`].
//! This module remains the transparent relay path that forwards complete raw
//! Klipper frames over the USB tunnel.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncWriteExt, split};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::windlass::McuSpec;
use crate::windlass::async_serial::open_serial;
use crate::windlass::framing::{KlipperFramer, TunnelCodec, TunnelFrame};
use crate::windlass::prepare_socket_path;

/// Run the host event loop.
///
/// `link_device` is the USB CDC gadget path (`/dev/ttyGS0`).
/// `channels` lists every MCU Unix socket endpoint to expose to Klipper.
pub async fn run_host(
    link_device: String,
    channels: Vec<McuSpec>,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("windlass-bridge host: opening USB link {}", link_device);

    // Open the USB CDC gadget device.  Baud = 0 → skip cfsetspeed.
    let usb = open_serial(&link_device, 0)?;
    let (usb_read, usb_write) = split(usb);

    // Shared USB writer (serialises writes from all channel tasks).
    let (usb_tx, mut usb_rx) = mpsc::unbounded_channel::<TunnelFrame>();

    // Per-channel sender: delivers raw Klipper frame bytes arriving FROM the
    // USB to the current Klipper client on that channel's socket.
    // Wrapped in Arc<Mutex<Option<…>>> so socket tasks can replace the
    // sender when Klipper reconnects.
    let mut klipper_write_txs: HashMap<
        u8,
        Arc<Mutex<Option<mpsc::UnboundedSender<Bytes>>>>,
    > = HashMap::new();

    for ch in channels {
        let ch_id = ch.ch_id;
        let socket_path = ch.path.clone();

        // Remove any stale socket from a previous run.
        prepare_socket_path(&socket_path)?;

        eprintln!(
            "windlass-bridge host: ch{} binding Unix socket {}",
            ch_id, socket_path
        );

        let listener = UnixListener::bind(&socket_path)?;
        let usb_tx_clone = usb_tx.clone();

        // Shared slot for the current Klipper client's write sender.
        let klipper_tx_slot: Arc<Mutex<Option<mpsc::UnboundedSender<Bytes>>>> =
            Arc::new(Mutex::new(None));
        klipper_write_txs.insert(ch_id, Arc::clone(&klipper_tx_slot));

        // Spawn a task that accepts Klipper connections in a loop.
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        eprintln!(
                            "windlass-bridge host: ch{} Klipper connected on {}",
                            ch_id, socket_path
                        );
                        handle_klipper_connection(
                            ch_id,
                            stream,
                            usb_tx_clone.clone(),
                            Arc::clone(&klipper_tx_slot),
                        )
                        .await;
                        eprintln!(
                            "windlass-bridge host: ch{} Klipper disconnected from {}",
                            ch_id, socket_path
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "windlass-bridge host: ch{} accept error: {}",
                            ch_id, e
                        );
                        // Brief pause before retrying.
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        });
    }

    // Task: USB writer.
    tokio::spawn(async move {
        let mut framed_w = FramedWrite::new(usb_write, TunnelCodec);
        while let Some(tf) = usb_rx.recv().await {
            if let Err(e) = framed_w.send(tf).await {
                eprintln!("windlass-bridge host: USB write error: {}", e);
                break;
            }
        }
    });

    // Main loop: USB reader + demux.
    // Read tunnel frames from the USB link and route each to the current
    // Klipper client for that channel (if any client is connected).
    let mut framed_r = FramedRead::new(usb_read, TunnelCodec);
    loop {
        match framed_r.next().await {
            Some(Ok(tf)) => {
                let ch_id = tf.ch_id;
                if let Some(slot) = klipper_write_txs.get(&ch_id) {
                    let guard = slot.lock().await;
                    if let Some(tx) = guard.as_ref() {
                        if tx.send(tf.frame).is_err() {
                            // Channel closed (Klipper disconnected); drop frame.
                        }
                    }
                    // If no Klipper client is connected, the frame is silently
                    // dropped — the MCU will retransmit if it needs an ACK.
                } else {
                    eprintln!(
                        "windlass-bridge host: received frame for unknown ch{}",
                        ch_id
                    );
                }
            }
            Some(Err(e)) => {
                eprintln!("windlass-bridge host: USB read error: {}", e);
                break;
            }
            None => {
                eprintln!("windlass-bridge host: USB link closed");
                break;
            }
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-connection handler
// ─────────────────────────────────────────────────────────────────────────────

/// Handle a single Klipper connection on a channel's Unix socket.
///
/// Spawns two inner tasks:
/// - **socket→USB**: reads Klipper frames from the socket, sends them over USB.
/// - **USB→socket**: receives frames routed from the USB reader, writes to socket.
///
/// Returns when Klipper disconnects (either task exits).
async fn handle_klipper_connection(
    ch_id: u8,
    stream: UnixStream,
    usb_tx: mpsc::UnboundedSender<TunnelFrame>,
    klipper_tx_slot: Arc<Mutex<Option<mpsc::UnboundedSender<Bytes>>>>,
) {
    let (sock_read, mut sock_write) = split(stream);

    // Per-connection channel: USB demux → socket writer.
    let (klipper_tx, mut klipper_rx) = mpsc::unbounded_channel::<Bytes>();

    // Install the sender so the USB demux can route frames here.
    {
        let mut guard = klipper_tx_slot.lock().await;
        *guard = Some(klipper_tx);
    }

    // Task: socket → USB.
    let usb_tx_clone = usb_tx.clone();
    let sock_to_usb = tokio::spawn(async move {
        let mut framed = FramedRead::new(sock_read, KlipperFramer::new());
        while let Some(result) = framed.next().await {
            match result {
                Ok(frame) => {
                    let tf = TunnelFrame { ch_id, frame };
                    if usb_tx_clone.send(tf).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!(
                        "windlass-bridge host: ch{} socket read error: {}",
                        ch_id, e
                    );
                    break;
                }
            }
        }
    });

    // Task: USB → socket.
    let usb_to_sock = tokio::spawn(async move {
        while let Some(frame) = klipper_rx.recv().await {
            if let Err(e) = sock_write.write_all(&frame).await {
                eprintln!(
                    "windlass-bridge host: ch{} socket write error: {}",
                    ch_id, e
                );
                break;
            }
        }
    });

    // Wait for either direction to finish (means Klipper disconnected).
    tokio::select! {
        _ = sock_to_usb => {}
        _ = usb_to_sock => {}
    }

    // Clear the sender slot so the USB demux stops routing to the old client.
    let mut guard = klipper_tx_slot.lock().await;
    *guard = None;
}

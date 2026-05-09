//! Exporter mode — runs on the K1 / K1C SoC.
//!
//! For each MCU channel the exporter:
//! 1. Opens the UART as a non-blocking async serial port.
//! 2. Reads raw Klipper frames from the UART using [`KlipperFramer`].
//! 3. Prefixes each frame with the channel ID and writes it to the USB CDC
//!    link using [`TunnelCodec`].
//! 4. Concurrently reads tunnel frames from the USB CDC link and routes them
//!    back to the correct UART.
//!
//! The USB CDC link is shared among all channel tasks via an
//! `mpsc::UnboundedSender<TunnelFrame>` that feeds a single dedicated writer
//! task — this serialises writes without any per-frame locking.
//!
//! For transport-terminating behavior and dictionary forwarding, use
//! [`crate::windlass::smart_exporter`]. This module remains the transparent
//! relay path that forwards complete raw Klipper frames over the USB tunnel.

use std::collections::HashMap;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::{split, AsyncWriteExt};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::windlass::async_serial::open_serial;
use crate::windlass::framing::{KlipperFramer, TunnelCodec, TunnelFrame};
use crate::windlass::McuSpec;

/// Run the exporter event loop.
///
/// `link_device` is the USB CDC ACM device path (`/dev/ttyACMn`).
/// `channels` lists every MCU UART to bridge.
///
/// This function runs until the process is killed (or one of the I/O tasks
/// encounters a fatal error).
pub async fn run_exporter(
    link_device: String,
    channels: Vec<McuSpec>,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("windlass-bridge exporter: opening USB link {}", link_device);

    // Open the USB CDC device.  Baud = 0 means "skip cfsetspeed" — CDC ACM
    // ignores baud, so we just need the raw non-blocking fd.
    let usb = open_serial(&link_device, 0)?;
    let (usb_read, usb_write) = split(usb);

    // Single writer task for the USB link (serialises concurrent writes).
    let (usb_tx, mut usb_rx) = mpsc::unbounded_channel::<TunnelFrame>();

    // Map ch_id → sender that delivers frames received FROM the USB to the
    // UART write task for that channel.
    let mut uart_write_txs: HashMap<u8, UnboundedSender<Bytes>> = HashMap::new();

    // Spawn a write task and a read task for each UART channel.
    for ch in channels {
        let ch_id = ch.ch_id;
        eprintln!(
            "windlass-bridge exporter: ch{} opening UART {} @ {} baud",
            ch_id, ch.path, ch.baud
        );

        let uart = match open_serial(&ch.path, ch.baud) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "windlass-bridge exporter: ch{} cannot open {}: {}",
                    ch_id, ch.path, e
                );
                return Err(e.into());
            }
        };
        let (uart_read, mut uart_write) = split(uart);

        // Per-channel channel for frames routed from USB to this UART.
        let (uart_write_tx, mut uart_write_rx) = mpsc::unbounded_channel::<Bytes>();
        uart_write_txs.insert(ch_id, uart_write_tx);

        let usb_tx_clone = usb_tx.clone();

        // Task: UART → USB.
        // Read complete Klipper frames from the UART and forward them over
        // the shared USB link with the channel ID prefix.
        tokio::spawn(async move {
            let mut framed = FramedRead::new(uart_read, KlipperFramer::new());
            loop {
                match framed.next().await {
                    Some(Ok(frame)) => {
                        let tf = TunnelFrame { ch_id, frame };
                        if usb_tx_clone.send(tf).is_err() {
                            // USB writer task exited.
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!(
                            "windlass-bridge exporter: ch{} UART read error: {}",
                            ch_id, e
                        );
                        break;
                    }
                    None => {
                        eprintln!("windlass-bridge exporter: ch{} UART stream ended", ch_id);
                        break;
                    }
                }
            }
        });

        // Task: USB → UART.
        // Receive raw Klipper frame bytes (already demuxed by ch_id) and
        // write them verbatim to the UART.
        tokio::spawn(async move {
            while let Some(frame) = uart_write_rx.recv().await {
                if let Err(e) = uart_write.write_all(&frame).await {
                    eprintln!(
                        "windlass-bridge exporter: ch{} UART write error: {}",
                        ch_id, e
                    );
                    break;
                }
            }
        });
    }

    // Task: USB writer.
    // All per-channel UART→USB tasks send to `usb_tx`; this task is the
    // only writer of the USB link, preventing concurrent write races.
    tokio::spawn(async move {
        let mut framed_w = FramedWrite::new(usb_write, TunnelCodec);
        while let Some(tf) = usb_rx.recv().await {
            if let Err(e) = framed_w.send(tf).await {
                eprintln!("windlass-bridge exporter: USB write error: {}", e);
                break;
            }
        }
    });

    // Task: USB reader + demux.
    // Read tunnel frames from the USB link and route each frame to the
    // matching UART write task by ch_id.
    let mut framed_r = FramedRead::new(usb_read, TunnelCodec);
    loop {
        match framed_r.next().await {
            Some(Ok(tf)) => {
                if let Some(tx) = uart_write_txs.get(&tf.ch_id) {
                    let _ = tx.send(tf.frame);
                } else {
                    eprintln!(
                        "windlass-bridge exporter: received frame for unknown ch{}",
                        tf.ch_id
                    );
                }
            }
            Some(Err(e)) => {
                eprintln!("windlass-bridge exporter: USB read error: {}", e);
                break;
            }
            None => {
                eprintln!("windlass-bridge exporter: USB link closed");
                break;
            }
        }
    }

    Ok(())
}

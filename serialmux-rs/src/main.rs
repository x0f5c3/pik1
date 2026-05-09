//! `serialmux` — Klipper serial + TCP multiplexer over a USB CDC gadget link.
//!
//! Drop-in Rust replacement for `serialmux.py`.  Identical CLI, identical
//! wire protocol, identical channel semantics — only the binary path in
//! `ExecStart` changes.
//!
//! # Modes
//! | Mode       | Description |
//! |------------|-------------|
//! | `exporter` | Runs on the machine wired to the MCUs (K1 / K1C SoC board). |
//! | `host`     | Runs on the Klipper host (Raspberry Pi / BTT CB1). |
//!
//! See `serialmux --help` for full usage.

mod channel;
mod daemon;
mod logging;
mod protocol;
mod serial;

use channel::{McuChannel, PtyChannel, TcpDestChannel, TcpSourceChannel};
use daemon::Daemon;
use serial::wait_for_acm;

const DESCRIPTION: &str = r#"serialmux -- Klipper serial + TCP multiplexer over a USB CDC gadget link.
Single-threaded, event-driven. Drop-in replacement for serialmux.py.

MODES
  exporter   Run on the machine physically wired to the MCUs (e.g. K1 / K1C board).
  host       Run on the machine running Klipper (e.g. Raspberry Pi + BTT CB1).

LINK DEVICE
  --usb VID:PID   Discover CDC ACM device by USB ID (waits for appearance).
  link_dev        Explicit device path (e.g. /dev/ttyGS0).
  One of these is required; they are mutually exclusive.

CHANNEL SPECS
  mcu:<id>:<uart_device>:<baud>      exporter MCU channel
  mcu:<id>:<pty_symlink>[:<baud>]    host MCU channel  (baud defaults to 230400)
  tcp:<id>:<bind_addr>:<port>        exporter TCP tunnel channel
  tcp:<id>:<dest_addr>:<port>        host TCP tunnel channel

EXAMPLES
  Exporter (K1C board):
    serialmux exporter --usb 1d6b:0104 \
        mcu:0:/dev/ttyS7:230400 \
        mcu:1:/dev/ttyS1:230400 \
        tcp:2:0.0.0.0:7125

  Host (Raspberry Pi / BTT CB1):
    serialmux host /dev/ttyGS0 \
        mcu:0:/tmp/klipper_mcu \
        mcu:1:/tmp/klipper_toolhead \
        tcp:2:127.0.0.1:7125

  printer.cfg:
    [mcu]
    serial: /tmp/klipper_mcu
"#;

// ---------------------------------------------------------------------------
// CLI argument types
// ---------------------------------------------------------------------------

/// Parsed channel specification from the CLI.
#[derive(Debug)]
enum ChannelSpec {
    McuExporter { id: u8, device: String, baud: u32 },
    McuHost     { id: u8, symlink: String, baud: u32 },
    TcpExporter { id: u8, bind_addr: String, port: u16 },
    TcpHost     { id: u8, dest_addr: String, port: u16 },
}

struct Args {
    mode:     String,
    link_dev: Option<String>,
    usb_id:   Option<(String, String)>,
    channels: Vec<ChannelSpec>,
}

// ---------------------------------------------------------------------------
// Argument parsing (no external dependency — mirrors argparse)
// ---------------------------------------------------------------------------

fn usage_exit(msg: &str) -> ! {
    eprintln!("serialmux: error: {}", msg);
    eprintln!("\nRun `serialmux --help` for usage.");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() < 2 {
        usage_exit("too few arguments");
    }
    if raw[1] == "-h" || raw[1] == "--help" {
        println!("{}", DESCRIPTION);
        std::process::exit(0);
    }

    // Validate and consume mode
    let mode = raw[1].clone();
    if mode != "exporter" && mode != "host" {
        usage_exit(&format!("mode must be 'exporter' or 'host', got {:?}", mode));
    }

    let mut link_dev: Option<String> = None;
    let mut usb_id: Option<(String, String)> = None;
    let mut channel_specs: Vec<String> = Vec::new();
    let mut i = 2usize;

    while i < raw.len() {
        let arg = &raw[i];
        if arg == "--usb" {
            i += 1;
            if i >= raw.len() { usage_exit("--usb requires an argument"); }
            let val = &raw[i];
            usb_id = Some(parse_usb_id(val));
        } else if arg.starts_with("--usb=") {
            let val = &arg["--usb=".len()..];
            usb_id = Some(parse_usb_id(val));
        } else if arg.starts_with("mcu:") || arg.starts_with("tcp:") {
            channel_specs.push(arg.clone());
        } else if !arg.starts_with('-') && link_dev.is_none() && usb_id.is_none() {
            link_dev = Some(arg.clone());
        } else if !arg.starts_with('-') {
            // Could be a positional link_dev after --usb was parsed out of order
            if link_dev.is_none() {
                link_dev = Some(arg.clone());
            } else {
                channel_specs.push(arg.clone());
            }
        } else {
            usage_exit(&format!("unknown argument: {:?}", arg));
        }
        i += 1;
    }

    if link_dev.is_some() && usb_id.is_some() {
        usage_exit("link_dev and --usb are mutually exclusive");
    }
    if link_dev.is_none() && usb_id.is_none() {
        usage_exit("one of link_dev or --usb is required");
    }
    if channel_specs.is_empty() {
        usage_exit("at least one channel_spec is required");
    }

    let channels = parse_channel_specs(&mode, &channel_specs);

    Args { mode, link_dev, usb_id, channels }
}

fn parse_usb_id(val: &str) -> (String, String) {
    let parts: Vec<&str> = val.splitn(2, ':').collect();
    if parts.len() != 2 {
        usage_exit(&format!("--usb must be VID:PID (e.g. 1d6b:0104), got {:?}", val));
    }
    for (part, name) in parts.iter().zip(["VID", "PID"].iter()) {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            usage_exit(&format!("{} in {:?} must be a hex string", name, val));
        }
    }
    (parts[0].to_lowercase(), parts[1].to_lowercase())
}

fn parse_channel_specs(mode: &str, specs: &[String]) -> Vec<ChannelSpec> {
    let valid_bauds: &[u32] = &[1200, 2400, 4800, 9600, 19200, 38400, 57600,
                                 115200, 230400, 460800, 921600];
    let mut seen_ids: std::collections::HashSet<u8> = Default::default();
    let mut out = Vec::new();

    for spec in specs {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 3 {
            usage_exit(&format!("channel spec {:?} is too short\n\
                exporter mcu: mcu:<id>:<device>:<baud>\n\
                host     mcu: mcu:<id>:<pty_symlink>[:<baud>]\n\
                exporter tcp: tcp:<id>:<bind_addr>:<port>\n\
                host     tcp: tcp:<id>:<dest_addr>:<port>", spec));
        }
        let kind = parts[0];
        if kind != "mcu" && kind != "tcp" {
            usage_exit(&format!("unknown channel type {:?} -- must be 'mcu' or 'tcp'", kind));
        }
        let id: u8 = parts[1].parse()
            .unwrap_or_else(|_| usage_exit(&format!("channel id in {:?} must be 0-255", spec)));
        if !seen_ids.insert(id) {
            usage_exit(&format!("duplicate channel id {} in {:?}", id, spec));
        }

        match (kind, mode) {
            ("mcu", "exporter") => {
                if parts.len() != 4 {
                    usage_exit(&format!("exporter mcu spec {:?}: need mcu:<id>:<device>:<baud>", spec));
                }
                let baud: u32 = parts[3].parse()
                    .unwrap_or_else(|_| usage_exit(&format!("baud in {:?} is not a number", spec)));
                if !valid_bauds.contains(&baud) {
                    usage_exit(&format!("baud {} in {:?} is not a standard value", baud, spec));
                }
                out.push(ChannelSpec::McuExporter {
                    id, device: parts[2].to_string(), baud,
                });
            }
            ("mcu", "host") => {
                if parts.len() < 3 || parts.len() > 4 {
                    usage_exit(&format!("host mcu spec {:?}: need mcu:<id>:<symlink>[:<baud>]", spec));
                }
                let baud: u32 = if parts.len() == 4 {
                    parts[3].parse()
                        .unwrap_or_else(|_| usage_exit(&format!("baud in {:?} is not a number", spec)))
                } else { 230400 };
                if !valid_bauds.contains(&baud) {
                    usage_exit(&format!("baud {} in {:?} is not standard", baud, spec));
                }
                out.push(ChannelSpec::McuHost {
                    id, symlink: parts[2].to_string(), baud,
                });
            }
            ("tcp", mode_str) => {
                if parts.len() != 4 {
                    usage_exit(&format!("tcp spec {:?}: need tcp:<id>:<addr>:<port>", spec));
                }
                let port: u16 = parts[3].parse()
                    .unwrap_or_else(|_| usage_exit(&format!("port in {:?} must be 1-65535", spec)));
                if mode_str == "exporter" {
                    out.push(ChannelSpec::TcpExporter {
                        id, bind_addr: parts[2].to_string(), port,
                    });
                } else {
                    out.push(ChannelSpec::TcpHost {
                        id, dest_addr: parts[2].to_string(), port,
                    });
                }
            }
            _ => unreachable!(),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Build channel objects
// ---------------------------------------------------------------------------

fn build_channels(
    specs: &[ChannelSpec],
    poll: &mio::Poll,
) -> Vec<Box<dyn channel::Channel>> {
    let mut channels: Vec<Box<dyn channel::Channel>> = Vec::new();
    for spec in specs {
        match spec {
            ChannelSpec::McuExporter { id, device, baud } => {
                channels.push(Box::new(
                    McuChannel::new(*id, device.clone(), *baud, poll.registry())
                ));
            }
            ChannelSpec::McuHost { id, symlink, baud } => {
                channels.push(Box::new(PtyChannel::new(*id, symlink.clone(), *baud)));
            }
            ChannelSpec::TcpExporter { id, bind_addr, port } => {
                match TcpSourceChannel::new(*id, bind_addr, *port, poll.registry()) {
                    Ok(ch) => channels.push(Box::new(ch)),
                    Err(e) => {
                        tracing::error!(bind_addr, port, err = %e, "serialmux: failed to bind TCP");
                        std::process::exit(1);
                    }
                }
            }
            ChannelSpec::TcpHost { id, dest_addr, port } => {
                channels.push(Box::new(TcpDestChannel::new(*id, dest_addr.clone(), *port)));
            }
        }
    }
    channels
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    logging::init("serialmux");
    let args = parse_args();

    // If USB discovery is requested, block until the device appears once.
    if let Some((vid, pid)) = &args.usb_id {
        if crate::serial::find_acm_by_usb_id(vid, pid).is_none() {
            wait_for_acm(vid, pid);
            tracing::info!(vid, pid, "USB device found, starting daemon");
        }
    }

    let mut poll = mio::Poll::new().unwrap_or_else(|e| {
        tracing::error!(err = %e, "serialmux: cannot create poll");
        std::process::exit(1);
    });

    let channels = build_channels(&args.channels, &poll);

    let mut daemon = Daemon::new(
        args.mode,
        args.link_dev,
        args.usb_id,
        channels,
    );

    daemon.run(&mut poll);
}

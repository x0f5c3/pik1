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
use clap::{Args as ClapArgs, Parser, Subcommand};
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
#[derive(Debug, Clone)]
enum ChannelSpec {
    McuExporter {
        id: u8,
        device: String,
        baud: u32,
    },
    McuHost {
        id: u8,
        symlink: String,
        baud: u32,
    },
    TcpExporter {
        id: u8,
        bind_addr: String,
        port: u16,
    },
    TcpHost {
        id: u8,
        dest_addr: String,
        port: u16,
    },
}

struct Args {
    mode: String,
    link_dev: Option<String>,
    usb_id: Option<(String, String)>,
    channels: Vec<ChannelSpec>,
}

#[derive(Debug, Parser)]
#[command(
    name = "serialmux",
    long_about = DESCRIPTION,
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Exporter(ExporterArgs),
    Host(HostArgs),
}

#[derive(Debug, ClapArgs)]
#[command(arg_required_else_help = true)]
struct ExporterArgs {
    #[arg(
        value_name = "link_dev",
        conflicts_with = "usb_id",
        required_unless_present = "usb_id"
    )]
    link_dev: Option<String>,

    #[arg(
        long = "usb",
        value_name = "VID:PID",
        conflicts_with = "link_dev",
        required_unless_present = "link_dev",
        value_parser = parse_usb_id
    )]
    usb_id: Option<(String, String)>,

    #[arg(value_name = "channel_spec", required = true, value_parser = parse_exporter_channel_spec)]
    channels: Vec<ChannelSpec>,
}

#[derive(Debug, ClapArgs)]
#[command(arg_required_else_help = true)]
struct HostArgs {
    #[arg(
        value_name = "link_dev",
        conflicts_with = "usb_id",
        required_unless_present = "usb_id"
    )]
    link_dev: Option<String>,

    #[arg(
        long = "usb",
        value_name = "VID:PID",
        conflicts_with = "link_dev",
        required_unless_present = "link_dev",
        value_parser = parse_usb_id
    )]
    usb_id: Option<(String, String)>,

    #[arg(value_name = "channel_spec", required = true, value_parser = parse_host_channel_spec)]
    channels: Vec<ChannelSpec>,
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn parse_args() -> Args {
    match Cli::parse().command {
        Command::Exporter(args) => {
            validate_unique_channel_ids(&args.channels);
            Args {
                mode: "exporter".to_string(),
                link_dev: args.link_dev,
                usb_id: args.usb_id,
                channels: args.channels,
            }
        }
        Command::Host(args) => {
            validate_unique_channel_ids(&args.channels);
            Args {
                mode: "host".to_string(),
                link_dev: args.link_dev,
                usb_id: args.usb_id,
                channels: args.channels,
            }
        }
    }
}

fn parse_usb_id(val: &str) -> Result<(String, String), String> {
    let (vid, pid) = val
        .split_once(':')
        .ok_or_else(|| format!("--usb must be VID:PID (e.g. 1d6b:0104), got {:?}", val))?;

    for (part, name) in [(vid, "VID"), (pid, "PID")] {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{} in {:?} must be a hex string", name, val));
        }
    }
    Ok((vid.to_lowercase(), pid.to_lowercase()))
}

fn parse_exporter_channel_spec(spec: &str) -> Result<ChannelSpec, String> {
    parse_channel_spec("exporter", spec)
}

fn parse_host_channel_spec(spec: &str) -> Result<ChannelSpec, String> {
    parse_channel_spec("host", spec)
}

fn parse_channel_spec(mode: &str, spec: &str) -> Result<ChannelSpec, String> {
    let valid_bauds: &[u32] = &[
        1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600,
    ];

    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() < 3 {
        return Err(format!(
            "channel spec {:?} is too short\n\
            exporter mcu: mcu:<id>:<device>:<baud>\n\
            host     mcu: mcu:<id>:<pty_symlink>[:<baud>]\n\
            exporter tcp: tcp:<id>:<bind_addr>:<port>\n\
            host     tcp: tcp:<id>:<dest_addr>:<port>",
            spec
        ));
    }
    let kind = parts[0];
    if kind != "mcu" && kind != "tcp" {
        return Err(format!(
            "unknown channel type {:?} -- must be 'mcu' or 'tcp'",
            kind
        ));
    }
    let id = parts[1]
        .parse()
        .map_err(|_| format!("channel id in {:?} must be 0-255", spec))?;

    match (kind, mode) {
        ("mcu", "exporter") => {
            if parts.len() != 4 {
                return Err(format!(
                    "exporter mcu spec {:?}: need mcu:<id>:<device>:<baud>",
                    spec
                ));
            }
            let baud = parse_standard_baud(parts[3], spec, valid_bauds)?;
            Ok(ChannelSpec::McuExporter {
                id,
                device: parts[2].to_string(),
                baud,
            })
        }
        ("mcu", "host") => {
            if parts.len() < 3 || parts.len() > 4 {
                return Err(format!(
                    "host mcu spec {:?}: need mcu:<id>:<symlink>[:<baud>]",
                    spec
                ));
            }
            let baud = if parts.len() == 4 {
                parse_standard_baud(parts[3], spec, valid_bauds)?
            } else {
                230400
            };
            Ok(ChannelSpec::McuHost {
                id,
                symlink: parts[2].to_string(),
                baud,
            })
        }
        ("tcp", "exporter") => Ok(ChannelSpec::TcpExporter {
            id,
            bind_addr: parts
                .get(2)
                .ok_or_else(|| format!("tcp spec {:?}: need tcp:<id>:<addr>:<port>", spec))?
                .to_string(),
            port: parse_tcp_port(parts.get(3).copied(), spec)?,
        }),
        ("tcp", "host") => Ok(ChannelSpec::TcpHost {
            id,
            dest_addr: parts
                .get(2)
                .ok_or_else(|| format!("tcp spec {:?}: need tcp:<id>:<addr>:<port>", spec))?
                .to_string(),
            port: parse_tcp_port(parts.get(3).copied(), spec)?,
        }),
        _ => unreachable!(),
    }
}

fn parse_standard_baud(raw: &str, spec: &str, valid_bauds: &[u32]) -> Result<u32, String> {
    let baud = raw
        .parse()
        .map_err(|_| format!("baud in {:?} is not a number", spec))?;
    if !valid_bauds.contains(&baud) {
        return Err(format!(
            "baud {} in {:?} is not a standard value",
            baud, spec
        ));
    }
    Ok(baud)
}

fn parse_tcp_port(raw: Option<&str>, spec: &str) -> Result<u16, String> {
    let raw = raw.ok_or_else(|| format!("tcp spec {:?}: need tcp:<id>:<addr>:<port>", spec))?;
    raw.parse()
        .map_err(|_| format!("port in {:?} must be 1-65535", spec))
}

fn validate_unique_channel_ids(channels: &[ChannelSpec]) {
    let mut seen_ids = std::collections::HashSet::<u8>::new();
    for channel in channels {
        let id = match channel {
            ChannelSpec::McuExporter { id, .. }
            | ChannelSpec::McuHost { id, .. }
            | ChannelSpec::TcpExporter { id, .. }
            | ChannelSpec::TcpHost { id, .. } => *id,
        };
        if !seen_ids.insert(id) {
            clap::Error::raw(
                clap::error::ErrorKind::ValueValidation,
                format!("duplicate channel id {}", id),
            )
            .exit();
        }
    }
}

// ---------------------------------------------------------------------------
// Build channel objects
// ---------------------------------------------------------------------------

fn build_channels(specs: &[ChannelSpec], poll: &mio::Poll) -> Vec<Box<dyn channel::Channel>> {
    let mut channels: Vec<Box<dyn channel::Channel>> = Vec::new();
    for spec in specs {
        match spec {
            ChannelSpec::McuExporter { id, device, baud } => {
                channels.push(Box::new(McuChannel::new(
                    *id,
                    device.clone(),
                    *baud,
                    poll.registry(),
                )));
            }
            ChannelSpec::McuHost { id, symlink, baud } => {
                channels.push(Box::new(PtyChannel::new(*id, symlink.clone(), *baud)));
            }
            ChannelSpec::TcpExporter {
                id,
                bind_addr,
                port,
            } => match TcpSourceChannel::new(*id, bind_addr, *port, poll.registry()) {
                Ok(ch) => channels.push(Box::new(ch)),
                Err(e) => {
                    tracing::error!(bind_addr, port, err = %e, "serialmux: failed to bind TCP");
                    std::process::exit(1);
                }
            },
            ChannelSpec::TcpHost {
                id,
                dest_addr,
                port,
            } => {
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

    let mut daemon = Daemon::new(args.mode, args.link_dev, args.usb_id, channels);

    daemon.run(&mut poll);
}

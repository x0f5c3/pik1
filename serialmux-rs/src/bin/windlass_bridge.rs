//! `windlass-bridge` — native Klipper transport relay.
//!
//! A drop-in **alternative** to `serialmux` that bridges MCU UARTs to a
//! Klipper host using a simpler, more efficient tunnel format.
//!
//! **This binary is NOT compatible with the Python/C serialmux daemon.**
//! Both the K1 (exporter) and the Pi/CB1 (host) must run `windlass-bridge`.
//!
//! # Modes
//! | Mode       | Description |
//! |------------|-------------|
//! | `exporter` | Runs on the machine wired to the MCUs (K1 / K1C SoC). |
//! | `host`     | Runs on the Klipper host (Raspberry Pi / BTT CB1). |
//!
//! # Examples
//!
//! ```text
//! # K1C SoC (exporter):
//! windlass-bridge exporter --usb 1d6b:0104 \
//!     mcu:0:/dev/ttyS7:230400 \
//!     mcu:1:/dev/ttyS1:230400
//!
//! # Pi / CB1 (host):
//! windlass-bridge host /dev/ttyGS0 \
//!     mcu:0:/tmp/klipper_mcu0 \
//!     mcu:1:/tmp/klipper_mcu1
//!
//! # printer.cfg (host side):
//! [mcu]
//! serial: /tmp/klipper_mcu0
//! restart_method: command
//!
//! [mcu nozzle_mcu]
//! serial: /tmp/klipper_mcu1
//! restart_method: command
//! ```
//!
//! # Tunnel wire format
//!
//! ```text
//! [ ch_id : u8 ][ raw Klipper frame : 5..=64 bytes ]
//! ```
//!
//! The raw Klipper frame is self-delimiting (length byte + sync byte `0x7E`).
//! The tunnel adds only one byte of overhead per frame compared to ~10 bytes
//! for the serialmux envelope.

use clap::{Args as ClapArgs, Parser, Subcommand};
use serialmux::windlass::{
    McuSpec, exporter::run_exporter, host::run_host, resolve_link_device,
    smart_exporter::run_smart_exporter, smart_host::run_smart_host,
};

const DESCRIPTION: &str = r#"windlass-bridge -- native Klipper transport relay over USB CDC.

NOT COMPATIBLE with serialmux.py or the serialmux Rust binary.
Both ends (exporter and host) must run windlass-bridge.

MODES
  exporter   Run on the machine wired to the MCUs (K1 / K1C SoC).
  host       Run on the Klipper host (Pi / CB1).  Exposes Unix sockets
             that Klipper connects to instead of PTY devices.

OPTIONS
  --smart    Enable smart-proxy mode (windlass + anchor).  In this mode the
             Klipper transport session is terminated at each end of the USB
             link.  The MCU dictionary is forwarded to the host so Klipper's
             'identify' handshake is answered locally.  Both ends must use
             the same mode flag.

LINK DEVICE
  --usb VID:PID   Discover CDC ACM device by USB ID (waits for appearance).
  link_dev        Explicit device path (e.g. /dev/ttyGS0).
  One of these is required; they are mutually exclusive.

CHANNEL SPECS
  mcu:<id>:<uart_device>:<baud>   exporter: UART to bridge
  mcu:<id>:<socket_path>          host: Unix socket path for Klipper

EXAMPLES
  Transparent relay (default):
    windlass-bridge exporter --usb 1d6b:0104 \
        mcu:0:/dev/ttyS7:230400

    windlass-bridge host /dev/ttyGS0 \
        mcu:0:/tmp/klipper_mcu0

  Smart-proxy mode (windlass + anchor, recommended):
    windlass-bridge exporter --smart --usb 1d6b:0104 \
        mcu:0:/dev/ttyS7:230400

    windlass-bridge host --smart /dev/ttyGS0 \
        mcu:0:/tmp/klipper_mcu0

  printer.cfg:
    [mcu]
    serial: /tmp/klipper_mcu0
    restart_method: command
"#;

// ─────────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────────

struct Args {
    mode: String,
    smart: bool,
    link_dev: Option<String>,
    usb_id: Option<(String, String)>,
    channels: Vec<McuSpec>,
}

#[derive(Debug, Parser)]
#[command(
    name = "windlass-bridge",
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
    #[arg(long)]
    smart: bool,

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
    channels: Vec<McuSpec>,
}

#[derive(Debug, ClapArgs)]
#[command(arg_required_else_help = true)]
struct HostArgs {
    #[arg(long)]
    smart: bool,

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
    channels: Vec<McuSpec>,
}

fn parse_args() -> Args {
    match Cli::parse().command {
        Command::Exporter(args) => {
            validate_unique_channel_ids(&args.channels);
            Args {
                mode: "exporter".to_string(),
                smart: args.smart,
                link_dev: args.link_dev,
                usb_id: args.usb_id,
                channels: args.channels,
            }
        }
        Command::Host(args) => {
            validate_unique_channel_ids(&args.channels);
            Args {
                mode: "host".to_string(),
                smart: args.smart,
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

fn parse_exporter_channel_spec(spec: &str) -> Result<McuSpec, String> {
    parse_channel_spec("exporter", spec)
}

fn parse_host_channel_spec(spec: &str) -> Result<McuSpec, String> {
    parse_channel_spec("host", spec)
}

fn parse_channel_spec(mode: &str, spec: &str) -> Result<McuSpec, String> {
    let valid_bauds: &[u32] = &[
        1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600,
    ];

    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() < 3 {
        return Err(format!(
            "channel spec {:?} too short\n\
             exporter: mcu:<id>:<uart_device>:<baud>\n\
             host:     mcu:<id>:<socket_path>",
            spec
        ));
    }

    if parts[0] != "mcu" {
        return Err(format!(
            "unknown channel type {:?} — only 'mcu' is supported by windlass-bridge",
            parts[0]
        ));
    }

    let ch_id = parts[1]
        .parse()
        .map_err(|_| format!("channel id in {:?} must be 0-255", spec))?;

    if ch_id == 0xFF {
        return Err(format!(
            "channel id 255 in {:?} is reserved for the windlass smart-proxy control channel",
            spec
        ));
    }

    if mode == "exporter" {
        if parts.len() != 4 {
            return Err(format!(
                "exporter mcu spec {:?}: need mcu:<id>:<device>:<baud>",
                spec
            ));
        }
        let baud = parse_standard_baud(parts[3], spec, valid_bauds)?;
        Ok(McuSpec {
            ch_id,
            path: parts[2].to_string(),
            baud,
        })
    } else {
        if parts.len() != 3 {
            return Err(format!(
                "host mcu spec {:?}: need mcu:<id>:<socket_path>",
                spec
            ));
        }
        Ok(McuSpec {
            ch_id,
            path: parts[2].to_string(),
            baud: 0,
        })
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

fn validate_unique_channel_ids(channels: &[McuSpec]) {
    let mut seen_ids = std::collections::HashSet::<u8>::new();
    for channel in channels {
        if !seen_ids.insert(channel.ch_id) {
            clap::Error::raw(
                clap::error::ErrorKind::ValueValidation,
                format!("duplicate channel id {}", channel.ch_id),
            )
            .exit();
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// main
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    serialmux::logging::init("windlass-bridge");
    let args = parse_args();

    let link_device = resolve_link_device(
        args.link_dev.as_deref(),
        args.usb_id.as_ref().map(|(v, p)| (v.as_str(), p.as_str())),
    )
    .await;

    let result = if args.mode == "exporter" {
        if args.smart {
            run_smart_exporter(link_device, args.channels).await
        } else {
            run_exporter(link_device, args.channels).await
        }
    } else if args.smart {
        run_smart_host(link_device, args.channels).await
    } else {
        run_host(link_device, args.channels).await
    };

    if let Err(e) = result {
        tracing::error!(err = %e, "windlass-bridge: fatal error");
        std::process::exit(1);
    }
}

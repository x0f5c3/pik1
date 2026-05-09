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

use serialmux::windlass::{
    McuSpec,
    exporter::run_exporter,
    host::run_host,
    smart_exporter::run_smart_exporter,
    smart_host::run_smart_host,
    resolve_link_device,
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
    mode:     String,
    smart:    bool,
    link_dev: Option<String>,
    usb_id:   Option<(String, String)>,
    channels: Vec<McuSpec>,
}

fn usage_exit(msg: &str) -> ! {
    eprintln!("windlass-bridge: error: {}", msg);
    eprintln!("\nRun `windlass-bridge --help` for usage.");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() < 2 {
        usage_exit("too few arguments");
    }
    if raw[1] == "-h" || raw[1] == "--help" {
        print!("{}", DESCRIPTION);
        std::process::exit(0);
    }

    let mode = raw[1].clone();
    if mode != "exporter" && mode != "host" {
        usage_exit(&format!(
            "mode must be 'exporter' or 'host', got {:?}",
            mode
        ));
    }

    let mut link_dev: Option<String> = None;
    let mut usb_id: Option<(String, String)> = None;
    let mut channel_strs: Vec<String> = Vec::new();
    let mut smart = false;
    let mut i = 2usize;

    while i < raw.len() {
        let arg = &raw[i];
        if arg == "--smart" {
            smart = true;
        } else if arg == "--usb" {
            i += 1;
            if i >= raw.len() {
                usage_exit("--usb requires an argument");
            }
            usb_id = Some(parse_usb_id(&raw[i]));
        } else if let Some(val) = arg.strip_prefix("--usb=") {
            usb_id = Some(parse_usb_id(val));
        } else if arg.starts_with("mcu:") {
            channel_strs.push(arg.clone());
        } else if !arg.starts_with('-') {
            if link_dev.is_none() && usb_id.is_none() {
                link_dev = Some(arg.clone());
            } else if link_dev.is_none() {
                link_dev = Some(arg.clone());
            } else {
                channel_strs.push(arg.clone());
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
    if channel_strs.is_empty() {
        usage_exit("at least one mcu:<id>:<path>[:<baud>] spec is required");
    }

    let channels = parse_channel_specs(&mode, &channel_strs);
    Args { mode, smart, link_dev, usb_id, channels }
}

fn parse_usb_id(val: &str) -> (String, String) {
    let parts: Vec<&str> = val.splitn(2, ':').collect();
    if parts.len() != 2 {
        usage_exit(&format!(
            "--usb must be VID:PID (e.g. 1d6b:0104), got {:?}",
            val
        ));
    }
    for (part, name) in parts.iter().zip(["VID", "PID"].iter()) {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            usage_exit(&format!("{} in {:?} must be a hex string", name, val));
        }
    }
    (parts[0].to_lowercase(), parts[1].to_lowercase())
}

fn parse_channel_specs(mode: &str, specs: &[String]) -> Vec<McuSpec> {
    let valid_bauds: &[u32] = &[
        1200, 2400, 4800, 9600, 19200, 38400, 57600,
        115200, 230400, 460800, 921600,
    ];
    let mut seen_ids = std::collections::HashSet::<u8>::new();
    let mut out = Vec::new();

    for spec in specs {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 3 {
            usage_exit(&format!(
                "channel spec {:?} too short\n\
                 exporter: mcu:<id>:<uart_device>:<baud>\n\
                 host:     mcu:<id>:<socket_path>",
                spec
            ));
        }
        if parts[0] != "mcu" {
            usage_exit(&format!(
                "unknown channel type {:?} — only 'mcu' is supported by windlass-bridge",
                parts[0]
            ));
        }

        let ch_id: u8 = parts[1]
            .parse()
            .unwrap_or_else(|_| usage_exit(&format!("channel id in {:?} must be 0-255", spec)));
        if !seen_ids.insert(ch_id) {
            usage_exit(&format!("duplicate channel id {} in {:?}", ch_id, spec));
        }

        if mode == "exporter" {
            if parts.len() != 4 {
                usage_exit(&format!(
                    "exporter mcu spec {:?}: need mcu:<id>:<device>:<baud>",
                    spec
                ));
            }
            let baud: u32 = parts[3].parse().unwrap_or_else(|_| {
                usage_exit(&format!("baud in {:?} is not a number", spec))
            });
            if !valid_bauds.contains(&baud) {
                usage_exit(&format!(
                    "baud {} in {:?} is not a standard value",
                    baud, spec
                ));
            }
            out.push(McuSpec {
                ch_id,
                path: parts[2].to_string(),
                baud,
            });
        } else {
            // host: mcu:<id>:<socket_path>  (baud field ignored)
            if parts.len() < 3 || parts.len() > 4 {
                usage_exit(&format!(
                    "host mcu spec {:?}: need mcu:<id>:<socket_path>",
                    spec
                ));
            }
            out.push(McuSpec {
                ch_id,
                path: parts[2].to_string(),
                baud: 0,
            });
        }
    }
    out
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
        args.usb_id
            .as_ref()
            .map(|(v, p)| (v.as_str(), p.as_str())),
    );

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
        eprintln!("windlass-bridge: fatal: {}", e);
        std::process::exit(1);
    }
}

//! Native Klipper transport relay — the `windlass` feature module.
//!
//! This module is compiled only when the `windlass` Cargo feature is enabled.
//! It backs the `windlass-bridge` binary.
//!
//! # Architecture
//!
//! ```text
//! Transparent relay:
//!   K1C SoC (exporter):
//!     ttyS7 ──► [KlipperFramer] ──► ch_id prefix ──► CDC ACM
//!
//!   Pi/CB1 (host):
//!     CDC ACM ──► [TunnelCodec] ──► demux by ch_id
//!                                        │
//!                                        ▼
//!                                Unix socket → Klipper
//! ```
//!
//! Smart proxy:
//! - [`smart_exporter`] fetches the raw MCU dictionary via `windlass::McuConnection`,
//!   then relays decoded payloads via `windlass::Transport`.
//! - [`smart_host`] terminates Klipper's transport with `anchor`, handles
//!   `identify` locally, and forwards other commands as raw payloads.
//!
//! ## Tunnel wire format
//!
//! The CDC ACM link carries different tunnel frame formats depending on the
//! operating mode:
//!
//! Transparent relay:
//!
//! ```text
//! [ ch_id : u8 ][ raw Klipper frame : 5..=64 bytes ]
//! ```
//!
//! The raw Klipper frame is self-delimiting: its first byte is the total
//! frame length (5–64), and its last byte is always `0x7E` (sync). No
//! additional magic bytes, length prefixes, or CRCs are added by the
//! tunnel layer in this mode; Klipper's own CRC-16 already covers frame
//! integrity.
//!
//! Smart proxy:
//!
//! ```text
//! [ ch_id : u8 ][ payload_len : u8 ][ payload : 0..=255 bytes ]
//! ```
//!
//! In smart-proxy mode, the tunnel transports decoded payload bytes rather
//! than full raw Klipper frames. `payload_len` gives the number of payload
//! bytes that follow for the selected channel.
//!
//! ## Compatibility
//!
//! This protocol is **not** compatible with the C/Python `serialmux` daemon.
//! Users who require TCP channel tunnelling (e.g. grumpyscreen → Moonraker)
//! must continue using the `serialmux` binary.
//!
pub mod async_serial;
pub mod exporter;
pub mod framing;
pub mod host;
pub mod smart_exporter;
pub mod smart_host;

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A parsed MCU channel specification from the CLI.
#[derive(Debug, Clone)]
pub struct McuSpec {
    /// Channel index (0–255).  Must be unique across all channels.
    pub ch_id: u8,
    /// Exporter: UART device path.  Host: Unix socket path.
    pub path: String,
    /// Baud rate.  Ignored by the host (socket has no baud rate).
    pub baud: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// USB device discovery (mirrors serial.rs without the mio dependency)
// ─────────────────────────────────────────────────────────────────────────────

/// Scan `/sys/class/tty` for a `ttyACM*` device whose USB parent matches
/// the given `vid:pid` strings (lowercase hex, no `0x` prefix).
///
/// Returns the first match as `"/dev/ttyACMn"`, or `None`.
pub fn find_acm_by_usb_id(vid: &str, pid: &str) -> Option<String> {
    let base = std::path::Path::new("/sys/class/tty");
    let mut entries: Vec<_> = std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("ttyACM"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let mut cur = std::fs::canonicalize(entry.path()).unwrap_or_else(|_| entry.path());

        for _ in 0..8 {
            let vendor_p = cur.join("idVendor");
            let product_p = cur.join("idProduct");
            match (
                std::fs::read_to_string(&vendor_p),
                std::fs::read_to_string(&product_p),
            ) {
                (Ok(v), Ok(p)) => {
                    if v.trim() == vid && p.trim() == pid {
                        return Some(format!("/dev/{}", name));
                    }
                    break;
                }
                _ => {}
            }
            let parent = cur.parent().map(|p| p.to_path_buf());
            match parent {
                Some(p) if p != cur => cur = p,
                _ => break,
            }
        }
    }
    None
}

/// Block (synchronously) until a `ttyACM*` device matching `vid:pid` appears.
///
/// Logs progress every 10 seconds.  Returns the device path.
pub fn wait_for_acm(vid: &str, pid: &str) -> String {
    let mut last_log = Instant::now() - Duration::from_secs(11);
    loop {
        if let Some(dev) = find_acm_by_usb_id(vid, pid) {
            return dev;
        }
        let now = Instant::now();
        if now.duration_since(last_log) >= Duration::from_secs(10) {
            tracing::info!(
                vid,
                pid,
                "windlass-bridge: USB device not yet present, waiting"
            );
            last_log = now;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Resolve a link device: either a direct path or a USB-discovered path.
///
/// Blocks until the device appears if USB discovery is requested.
pub fn resolve_link_device(link_dev: Option<&str>, usb_id: Option<(&str, &str)>) -> String {
    if let Some(dev) = link_dev {
        return dev.to_string();
    }
    if let Some((vid, pid)) = usb_id {
        return wait_for_acm(vid, pid);
    }
    unreachable!("caller must supply link_dev or usb_id")
}

/// Ensure the parent directory of `path` exists, then remove any stale Unix
/// socket or symlink at that path so `UnixListener::bind` won't fail.
///
/// Returns an error if `path` already exists as a non-socket, non-symlink file.
pub fn prepare_socket_path(path: &str) -> std::io::Result<()> {
    use std::io;
    use std::os::unix::fs::FileTypeExt as _;

    let p = PathBuf::from(path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }

    match std::fs::symlink_metadata(&p) {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_socket() || file_type.is_symlink() {
                std::fs::remove_file(&p)?;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("refusing to remove non-socket file at {}", p.display()),
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::prepare_socket_path;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let mut path = std::env::temp_dir();
            let unique = format!(
                "serialmux-windlass-test-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            );
            path.push(unique);
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn path_str(path: &Path) -> &str {
        path.to_str().unwrap()
    }

    #[test]
    fn prepare_socket_path_removes_stale_socket() {
        let dir = TestDir::new();
        let socket_path = dir.join("klipper.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();

        prepare_socket_path(path_str(&socket_path)).unwrap();

        assert!(!socket_path.exists());
    }

    #[test]
    fn prepare_socket_path_removes_symlink() {
        let dir = TestDir::new();
        let target = dir.join("target");
        let link = dir.join("klipper.sock");
        fs::write(&target, b"target").unwrap();
        symlink(&target, &link).unwrap();

        prepare_socket_path(path_str(&link)).unwrap();

        assert!(!link.exists());
        assert!(target.exists());
    }

    #[test]
    fn prepare_socket_path_rejects_regular_file() {
        let dir = TestDir::new();
        let file_path = dir.join("not-a-socket");
        fs::write(&file_path, b"regular file").unwrap();

        let err = prepare_socket_path(path_str(&file_path)).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(file_path.exists());
    }
}

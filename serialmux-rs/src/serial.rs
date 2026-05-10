//! Serial-port, PTY, and USB-discovery helpers.
//!
//! All public functions are safe to call; unsafe syscall code is confined to
//! the private low-level wrappers below, each annotated with a `// SAFETY:`
//! comment explaining the required invariants.

use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ─────────────────────────────────────────────────────────────────────────────
// File-descriptor helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Set `O_NONBLOCK` on a file descriptor.
pub fn set_nonblock(fd: RawFd) -> std::io::Result<()> {
    use libc::{F_GETFL, F_SETFL, O_NONBLOCK};
    // SAFETY: `fd` is a valid open file descriptor; F_GETFL / F_SETFL only
    // read or update flags and have no memory-safety implications.
    let flags = unsafe { libc::fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same `fd`; F_SETFL with the updated flags is safe.
    let rc = unsafe { libc::fcntl(fd, F_SETFL, flags | O_NONBLOCK) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Close a raw file descriptor, ignoring any error.
///
/// The caller must not use `fd` after this call.
pub(crate) fn close_raw(fd: RawFd) {
    // SAFETY: `fd` is a valid open file descriptor and ownership is being
    // transferred here; it will not be used again.
    unsafe { libc::close(fd) };
}

/// Non-blocking read from `fd` into `buf`.
///
/// Returns `Ok(Some(n))` on success, `Ok(None)` on `EAGAIN` /
/// `EWOULDBLOCK`, or `Err` on a real I/O error.
pub fn read_nonblock(fd: RawFd, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
    // SAFETY: `fd` is a valid non-blocking fd; `buf` is valid for writes
    // of up to `buf.len()` bytes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error()
            .map_or(false, |c| c == libc::EAGAIN || c == libc::EWOULDBLOCK)
        {
            return Ok(None);
        }
        return Err(e);
    }
    Ok(Some(n as usize))
}

/// Non-blocking write to `fd`.
///
/// Returns `Ok(n)` bytes written; `Ok(0)` means `EAGAIN` / `EWOULDBLOCK`.
pub fn write_nonblock(fd: RawFd, buf: &[u8]) -> std::io::Result<usize> {
    // SAFETY: `fd` is a valid non-blocking fd; `buf` is valid for reads
    // of up to `buf.len()` bytes.
    let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error()
            .map_or(false, |c| c == libc::EAGAIN || c == libc::EWOULDBLOCK)
        {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(n as usize)
}

// ─────────────────────────────────────────────────────────────────────────────
// Serial port
// ─────────────────────────────────────────────────────────────────────────────

/// Open `dev` as a raw non-blocking serial port at `baud` bps.
///
/// Applies the same `termios` settings as the C `open_serial_fd()`:
/// all of `c_iflag`, `c_oflag`, `c_lflag` cleared; `CS8 | CREAD | CLOCAL`;
/// `VMIN = 0`, `VTIME = 0`.
pub fn open_serial_fd(dev: &str, baud: u32) -> std::io::Result<RawFd> {
    use libc::{O_NOCTTY, O_NONBLOCK, O_RDWR};
    let path = std::ffi::CString::new(dev).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid device path")
    })?;
    // SAFETY: `path` is a valid NUL-terminated C string; the flags are
    // well-defined open(2) constants.
    let fd = unsafe { libc::open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if let Err(e) = apply_termios(fd, baud) {
        close_raw(fd);
        return Err(e);
    }
    Ok(fd)
}

/// Map a numeric baud rate to a `nix` [`BaudRate`](nix::sys::termios::BaudRate).
///
/// Supports 1200 – 921600.  Returns `None` for unsupported values.
fn baud_to_speed(baud: u32) -> Option<nix::sys::termios::BaudRate> {
    use nix::sys::termios::BaudRate::*;
    Some(match baud {
        1200 => B1200,
        2400 => B2400,
        4800 => B4800,
        9600 => B9600,
        19200 => B19200,
        38400 => B38400,
        57600 => B57600,
        115200 => B115200,
        230400 => B230400,
        460800 => B460800,
        921600 => B921600,
        _ => return None,
    })
}

/// Configure `fd` for raw binary communication at `baud` bps.
fn apply_termios(fd: RawFd, baud: u32) -> std::io::Result<()> {
    use nix::sys::termios::{
        ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices as SCI,
        cfsetspeed, tcgetattr, tcsetattr,
    };
    let speed = baud_to_speed(baud).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsupported baud rate: {}", baud),
        )
    })?;
    // SAFETY: `fd` is a valid open terminal file descriptor.
    let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut attrs = tcgetattr(bfd).map_err(nix_to_io)?;
    attrs.input_flags = InputFlags::empty();
    attrs.output_flags = OutputFlags::empty();
    attrs.control_flags = ControlFlags::CREAD | ControlFlags::CLOCAL | ControlFlags::CS8;
    attrs.local_flags = LocalFlags::empty();
    attrs.control_chars[SCI::VMIN as usize] = 0;
    attrs.control_chars[SCI::VTIME as usize] = 0;
    cfsetspeed(&mut attrs, speed).map_err(nix_to_io)?;
    // SAFETY: same `fd`.
    let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    tcsetattr(bfd, SetArg::TCSAFLUSH, &attrs).map_err(nix_to_io)?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// PTY helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Open a raw PTY pair configured for `baud` bps.
///
/// Returns `(master_fd, slave_fd)`.  The slave fd is kept open so the pair
/// remains alive; close the slave to generate `EIO` on the master.
pub fn open_pty_raw(baud: u32) -> std::io::Result<(RawFd, RawFd)> {
    use std::os::unix::io::IntoRawFd;
    let result = nix::pty::openpty(None, None).map_err(nix_to_io)?;
    let master_fd: RawFd = result.master.into_raw_fd();
    let slave_fd: RawFd = result.slave.into_raw_fd();
    apply_termios(slave_fd, baud)?;
    set_nonblock(master_fd)?;
    Ok((master_fd, slave_fd))
}

/// Return the filesystem path of a PTY slave fd via `/proc/self/fd`.
pub fn pty_slave_name(slave_fd: RawFd) -> std::io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", slave_fd))
}

// ─────────────────────────────────────────────────────────────────────────────
// USB sysfs discovery
// ─────────────────────────────────────────────────────────────────────────────

/// Scan `/sys/class/tty` for a `ttyACM*` device whose USB parent matches
/// `vid:pid`.
///
/// Returns the first match as `"/dev/ttyACMn"`, or `None`.
///
/// The sysfs tree is walked upward from each `ttyACM*` entry (up to 8
/// levels) until `idVendor` / `idProduct` files are found.  If they match a
/// different device, the search stops for that entry (no further climbing).
pub fn find_acm_by_usb_id(vid: &str, pid: &str) -> Option<String> {
    let base = std::path::Path::new("/sys/class/tty");
    let mut entries: Vec<_> = std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("ttyACM"))
        .collect();
    // Sort for deterministic selection (lowest ttyACMn wins on a tie).
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
                    // Found the USB node but VID/PID don't match; don't climb.
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

/// Block until a `ttyACM*` device matching `vid:pid` appears.
///
/// Logs a message to stderr every 10 seconds while waiting.
pub fn wait_for_acm(vid: &str, pid: &str) -> String {
    let mut last_log = Instant::now() - Duration::from_secs(11);
    loop {
        if let Some(dev) = find_acm_by_usb_id(vid, pid) {
            return dev;
        }
        let now = Instant::now();
        if now.duration_since(last_log) >= Duration::from_secs(10) {
            tracing::info!(vid, pid, "USB device not yet present, waiting…");
            last_log = now;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn nix_to_io(e: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e as i32)
}

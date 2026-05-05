//! Async serial-port helper.
//!
//! Wraps a raw file descriptor (opened with `O_RDWR | O_NOCTTY | O_NONBLOCK`)
//! in a [`tokio::io::unix::AsyncFd`] and implements [`AsyncRead`] + [`AsyncWrite`]
//! so it can be used with `tokio_util::codec::Framed` and `tokio::io::split`.
//!
//! [`open_serial`] opens the device, applies the requested `termios` settings,
//! and returns an [`AsyncSerial`] ready for async I/O.

use std::io::{self, Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// ─────────────────────────────────────────────────────────────────────────────
// AsyncSerial
// ─────────────────────────────────────────────────────────────────────────────

/// Async wrapper for a raw serial-port (or CDC ACM) file descriptor.
///
/// Implements [`AsyncRead`] and [`AsyncWrite`] via the tokio `AsyncFd`
/// readiness API, enabling use with `tokio_util::codec::Framed`.
pub struct AsyncSerial(AsyncFd<std::fs::File>);

impl AsyncSerial {
    /// Wrap a raw file descriptor that is already open with `O_NONBLOCK`.
    ///
    /// Takes ownership of the fd; it will be closed when `AsyncSerial` is
    /// dropped.
    ///
    /// # Safety
    ///
    /// `fd` must be a valid, open, non-blocking file descriptor.  The caller
    /// must not use `fd` after calling this function.
    unsafe fn from_raw_fd(fd: RawFd) -> io::Result<Self> {
        // SAFETY: We take ownership; std::fs::File::from_raw_fd is unsafe for
        // the same reason — the caller guarantees the fd is valid and owned.
        let file = std::fs::File::from_raw_fd(fd);
        Ok(AsyncSerial(AsyncFd::new(file)?))
    }
}

impl AsyncRead for AsyncSerial {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = match self.0.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|inner| inner.get_ref().read(unfilled)) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for AsyncSerial {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = match self.0.poll_write_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            match guard.try_io(|inner| inner.get_ref().write(data)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// open_serial
// ─────────────────────────────────────────────────────────────────────────────

/// Open `device` as a non-blocking raw serial port at `baud` bps.
///
/// - `O_RDWR | O_NOCTTY | O_NONBLOCK` open flags.
/// - All `termios` flags cleared (`cfmakeraw` style).
/// - `VMIN = 0`, `VTIME = 0`.
/// - Baud set via `cfsetspeed`.
///
/// Passing `baud = 0` skips the baud-rate configuration (useful for USB CDC
/// ACM devices where the baud setting is advisory / ignored by the driver).
pub fn open_serial(device: &str, baud: u32) -> io::Result<AsyncSerial> {
    use libc::{O_NOCTTY, O_NONBLOCK, O_RDWR};

    let path = std::ffi::CString::new(device).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid device path (contains NUL)")
    })?;

    // SAFETY: `path` is a valid NUL-terminated C string.  The flags are
    // well-defined `open(2)` constants.
    let fd = unsafe { libc::open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    if let Err(e) = apply_termios(fd, baud) {
        // SAFETY: `fd` was just opened and we are discarding it on error.
        unsafe { libc::close(fd) };
        return Err(e);
    }

    // SAFETY: `fd` is valid, open, and we are transferring ownership.
    unsafe { AsyncSerial::from_raw_fd(fd) }
}

// ─────────────────────────────────────────────────────────────────────────────
// termios helpers
// ─────────────────────────────────────────────────────────────────────────────

fn apply_termios(fd: RawFd, baud: u32) -> io::Result<()> {
    use nix::sys::termios::{
        cfsetspeed, tcgetattr, tcsetattr, ControlFlags, InputFlags, LocalFlags, OutputFlags,
        SetArg, SpecialCharacterIndices as SCI,
    };

    // SAFETY: `fd` is a valid open terminal file descriptor.
    let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut attrs = tcgetattr(bfd).map_err(nix_to_io)?;

    attrs.input_flags = InputFlags::empty();
    attrs.output_flags = OutputFlags::empty();
    attrs.control_flags = ControlFlags::CREAD | ControlFlags::CLOCAL | ControlFlags::CS8;
    attrs.local_flags = LocalFlags::empty();
    attrs.control_chars[SCI::VMIN as usize] = 0;
    attrs.control_chars[SCI::VTIME as usize] = 0;

    if baud != 0 {
        let speed = baud_to_nix(baud).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported baud rate: {}", baud),
            )
        })?;
        cfsetspeed(&mut attrs, speed).map_err(nix_to_io)?;
    }

    // SAFETY: same `fd`.
    let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    tcsetattr(bfd, SetArg::TCSAFLUSH, &attrs).map_err(nix_to_io)?;
    Ok(())
}

fn baud_to_nix(baud: u32) -> Option<nix::sys::termios::BaudRate> {
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

pub(crate) fn nix_to_io(e: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

// ─────────────────────────────────────────────────────────────────────────────
// Retrieve the device path for a raw fd via /proc/self/fd (for logging).
// ─────────────────────────────────────────────────────────────────────────────

/// Return the filesystem path that `fd` refers to (via `/proc/self/fd`).
/// Returns `None` if resolution fails (non-Linux or permission error).
#[allow(dead_code)]
pub fn fd_path(fd: RawFd) -> Option<String> {
    std::fs::read_link(format!("/proc/self/fd/{}", fd))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

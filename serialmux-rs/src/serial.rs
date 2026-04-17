use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Write a timestamped log line to stderr (mirrors Python's _log()).
pub fn log(msg: &str) {
    eprintln!("{} {}", timestamp(), msg);
}

// ---------------------------------------------------------------------------
// TTY / serial helpers
// ---------------------------------------------------------------------------

/// Open `dev` as a raw non-blocking serial port at `baud`.
pub fn open_serial_fd(dev: &str, baud: u32) -> std::io::Result<RawFd> {
    use libc::{O_RDWR, O_NOCTTY, O_NONBLOCK};
    let path = std::ffi::CString::new(dev).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid device path")
    })?;
    let fd = unsafe { libc::open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    apply_termios(fd, baud)?;
    Ok(fd)
}

fn baud_to_speed(baud: u32) -> Option<nix::sys::termios::BaudRate> {
    use nix::sys::termios::BaudRate::*;
    Some(match baud {
        9600    => B9600,
        19200   => B19200,
        38400   => B38400,
        57600   => B57600,
        115200  => B115200,
        230400  => B230400,
        460800  => B460800,
        921600  => B921600,
        _       => return None,
    })
}

fn apply_termios(fd: RawFd, baud: u32) -> std::io::Result<()> {
    use nix::sys::termios::{
        tcgetattr, tcsetattr, cfsetspeed,
        InputFlags, OutputFlags, ControlFlags, LocalFlags,
        SetArg, SpecialCharacterIndices as SCI,
    };
    let speed = baud_to_speed(baud)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput,
                                           format!("unsupported baud rate: {}", baud)))?;
    let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut attrs = tcgetattr(bfd).map_err(nix_to_io)?;
    attrs.input_flags  = InputFlags::empty();
    attrs.output_flags = OutputFlags::empty();
    attrs.control_flags = ControlFlags::CREAD | ControlFlags::CLOCAL | ControlFlags::CS8;
    attrs.local_flags  = LocalFlags::empty();
    attrs.control_chars[SCI::VMIN  as usize] = 0;
    attrs.control_chars[SCI::VTIME as usize] = 0;
    cfsetspeed(&mut attrs, speed).map_err(nix_to_io)?;
    tcsetattr(bfd, SetArg::TCSAFLUSH, &attrs).map_err(nix_to_io)?;
    Ok(())
}

/// Open a raw PTY pair configured for `baud`.  Returns `(master_fd, slave_fd)`.
/// The slave is left open so the PTY pair remains alive; the caller may close
/// it when they want to generate EIO on the master.
pub fn open_pty_raw(baud: u32) -> std::io::Result<(RawFd, RawFd)> {
    use std::os::unix::io::IntoRawFd;
    use nix::pty::openpty;
    let result = openpty(None, None).map_err(nix_to_io)?;
    let master_fd: RawFd = result.master.into_raw_fd();
    let slave_fd:  RawFd = result.slave.into_raw_fd();
    // Configure slave in raw mode at the requested baud rate.
    apply_termios(slave_fd, baud)?;
    // Make master non-blocking.
    set_nonblock(master_fd)?;
    Ok((master_fd, slave_fd))
}

/// Return the path of a PTY slave fd via /proc/self/fd.
pub fn pty_slave_name(slave_fd: RawFd) -> std::io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", slave_fd))
}

/// Set O_NONBLOCK on a file descriptor.
pub fn set_nonblock(fd: RawFd) -> std::io::Result<()> {
    use libc::{F_GETFL, F_SETFL, O_NONBLOCK, fcntl};
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let rc = unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn nix_to_io(e: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e as i32)
}

// ---------------------------------------------------------------------------
// Non-blocking read / write helpers for raw fds
// ---------------------------------------------------------------------------

/// Non-blocking read from `fd` into `buf`.
/// Returns `Ok(Some(n))` on success, `Ok(None)` on EAGAIN, `Err(_)` on error.
pub fn read_nonblock(fd: RawFd, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error().map_or(false, |c| c == libc::EAGAIN || c == libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(e);
    }
    Ok(Some(n as usize))
}

/// Non-blocking write to `fd`.
/// Returns `Ok(n)` bytes written; 0 means EAGAIN.
pub fn write_nonblock(fd: RawFd, buf: &[u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::write(fd, buf.as_ptr() as *const _, buf.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error().map_or(false, |c| c == libc::EAGAIN || c == libc::EWOULDBLOCK) {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(n as usize)
}

// ---------------------------------------------------------------------------
// USB sysfs discovery
// ---------------------------------------------------------------------------

/// Scan /sys/class/tty for a ttyACM* whose USB parent matches `vid:pid`.
/// Returns the first match as "/dev/ttyACMn", or None.
pub fn find_acm_by_usb_id(vid: &str, pid: &str) -> Option<String> {
    let base = std::path::Path::new("/sys/class/tty");
    let mut entries: Vec<_> = std::fs::read_dir(base).ok()?
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
            match (std::fs::read_to_string(&vendor_p), std::fs::read_to_string(&product_p)) {
                (Ok(v), Ok(p)) => {
                    if v.trim() == vid && p.trim() == pid {
                        return Some(format!("/dev/{}", name));
                    }
                    break; // found the USB node but VID/PID don't match
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

/// Block until a ttyACM* matching `vid:pid` appears.
pub fn wait_for_acm(vid: &str, pid: &str) -> String {
    let mut logged = false;
    let mut last_log = Instant::now() - Duration::from_secs(11);
    loop {
        if let Some(dev) = find_acm_by_usb_id(vid, pid) {
            return dev;
        }
        let now = Instant::now();
        if !logged || now.duration_since(last_log) >= Duration::from_secs(10) {
            eprintln!("{} USB {}:{} -- waiting for device to appear...",
                      timestamp(), vid, pid);
            logged = true;
            last_log = now;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub fn timestamp() -> String {
    // Wall-clock time (HH:MM:SS) to match the Python _log() format.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let h = (secs / 3600) % 24;
    let m = (secs / 60)   % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

//! Channel implementations.
//!
//! Each channel type wraps one or more file descriptors and speaks a specific
//! sub-protocol over the shared link.  The [`Channel`] trait provides the
//! uniform interface used by [`crate::daemon::Daemon`].
//!
//! Channel types:
//! - [`McuChannel`]       – UART ↔ F_DATA frames (exporter side)
//! - [`PtyChannel`]       – PTY  ↔ F_DATA frames (host side)
//! - [`TcpSourceChannel`] – TCP listener → F_TCONN/F_TDATA/F_TCLOSE (exporter)
//! - [`TcpDestChannel`]   – TCP connector ← F_TCONN/F_TDATA/F_TCLOSE (host)

use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};

use crate::protocol::{
    F_DATA, F_FLUSH, F_READY, F_TCLOSE, F_TCONN, F_TDATA, KLIPPER_SYNC, MAX_PAYLOAD, TxQueue,
    build_frame,
};
use crate::serial::{
    close_raw, open_pty_raw, open_serial_fd, pty_slave_name, read_nonblock, write_nonblock,
};

// ─────────────────────────────────────────────────────────────────────────────
// Token layout
// ─────────────────────────────────────────────────────────────────────────────
// Token(0)               – link fd (managed by Daemon)
// Token(1..=63)          – channel primary fd  (channel N → token N+1, N=0..=MAX_CHANNEL_ID)
// Token(TCP_BASE + N*MAX_TCP_CONNS + slot) – TCP connection slot for channel N
// ─────────────────────────────────────────────────────────────────────────────

/// mio token reserved for the link fd.
pub const TOKEN_LINK: Token = Token(0);

/// Base token for TCP connection slots.
const TCP_BASE: usize = 64;
/// Highest channel id that can be represented by primary-fd tokens.
pub const MAX_CHANNEL_ID: u8 = 62;

/// Maximum concurrent TCP connections per channel.  Matches the C
/// `MAX_TCP_CONNS` constant so connection IDs are interchangeable on the wire.
const MAX_TCP_CONNS: usize = 256;

/// Compute the mio token for a channel's primary fd (UART / PTY / listen socket).
pub fn primary_token(ch_id: u8) -> Token {
    debug_assert!(
        ch_id <= MAX_CHANNEL_ID,
        "channel id {} exceeds supported maximum {}",
        ch_id,
        MAX_CHANNEL_ID
    );
    Token(ch_id as usize + 1)
}

/// Compute the mio token for a TCP connection slot inside a channel.
pub fn tcp_slot_token(ch_id: u8, slot: usize) -> Token {
    Token(TCP_BASE + ch_id as usize * MAX_TCP_CONNS + slot)
}

/// Reverse-map any token to its owning channel id.
///
/// Returns `None` for `TOKEN_LINK`.
pub fn channel_id_for_token(token: Token) -> Option<u8> {
    match token.0 {
        0 => None,
        1..=63 => Some((token.0 - 1) as u8),
        t => {
            let id = (t - TCP_BASE) / MAX_TCP_CONNS;
            if id <= MAX_CHANNEL_ID as usize {
                Some(id as u8)
            } else {
                None
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Channel trait
// ─────────────────────────────────────────────────────────────────────────────

/// Uniform interface implemented by all channel types.
pub trait Channel {
    /// The wire channel id (0–255).
    fn channel_id(&self) -> u8;

    /// A frame arrived from the far side of the link.
    fn on_frame(&mut self, ftype: u8, payload: &[u8], txq: &mut TxQueue, reg: &Registry);

    /// The link just completed its handshake.
    fn on_link_connect(&mut self, txq: &mut TxQueue, reg: &Registry);

    /// The link dropped.  Close all associated I/O and clean up state.
    fn on_link_disconnect(&mut self, reg: &Registry);

    /// Periodic timer tick.  Handles UART reopen, MCU silence timeout, etc.
    fn tick(&mut self, now: Instant, txq: &mut TxQueue, reg: &Registry);

    /// Earliest instant at which [`tick`] must run, or `None` if not needed.
    fn next_deadline(&self) -> Option<Instant>;

    /// A mio event fired for one of this channel's registered fds.
    fn handle_event(
        &mut self,
        token: Token,
        readable: bool,
        writable: bool,
        txq: &mut TxQueue,
        reg: &Registry,
    );

    /// Close all fds and deregister from mio.
    ///
    /// Called on controlled teardown.  The daemon runs indefinitely and
    /// channels clean themselves up in [`on_link_disconnect`], so this method
    /// is available for completeness and future use.
    #[allow(dead_code)]
    fn close(&mut self, reg: &Registry);

    /// Suspend inbound reads (backpressure: link TX queue above high-water).
    fn pause_source_reads(&mut self, reg: &Registry);

    /// Resume inbound reads (link TX queue drained below low-water).
    fn resume_source_reads(&mut self, reg: &Registry);
}

// ─────────────────────────────────────────────────────────────────────────────
// mio registry helpers
// ─────────────────────────────────────────────────────────────────────────────

fn reg_add(reg: &Registry, fd: RawFd, token: Token, interest: Interest) {
    if let Err(e) = reg.register(&mut SourceFd(&fd), token, interest) {
        tracing::warn!(fd, ?token, err = %e, "mio register failed");
    }
}

fn reg_mod(reg: &Registry, fd: RawFd, token: Token, interest: Interest) {
    if let Err(e) = reg.reregister(&mut SourceFd(&fd), token, interest) {
        tracing::warn!(fd, ?token, err = %e, "mio reregister failed");
    }
}

fn reg_del(reg: &Registry, fd: RawFd) {
    let _ = reg.deregister(&mut SourceFd(&fd));
}

// ─────────────────────────────────────────────────────────────────────────────
// McuChannel  (exporter side: UART ↔ F_DATA frames)
// ─────────────────────────────────────────────────────────────────────────────

/// MCU state machine, matching the C `mcu_state_t` enum.
#[derive(Debug, PartialEq, Clone, Copy)]
enum McuState {
    /// Initial / waiting for Klipper sync byte.
    Init,
    /// Klipper is running; forward all UART bytes.
    Active,
    /// MCU reset detected (silence timeout); drain and wait for sync.
    Resetting,
}

/// Exporter-side MCU channel.
///
/// Reads bytes from a hardware UART and forwards them to the host as `F_DATA`
/// frames.  Waits for Klipper's sync byte (`0x7E`) before declaring the MCU
/// active, and transitions back to `Resetting` after [`RESET_SILENCE`] of
/// silence.
pub struct McuChannel {
    ch_id: u8,
    device: String,
    baud: u32,
    state: McuState,
    fd: Option<RawFd>,
    fd_in_sel: bool,
    /// Pending bytes to write to the UART (from the host).
    txbuf: Vec<u8>,
    last_rx: Option<Instant>,
    reopen_at: Option<Instant>,
    bp_paused: bool,
    link_up: bool,
}

/// How long the MCU must be silent before we declare a reset.
const RESET_SILENCE: Duration = Duration::from_secs(5);
/// How long to wait before re-opening a failed UART.
const REOPEN_DELAY: Duration = Duration::from_secs(2);

impl McuChannel {
    /// Create a new MCU channel and immediately attempt to open the UART.
    pub fn new(ch_id: u8, device: String, baud: u32, reg: &Registry) -> Self {
        let mut ch = McuChannel {
            ch_id,
            device,
            baud,
            state: McuState::Init,
            fd: None,
            fd_in_sel: false,
            txbuf: Vec::new(),
            last_rx: None,
            reopen_at: None,
            bp_paused: false,
            link_up: false,
        };
        ch.open_uart(reg);
        ch
    }

    fn open_uart(&mut self, reg: &Registry) {
        match open_serial_fd(&self.device, self.baud) {
            Ok(fd) => {
                self.fd = Some(fd);
                self.fd_in_sel = false;
                tracing::info!(
                    ch_id = self.ch_id, device = %self.device, baud = self.baud,
                    "MCU UART opened",
                );
                self.update_interest(reg);
            }
            Err(e) => {
                tracing::warn!(
                    ch_id = self.ch_id, device = %self.device, err = %e,
                    "MCU UART open failed, retrying in 2s",
                );
                self.reopen_at = Some(Instant::now() + REOPEN_DELAY);
            }
        }
    }

    fn close_uart(&mut self, reg: &Registry) {
        if let Some(fd) = self.fd.take() {
            if self.fd_in_sel {
                reg_del(reg, fd);
            }
            close_raw(fd);
        }
        self.fd_in_sel = false;
    }

    /// Sync mio interest flags with the current channel state.
    fn update_interest(&mut self, reg: &Registry) {
        let fd = match self.fd {
            Some(f) => f,
            None => return,
        };
        let mut interest = if self.bp_paused {
            None
        } else {
            Some(Interest::READABLE)
        };
        if !self.txbuf.is_empty() {
            interest = Some(interest.map_or(Interest::WRITABLE, |i| i | Interest::WRITABLE));
        }
        let token = primary_token(self.ch_id);
        match (interest, self.fd_in_sel) {
            (None, true) => {
                reg_del(reg, fd);
                self.fd_in_sel = false;
            }
            (None, false) => {}
            (Some(i), false) => {
                reg_add(reg, fd, token, i);
                self.fd_in_sel = true;
            }
            (Some(i), true) => {
                reg_mod(reg, fd, token, i);
            }
        }
    }

    fn uart_read(&mut self, txq: &mut TxQueue, reg: &Registry) {
        let fd = match self.fd {
            Some(f) => f,
            None => return,
        };
        let mut buf = [0u8; 4096];
        match read_nonblock(fd, &mut buf) {
            Ok(None) | Ok(Some(0)) => {}
            Ok(Some(n)) => {
                self.last_rx = Some(Instant::now());
                let data = &buf[..n];
                match self.state {
                    McuState::Init | McuState::Resetting => {
                        // Wait for Klipper sync byte 0x7E before forwarding.
                        if let Some(idx) = data.iter().position(|&b| b == KLIPPER_SYNC) {
                            tracing::info!(
                                ch_id = self.ch_id,
                                sync_offset = idx,
                                "MCU UART: 0x7E seen, transitioning to ACTIVE",
                            );
                            self.transition(McuState::Active, txq);
                            if self.link_up {
                                txq.enqueue(&build_frame(F_DATA, self.ch_id, &data[idx..]));
                            }
                        }
                        // Bootloader noise before sync — discard.
                    }
                    McuState::Active => {
                        if self.link_up {
                            txq.enqueue(&build_frame(F_DATA, self.ch_id, data));
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!(ch_id = self.ch_id, err = %e, "MCU UART read error");
                self.close_uart(reg);
                self.transition(McuState::Resetting, txq);
                self.reopen_at = Some(Instant::now() + Duration::from_secs(1));
            }
        }
    }

    fn uart_drain(&mut self, reg: &Registry) {
        let fd = match self.fd {
            Some(f) => f,
            None => return,
        };
        if self.txbuf.is_empty() {
            return;
        }
        match write_nonblock(fd, &self.txbuf) {
            Ok(0) => {}
            Ok(n) => {
                self.txbuf.drain(..n);
            }
            Err(e) => {
                tracing::error!(ch_id = self.ch_id, err = %e, "MCU UART write error");
                self.txbuf.clear();
            }
        }
        self.update_interest(reg);
    }

    fn transition(&mut self, new_state: McuState, txq: &mut TxQueue) {
        if new_state == self.state {
            return;
        }
        tracing::info!(
            ch_id = self.ch_id, from = ?self.state, to = ?new_state,
            "MCU state transition",
        );
        self.state = new_state;
        match new_state {
            McuState::Resetting => {
                self.last_rx = None;
                self.txbuf.clear();
                if self.link_up {
                    txq.enqueue(&build_frame(F_FLUSH, self.ch_id, b""));
                }
            }
            McuState::Active => {
                if self.link_up {
                    txq.enqueue(&build_frame(F_READY, self.ch_id, b""));
                }
            }
            McuState::Init => {}
        }
    }
}

impl Channel for McuChannel {
    fn channel_id(&self) -> u8 {
        self.ch_id
    }

    fn on_frame(&mut self, ftype: u8, payload: &[u8], _txq: &mut TxQueue, reg: &Registry) {
        if ftype == F_DATA {
            if self.fd.is_none() || self.state != McuState::Active {
                return;
            }
            self.txbuf.extend_from_slice(payload);
            self.uart_drain(reg);
        }
    }

    fn on_link_connect(&mut self, txq: &mut TxQueue, _reg: &Registry) {
        self.link_up = true;
        if self.state == McuState::Active {
            txq.enqueue(&build_frame(F_READY, self.ch_id, b""));
        } else {
            txq.enqueue(&build_frame(F_FLUSH, self.ch_id, b""));
        }
    }

    fn on_link_disconnect(&mut self, reg: &Registry) {
        self.link_up = false;
        self.txbuf.clear();
        self.update_interest(reg);
    }

    fn tick(&mut self, now: Instant, txq: &mut TxQueue, reg: &Registry) {
        if self.fd.is_none() {
            if self.reopen_at.map_or(false, |t| now >= t) {
                self.reopen_at = None;
                self.open_uart(reg);
            }
            return;
        }
        if self.state == McuState::Active {
            if let Some(last) = self.last_rx {
                if now.duration_since(last) > RESET_SILENCE {
                    self.transition(McuState::Resetting, txq);
                }
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        if self.fd.is_none() {
            return self.reopen_at;
        }
        if self.state == McuState::Active {
            return self.last_rx.map(|t| t + RESET_SILENCE);
        }
        None
    }

    fn handle_event(
        &mut self,
        _token: Token,
        readable: bool,
        writable: bool,
        txq: &mut TxQueue,
        reg: &Registry,
    ) {
        if readable {
            self.uart_read(txq, reg);
        }
        if writable {
            self.uart_drain(reg);
        }
    }

    fn close(&mut self, reg: &Registry) {
        self.close_uart(reg);
    }

    fn pause_source_reads(&mut self, reg: &Registry) {
        if !self.bp_paused {
            self.bp_paused = true;
            self.update_interest(reg);
        }
    }

    fn resume_source_reads(&mut self, reg: &Registry) {
        if self.bp_paused {
            self.bp_paused = false;
            self.update_interest(reg);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PtyChannel  (host side: PTY ↔ F_DATA frames)
// ─────────────────────────────────────────────────────────────────────────────

/// Host-side MCU channel.
///
/// Opens a PTY pair and exposes the slave via a well-known symlink so Klipper
/// can connect with `serial: /tmp/klipper_mcu`.  Forwards PTY master reads as
/// `F_DATA` frames and writes incoming `F_DATA` payloads to the master.
///
/// The PTY is opened on receipt of `F_READY` and closed on `F_FLUSH` or link
/// disconnect.
pub struct PtyChannel {
    ch_id: u8,
    symlink: String,
    baud: u32,
    master_fd: Option<RawFd>,
    slave_fd: Option<RawFd>,
    master_in_sel: bool,
    /// Bytes to write to the PTY master (from the link).
    txbuf: Vec<u8>,
    bp_paused: bool,
}

impl PtyChannel {
    /// Create a new PTY channel.  The symlink is removed if it already exists.
    pub fn new(ch_id: u8, symlink: String, baud: u32) -> Self {
        let ch = PtyChannel {
            ch_id,
            symlink,
            baud,
            master_fd: None,
            slave_fd: None,
            master_in_sel: false,
            txbuf: Vec::new(),
            bp_paused: false,
        };
        ch.remove_symlink();
        ch
    }

    fn open_pty(&mut self, reg: &Registry) {
        if self.master_fd.is_some() {
            return;
        }
        match open_pty_raw(self.baud) {
            Ok((master, slave)) => match pty_slave_name(slave) {
                Ok(slave_path) => {
                    self.remove_symlink();
                    if let Err(e) = std::os::unix::fs::symlink(&slave_path, &self.symlink) {
                        tracing::warn!(
                            ch_id = self.ch_id,
                            slave = %slave_path.display(),
                            symlink = %self.symlink,
                            err = %e,
                            "PTY symlink failed",
                        );
                        close_raw(master);
                        close_raw(slave);
                        return;
                    }
                    self.master_fd = Some(master);
                    self.slave_fd = Some(slave);
                    self.master_in_sel = false;
                    self.update_interest(reg);
                    tracing::info!(
                        ch_id = self.ch_id,
                        slave = %slave_path.display(),
                        symlink = %self.symlink,
                        "PTY opened",
                    );
                }
                Err(e) => {
                    tracing::warn!(ch_id = self.ch_id, err = %e, "PTY ttyname failed");
                    close_raw(master);
                    close_raw(slave);
                }
            },
            Err(e) => {
                tracing::warn!(ch_id = self.ch_id, err = %e, "PTY openpty failed");
            }
        }
    }

    fn close_pty(&mut self, reg: &Registry) {
        if let Some(fd) = self.master_fd.take() {
            if self.master_in_sel {
                reg_del(reg, fd);
            }
            close_raw(fd);
        }
        if let Some(fd) = self.slave_fd.take() {
            close_raw(fd);
        }
        self.master_in_sel = false;
        self.txbuf.clear();
        self.remove_symlink();
        tracing::info!(ch_id = self.ch_id, "PTY closed");
    }

    fn remove_symlink(&self) {
        let _ = std::fs::remove_file(&self.symlink);
    }

    fn update_interest(&mut self, reg: &Registry) {
        let fd = match self.master_fd {
            Some(f) => f,
            None => return,
        };
        let mut interest = if self.bp_paused {
            None
        } else {
            Some(Interest::READABLE)
        };
        if !self.txbuf.is_empty() {
            interest = Some(interest.map_or(Interest::WRITABLE, |i| i | Interest::WRITABLE));
        }
        let token = primary_token(self.ch_id);
        match (interest, self.master_in_sel) {
            (None, true) => {
                reg_del(reg, fd);
                self.master_in_sel = false;
            }
            (None, false) => {}
            (Some(i), false) => {
                reg_add(reg, fd, token, i);
                self.master_in_sel = true;
            }
            (Some(i), true) => {
                reg_mod(reg, fd, token, i);
            }
        }
    }

    fn pty_drain(&mut self, reg: &Registry) {
        let fd = match self.master_fd {
            Some(f) => f,
            None => return,
        };
        if self.txbuf.is_empty() {
            return;
        }
        match write_nonblock(fd, &self.txbuf) {
            Ok(0) => {}
            Ok(n) => {
                self.txbuf.drain(..n);
            }
            Err(e) => {
                tracing::error!(ch_id = self.ch_id, err = %e, "PTY write error");
                self.txbuf.clear();
            }
        }
        self.update_interest(reg);
    }
}

impl Channel for PtyChannel {
    fn channel_id(&self) -> u8 {
        self.ch_id
    }

    fn on_frame(&mut self, ftype: u8, payload: &[u8], _txq: &mut TxQueue, reg: &Registry) {
        match ftype {
            F_FLUSH => {
                tracing::info!(ch_id = self.ch_id, "PTY FLUSH -> WAITING");
                self.close_pty(reg);
            }
            F_READY => {
                tracing::info!(ch_id = self.ch_id, "PTY READY -> ACTIVE");
                self.open_pty(reg);
            }
            F_DATA => {
                if self.master_fd.is_some() {
                    self.txbuf.extend_from_slice(payload);
                    self.pty_drain(reg);
                }
            }
            _ => {}
        }
    }

    /// Stay closed until the exporter sends `F_READY`.
    fn on_link_connect(&mut self, _txq: &mut TxQueue, reg: &Registry) {
        self.close_pty(reg);
    }

    fn on_link_disconnect(&mut self, reg: &Registry) {
        tracing::info!(ch_id = self.ch_id, "PTY link down");
        self.close_pty(reg);
    }

    fn tick(&mut self, _now: Instant, _txq: &mut TxQueue, _reg: &Registry) {}
    fn next_deadline(&self) -> Option<Instant> {
        None
    }

    fn handle_event(
        &mut self,
        _token: Token,
        readable: bool,
        writable: bool,
        txq: &mut TxQueue,
        reg: &Registry,
    ) {
        if readable {
            let fd = match self.master_fd {
                Some(f) => f,
                None => return,
            };
            let mut buf = [0u8; 4096];
            match read_nonblock(fd, &mut buf) {
                Ok(Some(n)) if n > 0 => {
                    txq.enqueue(&build_frame(F_DATA, self.ch_id, &buf[..n]));
                }
                Err(e) => {
                    // EIO = all slave fds closed (Klipper disconnected).
                    if e.raw_os_error().map_or(false, |c| c == libc::EIO) {
                        self.close_pty(reg);
                    }
                }
                _ => {}
            }
        }
        if writable {
            self.pty_drain(reg);
        }
    }

    fn close(&mut self, reg: &Registry) {
        self.close_pty(reg);
    }

    fn pause_source_reads(&mut self, reg: &Registry) {
        if !self.bp_paused {
            self.bp_paused = true;
            self.update_interest(reg);
        }
    }

    fn resume_source_reads(&mut self, reg: &Registry) {
        if self.bp_paused {
            self.bp_paused = false;
            self.update_interest(reg);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TCP helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Wire connection-id helpers.
fn pack_cid(cid: u16) -> [u8; 2] {
    cid.to_le_bytes()
}

fn unpack_cid(payload: &[u8]) -> Option<(u16, &[u8])> {
    if payload.len() < 2 {
        return None;
    }
    Some((u16::from_le_bytes([payload[0], payload[1]]), &payload[2..]))
}

/// Per-connection buffer limit.  Matches the C `CONN_HIGH_WATER` constant.
const CONN_HIGH_WATER: usize = 256 * 1024;

/// State for a single TCP connection slot.
struct TcpConn {
    stream: mio::net::TcpStream,
    /// Bytes waiting to be written to the stream.
    txbuf: Vec<u8>,
    /// Async connect in progress.
    connecting: bool,
    /// Registered with mio.
    in_sel: bool,
    /// Peer sent F_TCLOSE; drain `txbuf` then close without sending F_TCLOSE back.
    close_pending: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// TcpSourceChannel  (exporter side: accept local conns → tunnel to host)
// ─────────────────────────────────────────────────────────────────────────────

/// Exporter-side TCP channel.
///
/// Listens on a local address and forwards accepted connections to the host as
/// `F_TCONN` / `F_TDATA` / `F_TCLOSE` frames.  The connection-id is a slot
/// index 0..`MAX_TCP_CONNS` — this matches the C `cid` field and is identical
/// on the wire.
pub struct TcpSourceChannel {
    ch_id: u8,
    listener: mio::net::TcpListener,
    /// Connection slots.  `None` = empty.
    conns: Vec<Option<TcpConn>>,
    next_slot: usize,
    link_up: bool,
    bp_paused: bool,
}

impl TcpSourceChannel {
    /// Create a new source channel that listens on `bind_addr:bind_port`.
    pub fn new(
        ch_id: u8,
        bind_addr: &str,
        bind_port: u16,
        reg: &Registry,
    ) -> std::io::Result<Self> {
        let addr = format!("{}:{}", bind_addr, bind_port)
            .parse()
            .map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("bad addr: {}", e))
            })?;
        let mut listener = mio::net::TcpListener::bind(addr)?;
        reg.register(&mut listener, primary_token(ch_id), Interest::READABLE)?;
        tracing::info!(ch_id, bind_addr, bind_port, "TCP source listening");
        Ok(TcpSourceChannel {
            ch_id,
            listener,
            conns: (0..MAX_TCP_CONNS).map(|_| None).collect(),
            next_slot: 0,
            link_up: false,
            bp_paused: false,
        })
    }

    /// Find the next free slot using round-robin allocation.
    fn alloc_slot(&mut self) -> Option<usize> {
        for _ in 0..MAX_TCP_CONNS {
            let s = self.next_slot % MAX_TCP_CONNS;
            self.next_slot += 1;
            if self.conns[s].is_none() {
                return Some(s);
            }
        }
        None
    }

    fn accept(&mut self, txq: &mut TxQueue, reg: &Registry) {
        loop {
            match self.listener.accept() {
                Ok((mut stream, addr)) => {
                    let _ = stream.set_nodelay(true);
                    if !self.link_up {
                        drop(stream);
                        continue;
                    }
                    let slot = match self.alloc_slot() {
                        Some(s) => s,
                        None => {
                            tracing::warn!(ch_id = self.ch_id, "TCP src: slot pool exhausted");
                            drop(stream);
                            continue;
                        }
                    };
                    let token = tcp_slot_token(self.ch_id, slot);
                    if let Err(e) =
                        reg.register(&mut stream, token, Interest::READABLE | Interest::WRITABLE)
                    {
                        tracing::warn!(ch_id = self.ch_id, err = %e, "TCP src: register error");
                        drop(stream);
                        continue;
                    }
                    tracing::info!(ch_id = self.ch_id, %addr, slot, "TCP src: accepted");
                    self.conns[slot] = Some(TcpConn {
                        stream,
                        txbuf: Vec::new(),
                        connecting: false,
                        in_sel: true,
                        close_pending: false,
                    });
                    txq.enqueue(&build_frame(F_TCONN, self.ch_id, &pack_cid(slot as u16)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!(ch_id = self.ch_id, err = %e, "TCP src: accept error");
                    break;
                }
            }
        }
    }

    fn tcp_event(
        &mut self,
        token: Token,
        readable: bool,
        writable: bool,
        txq: &mut TxQueue,
        reg: &Registry,
    ) {
        let slot = (token.0 - TCP_BASE) % MAX_TCP_CONNS;

        // ── Readable ──────────────────────────────────────────────────────
        if readable {
            let mut buf = [0u8; 65536];
            loop {
                // Skip reads when close_pending; peer said goodbye.
                let (eof, n) = {
                    let conn = match self.conns[slot].as_mut() {
                        Some(c) => c,
                        None => return,
                    };
                    if conn.close_pending {
                        break;
                    }
                    use std::io::Read;
                    match conn.stream.read(&mut buf) {
                        Ok(0) => (true, 0),
                        Ok(n) => (false, n),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => (true, 0),
                    }
                };
                if eof {
                    self.close_slot(slot, true, txq, reg);
                    return;
                }
                // Chunk into MAX_PAYLOAD-2 byte frames (2 bytes reserved for cid).
                let mut off = 0;
                while off < n {
                    let end = std::cmp::min(off + MAX_PAYLOAD - 2, n);
                    let mut payload = Vec::with_capacity(2 + end - off);
                    payload.extend_from_slice(&pack_cid(slot as u16));
                    payload.extend_from_slice(&buf[off..end]);
                    txq.enqueue(&build_frame(F_TDATA, self.ch_id, &payload));
                    off = end;
                }
            }
        }

        // ── Writable ──────────────────────────────────────────────────────
        if writable {
            let write_err = {
                let conn = match self.conns[slot].as_mut() {
                    Some(c) => c,
                    None => return,
                };
                if !conn.txbuf.is_empty() {
                    use std::io::Write;
                    match conn.stream.write(&conn.txbuf) {
                        Ok(n) => {
                            conn.txbuf.drain(..n);
                            false
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                        Err(_) => true,
                    }
                } else {
                    false
                }
            };
            if write_err {
                self.close_slot(slot, true, txq, reg);
                return;
            }
            // If close_pending and txbuf fully drained, close without notify.
            let close_now = match self.conns[slot].as_ref() {
                Some(c) => c.txbuf.is_empty() && c.close_pending,
                None => return,
            };
            if close_now {
                self.close_slot(slot, false, txq, reg);
                return;
            }
            self.update_conn_interest(slot, reg);
        }
    }

    fn update_conn_interest(&mut self, slot: usize, reg: &Registry) {
        let ch_id = self.ch_id;
        let bp_paused = self.bp_paused;
        if let Some(conn) = self.conns[slot].as_mut() {
            src_conn_update_interest(ch_id, bp_paused, slot, conn, reg);
        }
    }

    fn close_slot(&mut self, slot: usize, notify: bool, txq: &mut TxQueue, reg: &Registry) {
        if let Some(mut conn) = self.conns[slot].take() {
            let _ = reg.deregister(&mut conn.stream);
            if notify && self.link_up {
                txq.enqueue(&build_frame(F_TCLOSE, self.ch_id, &pack_cid(slot as u16)));
            }
            tracing::debug!(ch_id = self.ch_id, slot, "TCP src: slot closed");
        }
    }

    fn close_all(&mut self, txq: &mut TxQueue, reg: &Registry) {
        for slot in 0..MAX_TCP_CONNS {
            self.close_slot(slot, false, txq, reg);
        }
    }
}

impl Channel for TcpSourceChannel {
    fn channel_id(&self) -> u8 {
        self.ch_id
    }

    fn on_frame(&mut self, ftype: u8, payload: &[u8], txq: &mut TxQueue, reg: &Registry) {
        let (cid, data) = match unpack_cid(payload) {
            Some(x) => x,
            None => return,
        };
        let slot = cid as usize;
        if slot >= MAX_TCP_CONNS {
            return;
        }

        match ftype {
            F_TDATA => {
                if data.is_empty() {
                    return;
                }
                // Use a flag to avoid holding the `conn` borrow across `close_slot`
                // and `update_conn_interest`, which both need `&mut self`.
                enum TDataAction {
                    Close,
                    Update,
                    Skip,
                }
                let action = if let Some(conn) = self.conns[slot].as_mut() {
                    let avail = (CONN_HIGH_WATER + MAX_PAYLOAD).saturating_sub(conn.txbuf.len());
                    if conn.txbuf.len() > CONN_HIGH_WATER || data.len() > avail {
                        TDataAction::Close
                    } else {
                        conn.txbuf.extend_from_slice(data);
                        TDataAction::Update
                    }
                } else {
                    TDataAction::Skip
                };
                match action {
                    TDataAction::Close => {
                        tracing::warn!(
                            ch_id = self.ch_id,
                            slot,
                            "TCP src: slot exceeded high-water, closing",
                        );
                        self.close_slot(slot, true, txq, reg);
                    }
                    TDataAction::Update => self.update_conn_interest(slot, reg),
                    TDataAction::Skip => {}
                }
            }
            F_TCLOSE => {
                let should_close = if let Some(conn) = self.conns[slot].as_mut() {
                    if conn.txbuf.is_empty() {
                        true
                    } else {
                        conn.close_pending = true;
                        false
                    }
                } else {
                    return;
                };
                if should_close {
                    self.close_slot(slot, false, txq, reg);
                } else {
                    self.update_conn_interest(slot, reg);
                }
            }
            _ => {}
        }
    }

    fn on_link_connect(&mut self, _txq: &mut TxQueue, _reg: &Registry) {
        self.link_up = true;
    }

    fn on_link_disconnect(&mut self, reg: &Registry) {
        self.link_up = false;
        let mut dummy = TxQueue::new();
        self.close_all(&mut dummy, reg);
    }

    fn tick(&mut self, _now: Instant, _txq: &mut TxQueue, _reg: &Registry) {}
    fn next_deadline(&self) -> Option<Instant> {
        None
    }

    fn handle_event(
        &mut self,
        token: Token,
        readable: bool,
        writable: bool,
        txq: &mut TxQueue,
        reg: &Registry,
    ) {
        if token == primary_token(self.ch_id) {
            if readable {
                self.accept(txq, reg);
            }
        } else {
            self.tcp_event(token, readable, writable, txq, reg);
        }
    }

    fn close(&mut self, reg: &Registry) {
        let mut dummy = TxQueue::new();
        self.close_all(&mut dummy, reg);
        let _ = reg.deregister(&mut self.listener);
    }

    fn pause_source_reads(&mut self, reg: &Registry) {
        if self.bp_paused {
            return;
        }
        self.bp_paused = true;
        for slot in 0..MAX_TCP_CONNS {
            self.update_conn_interest(slot, reg);
        }
    }

    fn resume_source_reads(&mut self, reg: &Registry) {
        if !self.bp_paused {
            return;
        }
        self.bp_paused = false;
        for slot in 0..MAX_TCP_CONNS {
            self.update_conn_interest(slot, reg);
        }
    }
}

/// Update mio interest for one `TcpSourceChannel` connection slot.
///
/// Matches the C `tcp_conn_update()` logic:
/// - reads are suspended when backpressured **or** when `close_pending` is set
///   (no point reading when we will close after the final write)
/// - writes are requested whenever `txbuf` is non-empty
fn src_conn_update_interest(
    ch_id: u8,
    bp_paused: bool,
    slot: usize,
    conn: &mut TcpConn,
    reg: &Registry,
) {
    if !conn.in_sel {
        return;
    }
    // Pause reads when backpressured or when waiting to drain before close.
    let want_read = !(bp_paused || conn.close_pending);
    let want_write = !conn.txbuf.is_empty();
    let interest = match (want_read, want_write) {
        (true, true) => Interest::READABLE | Interest::WRITABLE,
        (true, false) => Interest::READABLE,
        (false, true) => Interest::WRITABLE,
        (false, false) => return, // deregister to avoid spurious wakeups
    };
    let _ = reg.reregister(&mut conn.stream, tcp_slot_token(ch_id, slot), interest);
}

// ─────────────────────────────────────────────────────────────────────────────
// TcpDestChannel  (host side: tunnel ← host → local service)
// ─────────────────────────────────────────────────────────────────────────────

/// Host-side TCP channel.
///
/// On receipt of `F_TCONN` it opens an async connection to the configured
/// destination address.  Data is forwarded bidirectionally until one side
/// sends `F_TCLOSE`.
pub struct TcpDestChannel {
    ch_id: u8,
    dest_addr: String,
    dest_port: u16,
    /// Connection slots.  `None` = empty.
    conns: Vec<Option<TcpConn>>,
    bp_paused: bool,
}

impl TcpDestChannel {
    /// Create a new destination channel.  Connections are opened lazily on
    /// `F_TCONN` frames.
    pub fn new(ch_id: u8, dest_addr: String, dest_port: u16) -> Self {
        TcpDestChannel {
            ch_id,
            dest_addr,
            dest_port,
            conns: (0..MAX_TCP_CONNS).map(|_| None).collect(),
            bp_paused: false,
        }
    }

    fn open_conn(&mut self, slot: usize, reg: &Registry, txq: &mut TxQueue) {
        let addr_str = format!("{}:{}", self.dest_addr, self.dest_port);
        let addr: std::net::SocketAddr = match addr_str.parse() {
            Ok(a) => a,
            Err(_) => {
                txq.enqueue(&build_frame(F_TCLOSE, self.ch_id, &pack_cid(slot as u16)));
                return;
            }
        };
        let mut stream = match mio::net::TcpStream::connect(addr) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(ch_id = self.ch_id, slot, err = %e, "TCP dst: connect failed");
                txq.enqueue(&build_frame(F_TCLOSE, self.ch_id, &pack_cid(slot as u16)));
                return;
            }
        };
        let _ = stream.set_nodelay(true);
        let token = tcp_slot_token(self.ch_id, slot);
        if let Err(e) = reg.register(&mut stream, token, Interest::READABLE | Interest::WRITABLE) {
            tracing::warn!(ch_id = self.ch_id, slot, err = %e, "TCP dst: register error");
            txq.enqueue(&build_frame(F_TCLOSE, self.ch_id, &pack_cid(slot as u16)));
            return;
        }
        tracing::info!(
            ch_id = self.ch_id, slot, dest_addr = %self.dest_addr, dest_port = self.dest_port,
            "TCP dst: connecting",
        );
        self.conns[slot] = Some(TcpConn {
            stream,
            txbuf: Vec::new(),
            connecting: true,
            in_sel: true,
            close_pending: false,
        });
    }

    fn tcp_event(
        &mut self,
        token: Token,
        readable: bool,
        writable: bool,
        txq: &mut TxQueue,
        reg: &Registry,
    ) {
        let slot = (token.0 - TCP_BASE) % MAX_TCP_CONNS;

        // ── Finalise async connect ────────────────────────────────────────
        let connect_failed = {
            let conn = match self.conns[slot].as_mut() {
                Some(c) => c,
                None => return,
            };
            if conn.connecting && writable {
                match conn.stream.peer_addr() {
                    Ok(_) => {
                        conn.connecting = false;
                        tracing::info!(ch_id = self.ch_id, slot, "TCP dst: connected");
                        false
                    }
                    Err(e) => {
                        tracing::warn!(ch_id = self.ch_id, slot, err = %e, "TCP dst: connect failed");
                        true
                    }
                }
            } else {
                false
            }
        };
        if connect_failed {
            self.close_slot(slot, true, txq, reg);
            return;
        }

        // ── Writable ──────────────────────────────────────────────────────
        if writable {
            let write_err = {
                let conn = match self.conns[slot].as_mut() {
                    Some(c) => c,
                    None => return,
                };
                if !conn.connecting && !conn.txbuf.is_empty() {
                    use std::io::Write;
                    match conn.stream.write(&conn.txbuf) {
                        Ok(n) => {
                            conn.txbuf.drain(..n);
                            false
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                        Err(_) => true,
                    }
                } else {
                    false
                }
            };
            if write_err {
                self.close_slot(slot, true, txq, reg);
                return;
            }
            // close_pending: drain complete → close without sending F_TCLOSE.
            let close_now = match self.conns[slot].as_ref() {
                Some(c) => c.txbuf.is_empty() && c.close_pending,
                None => return,
            };
            if close_now {
                self.close_slot(slot, false, txq, reg);
                return;
            }
        }

        // ── Readable ──────────────────────────────────────────────────────
        if readable {
            let mut buf = [0u8; 65536];
            loop {
                let (eof, n) = {
                    let conn = match self.conns[slot].as_mut() {
                        Some(c) => c,
                        None => return,
                    };
                    if conn.connecting || conn.close_pending {
                        break;
                    }
                    use std::io::Read;
                    match conn.stream.read(&mut buf) {
                        Ok(0) => (true, 0),
                        Ok(n) => (false, n),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => (true, 0),
                    }
                };
                if eof {
                    self.close_slot(slot, true, txq, reg);
                    return;
                }
                let mut off = 0;
                while off < n {
                    let end = std::cmp::min(off + MAX_PAYLOAD - 2, n);
                    let mut payload = Vec::with_capacity(2 + end - off);
                    payload.extend_from_slice(&pack_cid(slot as u16));
                    payload.extend_from_slice(&buf[off..end]);
                    txq.enqueue(&build_frame(F_TDATA, self.ch_id, &payload));
                    off = end;
                }
            }
        }

        // ── Update interest ───────────────────────────────────────────────
        let ch_id = self.ch_id;
        let bp_paused = self.bp_paused;
        if let Some(conn) = self.conns[slot].as_mut() {
            dst_conn_update_interest(ch_id, bp_paused, slot, conn, reg);
        }
    }

    fn update_conn_interest(&mut self, slot: usize, reg: &Registry) {
        let ch_id = self.ch_id;
        let bp_paused = self.bp_paused;
        if let Some(conn) = self.conns[slot].as_mut() {
            dst_conn_update_interest(ch_id, bp_paused, slot, conn, reg);
        }
    }

    fn update_conn_interest_all(&mut self, reg: &Registry) {
        let ch_id = self.ch_id;
        let bp_paused = self.bp_paused;
        for slot in 0..MAX_TCP_CONNS {
            if let Some(conn) = self.conns[slot].as_mut() {
                dst_conn_update_interest(ch_id, bp_paused, slot, conn, reg);
            }
        }
    }

    fn close_slot(&mut self, slot: usize, notify: bool, txq: &mut TxQueue, reg: &Registry) {
        if let Some(mut conn) = self.conns[slot].take() {
            let _ = reg.deregister(&mut conn.stream);
            if notify {
                txq.enqueue(&build_frame(F_TCLOSE, self.ch_id, &pack_cid(slot as u16)));
            }
            tracing::debug!(ch_id = self.ch_id, slot, "TCP dst: slot closed");
        }
    }
}

impl Channel for TcpDestChannel {
    fn channel_id(&self) -> u8 {
        self.ch_id
    }

    fn on_frame(&mut self, ftype: u8, payload: &[u8], txq: &mut TxQueue, reg: &Registry) {
        let (cid, data) = match unpack_cid(payload) {
            Some(x) => x,
            None => return,
        };
        let slot = cid as usize;
        if slot >= MAX_TCP_CONNS {
            return;
        }

        match ftype {
            F_TCONN => {
                if self.conns[slot].is_none() {
                    self.open_conn(slot, reg, txq);
                }
            }
            F_TDATA => {
                if data.is_empty() {
                    return;
                }
                enum TDataAction {
                    Close,
                    Update,
                    Skip,
                }
                let action = if let Some(conn) = self.conns[slot].as_mut() {
                    let avail = (CONN_HIGH_WATER + MAX_PAYLOAD).saturating_sub(conn.txbuf.len());
                    if conn.txbuf.len() > CONN_HIGH_WATER || data.len() > avail {
                        TDataAction::Close
                    } else {
                        conn.txbuf.extend_from_slice(data);
                        TDataAction::Update
                    }
                } else {
                    TDataAction::Skip
                };
                match action {
                    TDataAction::Close => {
                        tracing::warn!(
                            ch_id = self.ch_id,
                            slot,
                            "TCP dst: slot exceeded high-water, closing",
                        );
                        self.close_slot(slot, true, txq, reg);
                    }
                    TDataAction::Update => self.update_conn_interest(slot, reg),
                    TDataAction::Skip => {}
                }
            }
            F_TCLOSE => {
                let should_close = if let Some(conn) = self.conns[slot].as_mut() {
                    if conn.txbuf.is_empty() {
                        true
                    } else {
                        conn.close_pending = true;
                        false
                    }
                } else {
                    return;
                };
                if should_close {
                    self.close_slot(slot, false, txq, reg);
                } else {
                    self.update_conn_interest(slot, reg);
                }
            }
            _ => {}
        }
    }

    /// Close any connections left open from a previous session, matching the
    /// C `ch_on_link_up` → `ch_on_link_down` call for `CH_TCP_DST`.
    fn on_link_connect(&mut self, _txq: &mut TxQueue, reg: &Registry) {
        let mut dummy = TxQueue::new();
        for slot in 0..MAX_TCP_CONNS {
            self.close_slot(slot, false, &mut dummy, reg);
        }
    }

    fn on_link_disconnect(&mut self, reg: &Registry) {
        let mut dummy = TxQueue::new();
        for slot in 0..MAX_TCP_CONNS {
            self.close_slot(slot, false, &mut dummy, reg);
        }
    }

    fn tick(&mut self, _now: Instant, _txq: &mut TxQueue, _reg: &Registry) {}
    fn next_deadline(&self) -> Option<Instant> {
        None
    }

    fn handle_event(
        &mut self,
        token: Token,
        readable: bool,
        writable: bool,
        txq: &mut TxQueue,
        reg: &Registry,
    ) {
        self.tcp_event(token, readable, writable, txq, reg);
    }

    fn close(&mut self, reg: &Registry) {
        let mut dummy = TxQueue::new();
        for slot in 0..MAX_TCP_CONNS {
            self.close_slot(slot, false, &mut dummy, reg);
        }
    }

    fn pause_source_reads(&mut self, reg: &Registry) {
        if self.bp_paused {
            return;
        }
        self.bp_paused = true;
        self.update_conn_interest_all(reg);
    }

    fn resume_source_reads(&mut self, reg: &Registry) {
        if !self.bp_paused {
            return;
        }
        self.bp_paused = false;
        self.update_conn_interest_all(reg);
    }
}

/// Update mio interest for one `TcpDestChannel` connection slot.
///
/// Matches the C `tcp_conn_update()` logic:
/// reads are paused when backpressured or `close_pending`; writes are
/// requested when `txbuf` is non-empty or an async connect is in progress.
fn dst_conn_update_interest(
    ch_id: u8,
    bp_paused: bool,
    slot: usize,
    conn: &mut TcpConn,
    reg: &Registry,
) {
    if !conn.in_sel {
        return;
    }
    let want_write = !conn.txbuf.is_empty() || conn.connecting;
    let want_read = !(bp_paused || conn.close_pending);
    let interest = match (want_read, want_write) {
        (true, true) => Interest::READABLE | Interest::WRITABLE,
        (true, false) => Interest::READABLE,
        (false, true) => Interest::WRITABLE,
        (false, false) => return,
    };
    let _ = reg.reregister(&mut conn.stream, tcp_slot_token(ch_id, slot), interest);
}

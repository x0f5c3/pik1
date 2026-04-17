use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};

use crate::protocol::{
    build_frame, TxQueue, F_DATA, F_FLUSH, F_READY, F_TCLOSE, F_TCONN, F_TDATA,
    KLIPPER_SYNC, MAX_PAYLOAD,
};
use crate::serial::{log, open_pty_raw, open_serial_fd, pty_slave_name,
                    read_nonblock, write_nonblock};

// ---------------------------------------------------------------------------
// Token layout
// ---------------------------------------------------------------------------
// TOKEN_LINK (0)         – link fd (managed by Daemon)
// 1..=63                 – channel primary fd  (channel N = token N+1)
// 64 + N*1024 + slot     – TCP connection slot for channel N  (slot 0..1023)
// ---------------------------------------------------------------------------

pub const TOKEN_LINK: Token = Token(0);
pub const TCP_TOKEN_BASE: usize = 64;
pub const TCP_SLOTS: usize = 1024;

pub fn primary_token(ch_id: u8) -> Token {
    Token(ch_id as usize + 1)
}

pub fn tcp_slot_token(ch_id: u8, slot: usize) -> Token {
    Token(TCP_TOKEN_BASE + ch_id as usize * TCP_SLOTS + slot)
}

/// Map any token back to the owning channel id.
pub fn channel_id_for_token(token: Token) -> Option<u8> {
    match token.0 {
        0 => None,
        1..=63 => Some((token.0 - 1) as u8),
        t => {
            let id = (t - TCP_TOKEN_BASE) / TCP_SLOTS;
            if id <= 62 { Some(id as u8) } else { None }
        }
    }
}

// ---------------------------------------------------------------------------
// Channel trait
// ---------------------------------------------------------------------------

pub trait Channel {
    fn channel_id(&self) -> u8;
    fn on_frame(&mut self, ftype: u8, payload: &[u8], txq: &mut TxQueue, reg: &Registry);
    fn on_link_connect(&mut self, txq: &mut TxQueue, reg: &Registry);
    fn on_link_disconnect(&mut self, reg: &Registry);
    fn tick(&mut self, now: Instant, txq: &mut TxQueue, reg: &Registry);
    fn next_deadline(&self) -> Option<Instant>;
    fn handle_event(&mut self, token: Token, readable: bool, writable: bool,
                    txq: &mut TxQueue, reg: &Registry);
    fn close(&mut self, reg: &Registry);
    fn pause_source_reads(&mut self, reg: &Registry);
    fn resume_source_reads(&mut self, reg: &Registry);
}

// ---------------------------------------------------------------------------
// Helper: register / modify / deregister a raw fd with mio
// ---------------------------------------------------------------------------

fn reg_add(reg: &Registry, fd: RawFd, token: Token, interest: Interest) {
    if let Err(e) = reg.register(&mut SourceFd(&fd), token, interest) {
        log(&format!("mio register fd={} token={:?} error: {}", fd, token, e));
    }
}

fn reg_mod(reg: &Registry, fd: RawFd, token: Token, interest: Interest) {
    if let Err(e) = reg.reregister(&mut SourceFd(&fd), token, interest) {
        log(&format!("mio reregister fd={} token={:?} error: {}", fd, token, e));
    }
}

fn reg_del(reg: &Registry, fd: RawFd) {
    let _ = reg.deregister(&mut SourceFd(&fd));
}

fn close_fd(fd: RawFd) {
    unsafe { libc::close(fd); }
}

// ---------------------------------------------------------------------------
// McuChannel  (exporter side: UART → link)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Clone, Copy)]
enum McuState { Init, Active, Resetting }

pub struct McuChannel {
    ch_id: u8,
    device: String,
    baud: u32,
    state: McuState,
    fd: Option<RawFd>,
    fd_in_sel: bool,
    txbuf: Vec<u8>,
    last_rx: Option<Instant>,
    reopen_at: Option<Instant>,
    bp_paused: bool,
    link_up: bool,
}

const RESET_SILENCE: Duration = Duration::from_secs(5);
const REOPEN_DELAY:  Duration = Duration::from_secs(2);

impl McuChannel {
    pub fn new(ch_id: u8, device: String, baud: u32, reg: &Registry) -> Self {
        let mut ch = McuChannel {
            ch_id, device, baud,
            state: McuState::Init,
            fd: None, fd_in_sel: false,
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
                log(&format!("MCU ch{}: opened {} @ {}", self.ch_id, self.device, self.baud));
                self.update_interest(reg);
            }
            Err(e) => {
                log(&format!("MCU ch{}: cannot open {}: {} -- retry in 2s",
                             self.ch_id, self.device, e));
                self.reopen_at = Some(Instant::now() + REOPEN_DELAY);
            }
        }
    }

    fn close_uart(&mut self, reg: &Registry) {
        if let Some(fd) = self.fd.take() {
            if self.fd_in_sel { reg_del(reg, fd); }
            close_fd(fd);
        }
        self.fd_in_sel = false;
    }

    fn update_interest(&mut self, reg: &Registry) {
        let fd = match self.fd { Some(f) => f, None => return };
        let mut interest = if self.bp_paused { None } else { Some(Interest::READABLE) };
        if !self.txbuf.is_empty() {
            interest = Some(interest.map_or(Interest::WRITABLE, |i| i | Interest::WRITABLE));
        }
        let token = primary_token(self.ch_id);
        match (interest, self.fd_in_sel) {
            (None, true)  => { reg_del(reg, fd); self.fd_in_sel = false; }
            (None, false) => {}
            (Some(i), false) => { reg_add(reg, fd, token, i); self.fd_in_sel = true; }
            (Some(i), true)  => { reg_mod(reg, fd, token, i); }
        }
    }

    fn uart_read(&mut self, txq: &mut TxQueue, reg: &Registry) {
        let fd = match self.fd { Some(f) => f, None => return };
        let mut buf = [0u8; 4096];
        match read_nonblock(fd, &mut buf) {
            Ok(None) => {}
            Ok(Some(0)) => {}
            Ok(Some(n)) => {
                self.last_rx = Some(Instant::now());
                let data = &buf[..n];
                match self.state {
                    McuState::Init | McuState::Resetting => {
                        if let Some(idx) = data.iter().position(|&b| b == KLIPPER_SYNC) {
                            log(&format!("MCU ch{}: 0x7E seen at offset {} -> ACTIVE",
                                        self.ch_id, idx));
                            self.transition(McuState::Active, txq);
                            if self.link_up {
                                txq.enqueue(&build_frame(F_DATA, self.ch_id, &data[idx..]));
                            }
                        }
                    }
                    McuState::Active => {
                        if self.link_up {
                            txq.enqueue(&build_frame(F_DATA, self.ch_id, data));
                        }
                    }
                }
            }
            Err(e) => {
                log(&format!("MCU ch{}: UART read error: {}", self.ch_id, e));
                self.close_uart(reg);
                self.transition(McuState::Resetting, txq);
                self.reopen_at = Some(Instant::now() + Duration::from_secs(1));
            }
        }
    }

    fn uart_drain(&mut self, reg: &Registry) {
        let fd = match self.fd { Some(f) => f, None => return };
        if self.txbuf.is_empty() { return; }
        match write_nonblock(fd, &self.txbuf) {
            Ok(0) => {}
            Ok(n) => { self.txbuf.drain(..n); }
            Err(e) => {
                log(&format!("MCU ch{}: UART write error: {}", self.ch_id, e));
                self.txbuf.clear();
            }
        }
        self.update_interest(reg);
    }

    fn transition(&mut self, new_state: McuState, txq: &mut TxQueue) {
        if new_state == self.state { return; }
        log(&format!("MCU ch{}: {:?} -> {:?}", self.ch_id, self.state, new_state));
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
    fn channel_id(&self) -> u8 { self.ch_id }

    fn on_frame(&mut self, ftype: u8, payload: &[u8], _txq: &mut TxQueue, reg: &Registry) {
        if ftype == F_DATA {
            if self.fd.is_none() || self.state != McuState::Active { return; }
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
        if self.fd.is_none() { return self.reopen_at; }
        if self.state == McuState::Active {
            return self.last_rx.map(|t| t + RESET_SILENCE);
        }
        None
    }

    fn handle_event(&mut self, _token: Token, readable: bool, writable: bool,
                    txq: &mut TxQueue, reg: &Registry) {
        if readable { self.uart_read(txq, reg); }
        if writable { self.uart_drain(reg); }
    }

    fn close(&mut self, reg: &Registry) { self.close_uart(reg); }

    fn pause_source_reads(&mut self, reg: &Registry) {
        if !self.bp_paused { self.bp_paused = true; self.update_interest(reg); }
    }
    fn resume_source_reads(&mut self, reg: &Registry) {
        if self.bp_paused { self.bp_paused = false; self.update_interest(reg); }
    }
}

// ---------------------------------------------------------------------------
// PtyChannel  (host side: link → PTY ← Klipper)
// ---------------------------------------------------------------------------

pub struct PtyChannel {
    ch_id: u8,
    symlink: String,
    baud: u32,
    master_fd: Option<RawFd>,
    slave_fd:  Option<RawFd>,
    master_in_sel: bool,
    txbuf: Vec<u8>,
    bp_paused: bool,
}

impl PtyChannel {
    pub fn new(ch_id: u8, symlink: String, baud: u32) -> Self {
        let ch = PtyChannel {
            ch_id, symlink, baud,
            master_fd: None, slave_fd: None,
            master_in_sel: false,
            txbuf: Vec::new(),
            bp_paused: false,
        };
        ch.remove_symlink();
        ch
    }

    fn open_pty(&mut self, reg: &Registry) {
        if self.master_fd.is_some() { return; }
        match open_pty_raw(self.baud) {
            Ok((master, slave)) => {
                match pty_slave_name(slave) {
                    Ok(slave_path) => {
                        self.remove_symlink();
                        if let Err(e) = std::os::unix::fs::symlink(&slave_path, &self.symlink) {
                            log(&format!("PTY ch{}: symlink failed: {}", self.ch_id, e));
                            close_fd(master); close_fd(slave);
                            return;
                        }
                        self.master_fd = Some(master);
                        self.slave_fd  = Some(slave);
                        self.master_in_sel = false;
                        self.update_interest(reg);
                        log(&format!("PTY ch{}: opened {} -> {}",
                                     self.ch_id,
                                     slave_path.display(),
                                     self.symlink));
                    }
                    Err(e) => {
                        log(&format!("PTY ch{}: ttyname failed: {}", self.ch_id, e));
                        close_fd(master); close_fd(slave);
                    }
                }
            }
            Err(e) => {
                log(&format!("PTY ch{}: openpty failed: {}", self.ch_id, e));
            }
        }
    }

    fn close_pty(&mut self, reg: &Registry) {
        if let Some(fd) = self.master_fd.take() {
            if self.master_in_sel { reg_del(reg, fd); }
            close_fd(fd);
        }
        if let Some(fd) = self.slave_fd.take() { close_fd(fd); }
        self.master_in_sel = false;
        self.txbuf.clear();
        self.remove_symlink();
        log(&format!("PTY ch{}: closed", self.ch_id));
    }

    fn remove_symlink(&self) {
        let _ = std::fs::remove_file(&self.symlink);
    }

    fn update_interest(&mut self, reg: &Registry) {
        let fd = match self.master_fd { Some(f) => f, None => return };
        let mut interest = if self.bp_paused { None } else { Some(Interest::READABLE) };
        if !self.txbuf.is_empty() {
            interest = Some(interest.map_or(Interest::WRITABLE, |i| i | Interest::WRITABLE));
        }
        let token = primary_token(self.ch_id);
        match (interest, self.master_in_sel) {
            (None, true)  => { reg_del(reg, fd); self.master_in_sel = false; }
            (None, false) => {}
            (Some(i), false) => { reg_add(reg, fd, token, i); self.master_in_sel = true; }
            (Some(i), true)  => { reg_mod(reg, fd, token, i); }
        }
    }

    fn pty_drain(&mut self, reg: &Registry) {
        let fd = match self.master_fd { Some(f) => f, None => return };
        if self.txbuf.is_empty() { return; }
        match write_nonblock(fd, &self.txbuf) {
            Ok(0) => {}
            Ok(n) => { self.txbuf.drain(..n); }
            Err(e) => {
                log(&format!("PTY ch{}: write error: {}", self.ch_id, e));
                self.txbuf.clear();
            }
        }
        self.update_interest(reg);
    }
}

impl Channel for PtyChannel {
    fn channel_id(&self) -> u8 { self.ch_id }

    fn on_frame(&mut self, ftype: u8, payload: &[u8], _txq: &mut TxQueue, reg: &Registry) {
        match ftype {
            F_FLUSH => {
                log(&format!("PTY ch{}: FLUSH -> WAITING", self.ch_id));
                self.close_pty(reg);
            }
            F_READY => {
                log(&format!("PTY ch{}: READY -> ACTIVE", self.ch_id));
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

    fn on_link_connect(&mut self, _txq: &mut TxQueue, reg: &Registry) {
        // Stay closed; wait for exporter to send READY
        self.close_pty(reg);
    }

    fn on_link_disconnect(&mut self, reg: &Registry) {
        log(&format!("PTY ch{}: link down", self.ch_id));
        self.close_pty(reg);
    }

    fn tick(&mut self, _now: Instant, _txq: &mut TxQueue, _reg: &Registry) {}
    fn next_deadline(&self) -> Option<Instant> { None }

    fn handle_event(&mut self, _token: Token, readable: bool, writable: bool,
                    txq: &mut TxQueue, reg: &Registry) {
        if readable {
            let fd = match self.master_fd { Some(f) => f, None => return };
            let mut buf = [0u8; 4096];
            match read_nonblock(fd, &mut buf) {
                Ok(Some(n)) if n > 0 => {
                    txq.enqueue(&build_frame(F_DATA, self.ch_id, &buf[..n]));
                }
                Err(_) => {}
                _ => {}
            }
        }
        if writable { self.pty_drain(reg); }
    }

    fn close(&mut self, reg: &Registry) { self.close_pty(reg); }

    fn pause_source_reads(&mut self, reg: &Registry) {
        if !self.bp_paused { self.bp_paused = true; self.update_interest(reg); }
    }
    fn resume_source_reads(&mut self, reg: &Registry) {
        if self.bp_paused { self.bp_paused = false; self.update_interest(reg); }
    }
}

// ---------------------------------------------------------------------------
// TCP helpers
// ---------------------------------------------------------------------------

fn pack_cid(cid: u16) -> [u8; 2] { cid.to_le_bytes() }

fn unpack_cid(payload: &[u8]) -> Option<(u16, &[u8])> {
    if payload.len() < 2 { return None; }
    let cid = u16::from_le_bytes([payload[0], payload[1]]);
    Some((cid, &payload[2..]))
}

struct TcpConn {
    stream: mio::net::TcpStream,
    txbuf: Vec<u8>,
    connecting: bool,
    in_sel: bool,
}

const CONN_HIGH_WATER: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// TcpSourceChannel  (exporter side: accept local connections → tunnel to host)
// ---------------------------------------------------------------------------

pub struct TcpSourceChannel {
    ch_id: u8,
    listener: mio::net::TcpListener,
    conns: HashMap<usize, TcpConn>,  // slot → conn
    by_ptr: HashMap<usize, usize>,   // stream ptr → slot (for reverse lookup)
    next_slot: usize,
    link_up: bool,
    bp_paused: bool,
}

impl TcpSourceChannel {
    pub fn new(ch_id: u8, bind_addr: &str, bind_port: u16, reg: &Registry)
        -> std::io::Result<Self>
    {
        let addr = format!("{}:{}", bind_addr, bind_port).parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput,
                                              format!("bad addr: {}", e)))?;
        let mut listener = mio::net::TcpListener::bind(addr)?;
        reg.register(&mut listener, primary_token(ch_id), Interest::READABLE)?;
        log(&format!("TCP src ch{}: listening {}:{}", ch_id, bind_addr, bind_port));
        Ok(TcpSourceChannel {
            ch_id, listener,
            conns: HashMap::new(), by_ptr: HashMap::new(),
            next_slot: 0, link_up: false, bp_paused: false,
        })
    }

    fn alloc_slot(&mut self) -> Option<usize> {
        for _ in 0..TCP_SLOTS {
            let s = self.next_slot % TCP_SLOTS;
            self.next_slot += 1;
            if !self.conns.contains_key(&s) { return Some(s); }
        }
        None
    }

    fn stream_ptr(stream: &mio::net::TcpStream) -> usize {
        stream as *const _ as usize
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
                            log(&format!("TCP src ch{}: slot pool exhausted", self.ch_id));
                            drop(stream);
                            continue;
                        }
                    };
                    let token = tcp_slot_token(self.ch_id, slot);
                    if let Err(e) = reg.register(&mut stream, token,
                                                  Interest::READABLE | Interest::WRITABLE) {
                        log(&format!("TCP src ch{}: register error: {}", self.ch_id, e));
                        drop(stream);
                        continue;
                    }
                    let ptr = Self::stream_ptr(&stream);
                    log(&format!("TCP src ch{}: accepted {} slot={}", self.ch_id, addr, slot));
                    self.by_ptr.insert(ptr, slot);
                    self.conns.insert(slot, TcpConn {
                        stream, txbuf: Vec::new(), connecting: false, in_sel: true,
                    });
                    txq.enqueue(&build_frame(F_TCONN, self.ch_id, &pack_cid(slot as u16)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    log(&format!("TCP src ch{}: accept error: {}", self.ch_id, e));
                    break;
                }
            }
        }
    }

    fn tcp_event(&mut self, token: Token, readable: bool, writable: bool,
                 txq: &mut TxQueue, reg: &Registry)
    {
        let slot = (token.0 - TCP_TOKEN_BASE) % TCP_SLOTS;
        if readable {
            let mut buf = [0u8; 65536];
            loop {
                let (read_result, n) = {
                    let conn = match self.conns.get_mut(&slot) { Some(c) => c, None => return };
                    use std::io::Read;
                    match conn.stream.read(&mut buf) {
                        Ok(0) => (true, 0),
                        Ok(n) => (false, n),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => (true, 0),
                    }
                }; // conn borrow dropped
                if read_result { self.close_slot(slot, true, txq, reg); return; }
                let mut off = 0;
                while off < n {
                    let end = std::cmp::min(off + MAX_PAYLOAD - 2, n);
                    let chunk = &buf[off..end];
                    let mut payload = Vec::with_capacity(2 + chunk.len());
                    payload.extend_from_slice(&pack_cid(slot as u16));
                    payload.extend_from_slice(chunk);
                    txq.enqueue(&build_frame(F_TDATA, self.ch_id, &payload));
                    off = end;
                }
            }
        }
        if writable {
            let write_err = {
                let conn = match self.conns.get_mut(&slot) { Some(c) => c, None => return };
                if !conn.txbuf.is_empty() {
                    use std::io::Write;
                    match conn.stream.write(&conn.txbuf) {
                        Ok(n) => { conn.txbuf.drain(..n); false }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                        Err(_) => true,
                    }
                } else { false }
            }; // conn borrow dropped
            if write_err { self.close_slot(slot, true, txq, reg); return; }
            // Copy scalar fields before taking the conn borrow so the borrow
            // checker sees distinct borrows (ch_id, bp_paused are Copy).
            let ch_id = self.ch_id;
            let bp_paused = self.bp_paused;
            if let Some(conn) = self.conns.get_mut(&slot) {
                update_src_conn_interest(ch_id, bp_paused, slot, conn, reg);
            }
        }
    }

    fn update_conn_interest(&mut self, slot: usize, reg: &Registry) {
        let ch_id = self.ch_id;
        let bp_paused = self.bp_paused;
        if let Some(conn) = self.conns.get_mut(&slot) {
            update_src_conn_interest(ch_id, bp_paused, slot, conn, reg);
        }
    }

    fn close_slot(&mut self, slot: usize, notify: bool, txq: &mut TxQueue, reg: &Registry) {
        if let Some(mut conn) = self.conns.remove(&slot) {
            let ptr = Self::stream_ptr(&conn.stream);
            self.by_ptr.remove(&ptr);
            let _ = reg.deregister(&mut conn.stream);
            if notify && self.link_up {
                txq.enqueue(&build_frame(F_TCLOSE, self.ch_id, &pack_cid(slot as u16)));
            }
            log(&format!("TCP src ch{}: closed slot={}", self.ch_id, slot));
        }
    }

    fn close_all(&mut self, txq: &mut TxQueue, reg: &Registry) {
        let slots: Vec<usize> = self.conns.keys().copied().collect();
        for slot in slots { self.close_slot(slot, false, txq, reg); }
    }
}

impl Channel for TcpSourceChannel {
    fn channel_id(&self) -> u8 { self.ch_id }

    fn on_frame(&mut self, ftype: u8, payload: &[u8], txq: &mut TxQueue, reg: &Registry) {
        let (cid, data) = match unpack_cid(payload) { Some(x) => x, None => return };
        let slot = cid as usize;
        match ftype {
            F_TDATA => {
                if let Some(conn) = self.conns.get_mut(&slot) {
                    conn.txbuf.extend_from_slice(data);
                    if conn.txbuf.len() > CONN_HIGH_WATER {
                        log(&format!("TCP src ch{}: slot={} high-water", self.ch_id, slot));
                        self.close_slot(slot, true, txq, reg);
                    } else {
                        self.update_conn_interest(slot, reg);
                    }
                }
            }
            F_TCLOSE => { self.close_slot(slot, false, txq, reg); }
            _ => {}
        }
    }

    fn on_link_connect(&mut self, _txq: &mut TxQueue, _reg: &Registry) { self.link_up = true; }

    fn on_link_disconnect(&mut self, reg: &Registry) {
        self.link_up = false;
        let slots: Vec<usize> = self.conns.keys().copied().collect();
        // Use a local dummy TxQueue – no point sending TCLOSE frames when link is down
        let mut dummy = TxQueue::new();
        for slot in slots { self.close_slot(slot, false, &mut dummy, reg); }
    }

    fn tick(&mut self, _now: Instant, _txq: &mut TxQueue, _reg: &Registry) {}
    fn next_deadline(&self) -> Option<Instant> { None }

    fn handle_event(&mut self, token: Token, readable: bool, writable: bool,
                    txq: &mut TxQueue, reg: &Registry) {
        if token == primary_token(self.ch_id) {
            if readable { self.accept(txq, reg); }
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
        if self.bp_paused { return; }
        self.bp_paused = true;
        let slots: Vec<usize> = self.conns.keys().copied().collect();
        for slot in slots { self.update_conn_interest(slot, reg); }
    }

    fn resume_source_reads(&mut self, reg: &Registry) {
        if !self.bp_paused { return; }
        self.bp_paused = false;
        let slots: Vec<usize> = self.conns.keys().copied().collect();
        for slot in slots { self.update_conn_interest(slot, reg); }
    }
}

// ---------------------------------------------------------------------------
// Free helper: update mio interest for a single TcpSourceChannel connection
// ---------------------------------------------------------------------------

fn update_src_conn_interest(ch_id: u8, bp_paused: bool, slot: usize,
                             conn: &mut TcpConn, reg: &Registry) {
    let interest = if conn.txbuf.is_empty() {
        if !bp_paused { Interest::READABLE } else { return; }
    } else if bp_paused {
        Interest::WRITABLE
    } else {
        Interest::READABLE | Interest::WRITABLE
    };
    if conn.in_sel {
        let _ = reg.reregister(&mut conn.stream, tcp_slot_token(ch_id, slot), interest);
    }
}

// ---------------------------------------------------------------------------
// TcpDestChannel  (host side: tunnel ← host → local service)
// ---------------------------------------------------------------------------

pub struct TcpDestChannel {
    ch_id: u8,
    dest_addr: String,
    dest_port: u16,
    conns: HashMap<usize, TcpConn>,  // slot → conn
    bp_paused: bool,
}

impl TcpDestChannel {
    pub fn new(ch_id: u8, dest_addr: String, dest_port: u16) -> Self {
        TcpDestChannel {
            ch_id, dest_addr, dest_port,
            conns: HashMap::new(),
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
                log(&format!("TCP dst ch{}: connect slot={} failed: {}", self.ch_id, slot, e));
                txq.enqueue(&build_frame(F_TCLOSE, self.ch_id, &pack_cid(slot as u16)));
                return;
            }
        };
        let _ = stream.set_nodelay(true);
        let token = tcp_slot_token(self.ch_id, slot);
        if let Err(e) = reg.register(&mut stream, token,
                                      Interest::READABLE | Interest::WRITABLE) {
            log(&format!("TCP dst ch{}: register error slot={}: {}", self.ch_id, slot, e));
            txq.enqueue(&build_frame(F_TCLOSE, self.ch_id, &pack_cid(slot as u16)));
            return;
        }
        log(&format!("TCP dst ch{}: connecting slot={} -> {}:{}",
                     self.ch_id, slot, self.dest_addr, self.dest_port));
        self.conns.insert(slot, TcpConn { stream, txbuf: Vec::new(), connecting: true, in_sel: true });
    }

    fn tcp_event(&mut self, token: Token, readable: bool, writable: bool,
                 txq: &mut TxQueue, reg: &Registry)
    {
        let slot = (token.0 - TCP_TOKEN_BASE) % TCP_SLOTS;

        // Finalise async connect — use a scoped block to drop the conn borrow
        // before potentially calling close_slot (which also needs &mut self).
        let connect_failed = {
            let conn = match self.conns.get_mut(&slot) { Some(c) => c, None => return };
            if conn.connecting && writable {
                match conn.stream.peer_addr() {
                    Ok(_) => {
                        conn.connecting = false;
                        log(&format!("TCP dst ch{}: connected slot={}", self.ch_id, slot));
                        false
                    }
                    Err(e) => {
                        log(&format!("TCP dst ch{}: connect failed slot={}: {}",
                                     self.ch_id, slot, e));
                        true
                    }
                }
            } else { false }
        }; // conn borrow dropped here
        if connect_failed { self.close_slot(slot, true, txq, reg); return; }

        // Write: scoped block to drop borrow before close_slot
        let write_err = if writable {
            let conn = match self.conns.get_mut(&slot) { Some(c) => c, None => return };
            if !conn.connecting && !conn.txbuf.is_empty() {
                use std::io::Write;
                match conn.stream.write(&conn.txbuf) {
                    Ok(n) => { conn.txbuf.drain(..n); false }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(_) => true,
                }
            } else { false }
        } else { false };
        if write_err { self.close_slot(slot, true, txq, reg); return; }

        // Read: scoped block per iteration to allow close_slot calls
        if readable {
            let mut buf = [0u8; 65536];
            loop {
                let (read_result, n) = {
                    let conn = match self.conns.get_mut(&slot) { Some(c) => c, None => return };
                    if conn.connecting { break; }
                    use std::io::Read;
                    match conn.stream.read(&mut buf) {
                        Ok(0) => (true, 0),
                        Ok(n) => (false, n),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => (true, 0),
                    }
                }; // conn borrow dropped
                if read_result { self.close_slot(slot, true, txq, reg); return; }
                let mut off = 0;
                while off < n {
                    let end = std::cmp::min(off + MAX_PAYLOAD - 2, n);
                    let chunk = &buf[off..end];
                    let mut payload = Vec::with_capacity(2 + chunk.len());
                    payload.extend_from_slice(&pack_cid(slot as u16));
                    payload.extend_from_slice(chunk);
                    txq.enqueue(&build_frame(F_TDATA, self.ch_id, &payload));
                    off = end;
                }
            }
        }

        // Update interest
        let ch_id = self.ch_id;
        let bp_paused = self.bp_paused;
        let conn = match self.conns.get_mut(&slot) { Some(c) => c, None => return };
        update_dst_conn_interest(ch_id, bp_paused, slot, conn, reg);
    }

    fn update_conn_interest(&mut self, slot: usize, reg: &Registry) {
        let ch_id = self.ch_id;
        let bp_paused = self.bp_paused;
        if let Some(conn) = self.conns.get_mut(&slot) {
            update_dst_conn_interest(ch_id, bp_paused, slot, conn, reg);
        }
    }

    fn update_conn_interest_all(&mut self, reg: &Registry) {
        let ch_id = self.ch_id;
        let bp_paused = self.bp_paused;
        let slots: Vec<usize> = self.conns.keys().copied().collect();
        for slot in slots {
            if let Some(conn) = self.conns.get_mut(&slot) {
                update_dst_conn_interest(ch_id, bp_paused, slot, conn, reg);
            }
        }
    }

    fn close_slot(&mut self, slot: usize, notify: bool, txq: &mut TxQueue, reg: &Registry) {
        if let Some(mut conn) = self.conns.remove(&slot) {
            let _ = reg.deregister(&mut conn.stream);
            if notify {
                txq.enqueue(&build_frame(F_TCLOSE, self.ch_id, &pack_cid(slot as u16)));
            }
            log(&format!("TCP dst ch{}: closed slot={}", self.ch_id, slot));
        }
    }
}

impl Channel for TcpDestChannel {
    fn channel_id(&self) -> u8 { self.ch_id }

    fn on_frame(&mut self, ftype: u8, payload: &[u8], txq: &mut TxQueue, reg: &Registry) {
        let (cid, data) = match unpack_cid(payload) { Some(x) => x, None => return };
        let slot = cid as usize;
        match ftype {
            F_TCONN => { if !self.conns.contains_key(&slot) { self.open_conn(slot, reg, txq); } }
            F_TDATA => {
                if let Some(conn) = self.conns.get_mut(&slot) {
                    conn.txbuf.extend_from_slice(data);
                    if conn.txbuf.len() > CONN_HIGH_WATER {
                        log(&format!("TCP dst ch{}: slot={} high-water", self.ch_id, slot));
                        self.close_slot(slot, true, txq, reg);
                    } else {
                        self.update_conn_interest(slot, reg);
                    }
                }
            }
            F_TCLOSE => { self.close_slot(slot, false, txq, reg); }
            _ => {}
        }
    }

    fn on_link_connect(&mut self, _txq: &mut TxQueue, _reg: &Registry) {}

    fn on_link_disconnect(&mut self, reg: &Registry) {
        let slots: Vec<usize> = self.conns.keys().copied().collect();
        let mut dummy = TxQueue::new();
        for slot in slots { self.close_slot(slot, false, &mut dummy, reg); }
    }

    fn tick(&mut self, _now: Instant, _txq: &mut TxQueue, _reg: &Registry) {}
    fn next_deadline(&self) -> Option<Instant> { None }

    fn handle_event(&mut self, token: Token, readable: bool, writable: bool,
                    txq: &mut TxQueue, reg: &Registry) {
        self.tcp_event(token, readable, writable, txq, reg);
    }

    fn close(&mut self, reg: &Registry) {
        let mut dummy = TxQueue::new();
        let slots: Vec<usize> = self.conns.keys().copied().collect();
        for slot in slots { self.close_slot(slot, false, &mut dummy, reg); }
    }

    fn pause_source_reads(&mut self, reg: &Registry) {
        if self.bp_paused { return; }
        self.bp_paused = true;
        self.update_conn_interest_all(reg);
    }

    fn resume_source_reads(&mut self, reg: &Registry) {
        if !self.bp_paused { return; }
        self.bp_paused = false;
        self.update_conn_interest_all(reg);
    }
}

// ---------------------------------------------------------------------------
// Free helper: update mio interest for a single TcpDestChannel connection
// ---------------------------------------------------------------------------

fn update_dst_conn_interest(ch_id: u8, bp_paused: bool, slot: usize,
                             conn: &mut TcpConn, reg: &Registry) {
    let needs_write = !conn.txbuf.is_empty() || conn.connecting;
    let interest = match (bp_paused, needs_write) {
        (false, false) => Interest::READABLE,
        (false, true)  => Interest::READABLE | Interest::WRITABLE,
        (true,  true)  => Interest::WRITABLE,
        (true,  false) => return,
    };
    if conn.in_sel {
        let _ = reg.reregister(&mut conn.stream, tcp_slot_token(ch_id, slot), interest);
    }
}

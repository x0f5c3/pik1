use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll};

use crate::channel::{channel_id_for_token, Channel, TOKEN_LINK};
use crate::protocol::{
    build_frame, FrameParser, TxQueue, F_ACK, F_DATA, F_HELLO, F_PING, F_PONG, F_TDATA,
};
use crate::serial::{find_acm_by_usb_id, log, open_serial_fd, read_nonblock};

// ---------------------------------------------------------------------------
// Tuning constants
// ---------------------------------------------------------------------------

const MAX_TICK:        Duration = Duration::from_secs(1);
const KA_INTERVAL:     Duration = Duration::from_secs(3);
const KA_TIMEOUT:      Duration = Duration::from_secs(10);
const LINK_HIGH_WATER: usize    = 512 * 1024;
const LINK_LOW_WATER:  usize    = 256 * 1024;

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

pub struct Daemon {
    mode:     String,
    link_dev: Option<String>,
    usb_id:   Option<(String, String)>,

    // All channels indexed by their channel_id.
    channels: HashMap<u8, Box<dyn Channel>>,

    // Single outbound TX queue that all channels write directly into.
    txq: TxQueue,

    // Link state
    parser:        FrameParser,
    link_fd:       Option<RawFd>,
    link_up:       bool,
    link_bp:       bool,   // link TX queue above high-water; channel reads paused
    disconnected:  bool,
    reopen_at:     Instant,
    reopen_delay:  Duration,
    last_rx:       Option<Instant>,
    last_tx:       Option<Instant>,
}

impl Daemon {
    pub fn new(
        mode: String,
        link_dev: Option<String>,
        usb_id: Option<(String, String)>,
        channels: Vec<Box<dyn Channel>>,
    ) -> Self {
        let ch_map: HashMap<u8, Box<dyn Channel>> =
            channels.into_iter().map(|c| (c.channel_id(), c)).collect();
        Daemon {
            mode, link_dev, usb_id,
            channels: ch_map,
            txq:    TxQueue::new(),
            parser: FrameParser::new(),
            link_fd:  None,
            link_up:  false,
            link_bp:  false,
            disconnected: true,
            reopen_at:    Instant::now(),
            reopen_delay: Duration::from_millis(500),
            last_rx: None,
            last_tx: None,
        }
    }

    // -----------------------------------------------------------------------
    // Link device resolution
    // -----------------------------------------------------------------------

    fn resolve_dev(&self) -> Option<String> {
        if let Some((vid, pid)) = &self.usb_id {
            return find_acm_by_usb_id(vid, pid);
        }
        self.link_dev.clone()
    }

    // -----------------------------------------------------------------------
    // Link open / close
    // -----------------------------------------------------------------------

    fn open_link(&mut self, poll: &Poll) {
        let dev = match self.resolve_dev() {
            Some(d) => d,
            None => {
                log("Link: no device available");
                self.schedule_reopen();
                return;
            }
        };
        match open_serial_fd(&dev, 115200) {
            Ok(fd) => {
                if let Err(e) = poll.registry().register(
                    &mut SourceFd(&fd), TOKEN_LINK, Interest::READABLE)
                {
                    log(&format!("Link: register error: {}", e));
                    unsafe { libc::close(fd); }
                    self.schedule_reopen();
                    return;
                }
                self.link_fd      = Some(fd);
                self.disconnected = false;
                self.reopen_delay = Duration::from_millis(500);
                self.link_bp      = false;
                self.parser.reset();
                self.txq.reset();
                self.last_rx = Some(Instant::now());
                self.last_tx = Some(Instant::now());
                log(&format!("Link: opened {}", dev));
                // Initiate handshake
                self.txq.enqueue(&build_frame(F_HELLO, 0, b""));
                self.set_link_write_interest(poll, true);
            }
            Err(e) => {
                log(&format!("Link: cannot open {}: {}", dev, e));
                self.schedule_reopen();
            }
        }
    }

    fn close_link(&mut self, reason: &str, poll: &Poll) {
        if self.disconnected { return; }
        log(&format!("Link: down -- {}", reason));
        self.disconnected = true;
        self.link_up      = false;
        if let Some(fd) = self.link_fd.take() {
            let _ = poll.registry().deregister(&mut SourceFd(&fd));
            unsafe { libc::close(fd); }
        }
        self.txq.reset();
        // Resume all channels before signalling link-down so they can flush state.
        if self.link_bp {
            self.link_bp = false;
            let channels = &mut self.channels;
            let reg = poll.registry();
            for ch in channels.values_mut() { ch.resume_source_reads(reg); }
        }
        let channels = &mut self.channels;
        let reg = poll.registry();
        for ch in channels.values_mut() { ch.on_link_disconnect(reg); }
        self.schedule_reopen();
    }

    fn schedule_reopen(&mut self) {
        self.reopen_at    = Instant::now() + self.reopen_delay;
        self.reopen_delay = self.reopen_delay.saturating_mul(2).min(Duration::from_secs(8));
    }

    // -----------------------------------------------------------------------
    // Link I/O
    // -----------------------------------------------------------------------

    /// Update link fd read/write registration interest.
    fn set_link_write_interest(&self, poll: &Poll, want_write: bool) {
        if let Some(fd) = self.link_fd {
            let interest = if want_write {
                Interest::READABLE | Interest::WRITABLE
            } else {
                Interest::READABLE
            };
            let _ = poll.registry().reregister(&mut SourceFd(&fd), TOKEN_LINK, interest);
        }
    }

    fn link_read(&mut self, poll: &Poll) {
        let fd = match self.link_fd { Some(f) => f, None => return };
        let mut buf = [0u8; 65536];
        match read_nonblock(fd, &mut buf) {
            Ok(None) | Ok(Some(0)) => {
                // 0-byte read on a non-blocking TTY (VMIN=0) = no data, NOT EOF.
                // Real disconnect raises OSError(EIO), caught below.
            }
            Ok(Some(n)) => {
                self.last_rx = Some(Instant::now());
                // Collect frames first so we hold no borrow on self.parser during
                // dispatch, which needs &mut self.channels / &mut self.txq.
                let mut frames: Vec<(u8, u8, Vec<u8>)> = Vec::new();
                self.parser.feed(&buf[..n], |ftype, channel, payload| {
                    frames.push((ftype, channel, payload.to_vec()));
                });
                for (ftype, channel, payload) in frames {
                    self.dispatch_frame(ftype, channel, &payload, poll);
                }
            }
            Err(e) => {
                self.close_link(&format!("read error: {}", e), poll);
            }
        }
    }

    fn link_write(&mut self, poll: &Poll) {
        let fd = match self.link_fd { Some(f) => f, None => return };
        match self.txq.drain_to_fd(fd) {
            Err(e) => {
                self.close_link(&format!("write error: {}", e), poll);
                return;
            }
            Ok(_) => {}
        }
        // Backpressure: resume channel reads when queue drains below low-water.
        if self.link_bp && self.txq.queued_bytes <= LINK_LOW_WATER {
            self.link_bp = false;
            let channels = &mut self.channels;
            let reg = poll.registry();
            for ch in channels.values_mut() { ch.resume_source_reads(reg); }
        }
        if self.txq.is_empty() {
            self.set_link_write_interest(poll, false);
        }
    }

    /// Enqueue a pre-built frame and kick the write interest.
    fn enqueue_frame(&mut self, frame: Vec<u8>, poll: &Poll) {
        if self.disconnected || self.link_fd.is_none() { return; }
        self.txq.enqueue(&frame);
        self.last_tx = Some(Instant::now());
        self.set_link_write_interest(poll, true);
    }

    /// Apply backpressure check: pause channel reads if queue is above high-water.
    fn check_bp_high(&mut self, poll: &Poll) {
        if !self.link_bp && self.txq.queued_bytes >= LINK_HIGH_WATER {
            self.link_bp = true;
            let channels = &mut self.channels;
            let reg = poll.registry();
            for ch in channels.values_mut() { ch.pause_source_reads(reg); }
        }
    }

    // -----------------------------------------------------------------------
    // Frame dispatch
    // -----------------------------------------------------------------------

    fn dispatch_frame(&mut self, ftype: u8, channel: u8, payload: &[u8], poll: &Poll) {
        match ftype {
            F_PING => {
                self.enqueue_frame(build_frame(F_PONG, 0, b""), poll);
                return;
            }
            F_PONG => return,
            F_HELLO => {
                self.enqueue_frame(build_frame(F_ACK, 0, b""), poll);
                if self.link_up {
                    log("Link: peer reconnected, rebroadcasting channel states");
                    let ch_ids: Vec<u8> = self.channels.keys().copied().collect();
                    for id in ch_ids {
                        let ch = &mut self.channels;
                        let txq = &mut self.txq;
                        let reg = poll.registry();
                        if let Some(c) = ch.get_mut(&id) { c.on_link_connect(txq, reg); }
                    }
                    self.last_tx = Some(Instant::now());
                    self.set_link_write_interest(poll, !self.txq.is_empty());
                    return;
                }
                log("Link: received HELLO from peer");
                self.on_link_up(poll);
                return;
            }
            F_ACK => {
                log("Link: received ACK from peer");
                self.on_link_up(poll);
                return;
            }
            _ => {}
        }

        // Data / control frames → route to channel
        {
            // Apply backpressure before writing (mirrors Python's send())
            let bulk = ftype == F_DATA || ftype == F_TDATA;
            if bulk { self.check_bp_high(poll); }

            let ch = &mut self.channels;
            let txq = &mut self.txq;
            let reg = poll.registry();
            if let Some(c) = ch.get_mut(&channel) {
                c.on_frame(ftype, payload, txq, reg);
            }
        }
        self.last_tx = Some(Instant::now());
        self.set_link_write_interest(poll, !self.txq.is_empty());
    }

    fn on_link_up(&mut self, poll: &Poll) {
        if self.link_up { return; }
        self.link_up = true;
        log(&format!("Link: handshake complete ({})", self.mode));
        let ch_ids: Vec<u8> = self.channels.keys().copied().collect();
        for id in ch_ids {
            let ch  = &mut self.channels;
            let txq = &mut self.txq;
            let reg = poll.registry();
            if let Some(c) = ch.get_mut(&id) { c.on_link_connect(txq, reg); }
        }
        self.last_tx = Some(Instant::now());
        self.set_link_write_interest(poll, !self.txq.is_empty());
    }

    // -----------------------------------------------------------------------
    // Keepalive
    // -----------------------------------------------------------------------

    fn tick_keepalive(&mut self, now: Instant, poll: &Poll) {
        if self.disconnected { return; }
        if let Some(last) = self.last_rx {
            if now.duration_since(last) > KA_TIMEOUT {
                self.close_link("keepalive timeout", poll);
                return;
            }
        }
        if self.link_up {
            if let Some(lt) = self.last_tx {
                if now.duration_since(lt) >= KA_INTERVAL && self.txq.is_empty() {
                    self.enqueue_frame(build_frame(F_PING, 0, b""), poll);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Timeout computation
    // -----------------------------------------------------------------------

    fn next_timeout(&self, now: Instant) -> Duration {
        let mut deadline = now + MAX_TICK;
        if self.disconnected {
            deadline = deadline.min(self.reopen_at);
        } else if self.link_up {
            if let Some(lt) = self.last_tx { deadline = deadline.min(lt + KA_INTERVAL); }
            if let Some(lr) = self.last_rx { deadline = deadline.min(lr + KA_TIMEOUT); }
        }
        for ch in self.channels.values() {
            if let Some(d) = ch.next_deadline() { deadline = deadline.min(d); }
        }
        if deadline > now { deadline - now } else { Duration::ZERO }
    }

    // -----------------------------------------------------------------------
    // Main loop
    // -----------------------------------------------------------------------

    pub fn run(&mut self, poll: &mut Poll) {
        log(&format!("serialmux {} started", self.mode));
        let mut events = Events::with_capacity(128);
        if self.disconnected { self.open_link(poll); }

        loop {
            let now = Instant::now();

            if self.disconnected && now >= self.reopen_at {
                self.open_link(poll);
            }

            self.tick_keepalive(now, poll);

            // Tick all channels (timers, UART reopen, etc.)
            let ch_ids: Vec<u8> = self.channels.keys().copied().collect();
            for id in ch_ids {
                let ch  = &mut self.channels;
                let txq = &mut self.txq;
                let reg = poll.registry();
                if let Some(c) = ch.get_mut(&id) { c.tick(now, txq, reg); }
            }
            if !self.txq.is_empty() {
                self.last_tx = Some(Instant::now());
                self.set_link_write_interest(poll, true);
            }

            let timeout = self.next_timeout(Instant::now());
            if let Err(e) = poll.poll(&mut events, Some(timeout)) {
                if e.kind() != std::io::ErrorKind::Interrupted {
                    log(&format!("poll error: {}", e));
                }
                continue;
            }

            for event in events.iter() {
                let token    = event.token();
                let readable = event.is_readable();
                let writable = event.is_writable();

                if token == TOKEN_LINK {
                    if self.link_fd.is_none() { continue; } // stale event
                    if readable { self.link_read(poll); }
                    if writable && self.link_fd.is_some() { self.link_write(poll); }
                } else if let Some(ch_id) = channel_id_for_token(token) {
                    // Apply high-water backpressure before dispatching
                    // (conservative: always check here; channel reads are the source)
                    {
                        let ch  = &mut self.channels;
                        let txq = &mut self.txq;
                        let reg = poll.registry();
                        if let Some(c) = ch.get_mut(&ch_id) {
                            c.handle_event(token, readable, writable, txq, reg);
                        }
                    }
                    self.check_bp_high(poll);
                    self.last_tx = Some(Instant::now());
                    self.set_link_write_interest(poll, !self.txq.is_empty());
                }
            }
        }
    }
}

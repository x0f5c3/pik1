//! Event-loop daemon.
//!
//! [`Daemon`] owns the link fd, all channels, and the TX queue.  It runs a
//! single-threaded `mio` event loop that:
//!
//! 1. Resolves and opens the link device (CDC ACM or a fixed path).
//! 2. Performs the `F_HELLO` / `F_ACK` handshake.
//! 3. Routes incoming frames to the appropriate channel.
//! 4. Reads channel I/O events and enqueues `F_*` frames for the link.
//! 5. Applies backpressure (pause / resume channel reads at the high/low
//!    water marks of the TX queue).
//! 6. Sends keepalive `F_PING` frames and times out a silent link.

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll};

use crate::channel::{channel_id_for_token, Channel, TOKEN_LINK};
use crate::protocol::{
    build_frame, FrameParser, TxQueue, F_ACK, F_DATA, F_HELLO, F_PING, F_PONG, F_TDATA,
    LINK_HIGH_WATER, LINK_LOW_WATER,
};
use crate::serial::{close_raw, find_acm_by_usb_id, open_serial_fd, read_nonblock};

// ─────────────────────────────────────────────────────────────────────────────
// Tuning constants
// ─────────────────────────────────────────────────────────────────────────────

/// Maximum time to block in `poll()` before running timers.
const MAX_TICK:    Duration = Duration::from_secs(1);
/// Send a `F_PING` after this much idle TX time.
const KA_INTERVAL: Duration = Duration::from_secs(3);
/// Drop the link if no bytes arrive within this window.
const KA_TIMEOUT:  Duration = Duration::from_secs(10);

// ─────────────────────────────────────────────────────────────────────────────
// Daemon
// ─────────────────────────────────────────────────────────────────────────────

/// Central event-loop state.
pub struct Daemon {
    /// `"exporter"` or `"host"` — used for logging.
    mode:     String,
    /// Fixed device path (mutually exclusive with `usb_id`).
    link_dev: Option<String>,
    /// USB VID/PID to discover via sysfs (mutually exclusive with `link_dev`).
    usb_id:   Option<(String, String)>,

    /// All channels indexed by wire channel id.
    channels: HashMap<u8, Box<dyn Channel>>,

    /// Single outbound TX ring buffer shared by all channels.
    txq: TxQueue,

    // ── Link state ────────────────────────────────────────────────────────
    parser:       FrameParser,
    link_fd:      Option<RawFd>,
    /// Handshake complete.
    link_up:      bool,
    /// TX queue above high-water; channel reads are paused.
    link_bp:      bool,
    /// No link fd open; waiting to (re-)open.
    disconnected: bool,
    reopen_at:    Instant,
    reopen_delay: Duration,
    last_rx:      Option<Instant>,
    last_tx:      Option<Instant>,
}

impl Daemon {
    /// Create a new daemon.  `channels` will be registered with mio by the
    /// channel constructors before being passed here.
    pub fn new(
        mode:     String,
        link_dev: Option<String>,
        usb_id:   Option<(String, String)>,
        channels: Vec<Box<dyn Channel>>,
    ) -> Self {
        let ch_map: HashMap<u8, Box<dyn Channel>> =
            channels.into_iter().map(|c| (c.channel_id(), c)).collect();
        Daemon {
            mode,
            link_dev,
            usb_id,
            channels: ch_map,
            txq:          TxQueue::new(),
            parser:       FrameParser::new(),
            link_fd:      None,
            link_up:      false,
            link_bp:      false,
            disconnected: true,
            reopen_at:    Instant::now(),
            reopen_delay: Duration::from_millis(500),
            last_rx:      None,
            last_tx:      None,
        }
    }

    // ── Link device resolution ────────────────────────────────────────────

    /// Resolve the link device path from USB sysfs or the fixed path.
    fn resolve_dev(&self) -> Option<String> {
        if let Some((vid, pid)) = &self.usb_id {
            return find_acm_by_usb_id(vid, pid);
        }
        self.link_dev.clone()
    }

    // ── Link open / close ─────────────────────────────────────────────────

    fn open_link(&mut self, poll: &Poll) {
        let dev = match self.resolve_dev() {
            Some(d) => d,
            None => {
                tracing::warn!("Link: no device available");
                self.schedule_reopen();
                return;
            }
        };
        match open_serial_fd(&dev, 115200) {
            Ok(fd) => {
                if let Err(e) = poll.registry().register(
                    &mut SourceFd(&fd),
                    TOKEN_LINK,
                    Interest::READABLE,
                ) {
                    tracing::warn!(err = %e, "Link: register error");
                    // SAFETY: `fd` was just opened and we are discarding it.
                    close_raw(fd);
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
                tracing::info!(device = %dev, "Link: opened");
                // Initiate handshake.
                self.txq.enqueue(&build_frame(F_HELLO, 0, b""));
                self.set_link_write_interest(poll, true);
            }
            Err(e) => {
                tracing::warn!(device = %dev, err = %e, "Link: cannot open");
                self.schedule_reopen();
            }
        }
    }

    fn close_link(&mut self, reason: &str, poll: &Poll) {
        if self.disconnected { return; }
        tracing::warn!(%reason, "Link: down");
        self.disconnected = true;
        self.link_up      = false;
        if let Some(fd) = self.link_fd.take() {
            let _ = poll.registry().deregister(&mut SourceFd(&fd));
            // SAFETY: `fd` is the link fd we are closing; it won't be used again.
            close_raw(fd);
        }
        self.txq.reset();
        // Resume all channels before signalling link-down so they can flush.
        if self.link_bp {
            self.link_bp = false;
            let channels = &mut self.channels;
            let reg      = poll.registry();
            for ch in channels.values_mut() { ch.resume_source_reads(reg); }
        }
        let channels = &mut self.channels;
        let reg      = poll.registry();
        for ch in channels.values_mut() { ch.on_link_disconnect(reg); }
        self.schedule_reopen();
    }

    fn schedule_reopen(&mut self) {
        self.reopen_at    = Instant::now() + self.reopen_delay;
        self.reopen_delay = self.reopen_delay.saturating_mul(2).min(Duration::from_secs(8));
    }

    // ── Link I/O ──────────────────────────────────────────────────────────

    /// Update the link fd's mio interest (add / remove WRITABLE).
    fn set_link_write_interest(&self, poll: &Poll, want_write: bool) {
        if let Some(fd) = self.link_fd {
            let interest = if want_write {
                Interest::READABLE | Interest::WRITABLE
            } else {
                Interest::READABLE
            };
            let _ = poll
                .registry()
                .reregister(&mut SourceFd(&fd), TOKEN_LINK, interest);
        }
    }

    fn link_read(&mut self, poll: &Poll) {
        let fd = match self.link_fd { Some(f) => f, None => return };
        let mut buf = [0u8; 65536];
        match read_nonblock(fd, &mut buf) {
            Ok(None) | Ok(Some(0)) => {
                // 0-byte read on a non-blocking TTY (VMIN=0) = no data, not EOF.
                // Real disconnect raises EIO, caught in the Err branch.
            }
            Ok(Some(n)) => {
                self.last_rx = Some(Instant::now());
                // Collect frames first: the borrow on `self.parser` must end
                // before we call `dispatch_frame` which needs `&mut self`.
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
            Ok(_empty) => {}
        }
        // Backpressure: resume channel reads when queue drains below low-water.
        if self.link_bp && self.txq.used() <= LINK_LOW_WATER {
            self.link_bp = false;
            let channels = &mut self.channels;
            let reg      = poll.registry();
            for ch in channels.values_mut() { ch.resume_source_reads(reg); }
        }
        if self.txq.is_empty() {
            self.set_link_write_interest(poll, false);
        }
    }

    /// Enqueue a pre-built frame and arm the write interest.
    fn enqueue_frame(&mut self, frame: Vec<u8>, poll: &Poll) {
        if self.disconnected || self.link_fd.is_none() { return; }
        self.txq.enqueue(&frame);
        self.last_tx = Some(Instant::now());
        self.set_link_write_interest(poll, true);
    }

    /// Apply high-water backpressure: pause channel reads if the TX queue is
    /// above [`LINK_HIGH_WATER`].
    fn check_bp_high(&mut self, poll: &Poll) {
        if !self.link_bp && self.txq.used() >= LINK_HIGH_WATER {
            self.link_bp = true;
            let channels = &mut self.channels;
            let reg      = poll.registry();
            for ch in channels.values_mut() { ch.pause_source_reads(reg); }
        }
    }

    // ── Frame dispatch ────────────────────────────────────────────────────

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
                    tracing::info!("Link: peer reconnected, rebroadcasting channel states");
                    let ch_ids: Vec<u8> = self.channels.keys().copied().collect();
                    for id in ch_ids {
                        let ch  = &mut self.channels;
                        let txq = &mut self.txq;
                        let reg = poll.registry();
                        if let Some(c) = ch.get_mut(&id) { c.on_link_connect(txq, reg); }
                    }
                    self.last_tx = Some(Instant::now());
                    self.set_link_write_interest(poll, !self.txq.is_empty());
                    return;
                }
                tracing::info!("Link: received HELLO from peer");
                self.on_link_up(poll);
                return;
            }
            F_ACK => {
                tracing::info!("Link: received ACK from peer");
                self.on_link_up(poll);
                return;
            }
            _ => {}
        }

        // Bulk data / TCP control frames — route to channel.
        {
            let bulk = ftype == F_DATA || ftype == F_TDATA;
            if bulk { self.check_bp_high(poll); }

            let ch  = &mut self.channels;
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
        tracing::info!(mode = %self.mode, "Link: handshake complete");
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

    // ── Keepalive ─────────────────────────────────────────────────────────

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

    // ── Timeout computation ───────────────────────────────────────────────

    /// Compute how long to block in the next `poll()` call.
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

    // ── Main loop ─────────────────────────────────────────────────────────

    /// Run the event loop until killed.
    pub fn run(&mut self, poll: &mut Poll) {
        tracing::info!(mode = %self.mode, "serialmux started");
        let mut events = Events::with_capacity(128);
        if self.disconnected { self.open_link(poll); }

        loop {
            let now = Instant::now();

            if self.disconnected && now >= self.reopen_at {
                self.open_link(poll);
            }

            self.tick_keepalive(now, poll);

            // Tick all channels (UART reopen, MCU silence timeout, …).
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
                    tracing::warn!(err = %e, "poll error");
                }
                continue;
            }

            for event in events.iter() {
                let token    = event.token();
                let readable = event.is_readable();
                let writable = event.is_writable();

                if token == TOKEN_LINK {
                    if self.link_fd.is_none() { continue; } // stale event after close
                    if readable { self.link_read(poll); }
                    if writable && self.link_fd.is_some() { self.link_write(poll); }
                } else if let Some(ch_id) = channel_id_for_token(token) {
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

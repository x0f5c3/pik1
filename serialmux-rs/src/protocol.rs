//! Frame protocol for the serialmux link.
//!
//! Wire format (all integers little-endian):
//! ```text
//! [ 0xAA 0x55 ][ ftype:1 ][ channel:1 ][ length:2 LE ][ payload:N ][ crc32:4 LE ]
//! ```
//!
//! This module mirrors the C `serialmux.h` / `serialmux.c` implementation
//! (CRC polynomial, constants, parser behaviour, ring-buffer TX queue).

pub const HDR_SIZE:    usize = 6;  // magic(2) + type(1) + channel(1) + length(2 LE)
pub const CRC_SIZE:    usize = 4;
/// Maximum payload bytes in a single frame.
pub const MAX_PAYLOAD: usize = 16 * 1024;
/// Total size of the largest possible frame.
pub const MAX_FRAME:   usize = HDR_SIZE + MAX_PAYLOAD + CRC_SIZE;

// ── Frame type constants (must match C serialmux.h) ─────────────────────────
/// Raw MCU serial data (exporter → host or host → MCU).
pub const F_DATA:   u8 = 0x01;
/// MCU is resetting; host should close the PTY.
pub const F_FLUSH:  u8 = 0x02;
/// MCU is ready; host should open the PTY.
pub const F_READY:  u8 = 0x03;
/// Link handshake initiation.
pub const F_HELLO:  u8 = 0x05;
/// Link handshake acknowledgement.
pub const F_ACK:    u8 = 0x06;
/// TCP tunnel: new connection notification.
pub const F_TCONN:  u8 = 0x10;
/// TCP tunnel: payload bytes.
pub const F_TDATA:  u8 = 0x11;
/// TCP tunnel: connection closed.
pub const F_TCLOSE: u8 = 0x12;
/// Keepalive probe.
pub const F_PING:   u8 = 0x20;
/// Keepalive reply.
pub const F_PONG:   u8 = 0x21;

/// Klipper firmware sync byte.  Seeing `0x7E` after silence means the MCU
/// has booted and Klipper is running.
pub const KLIPPER_SYNC: u8 = 0x7E;

// ── Backpressure thresholds ──────────────────────────────────────────────────
/// Pause all channel reads when the TX queue exceeds this many bytes.
pub const LINK_HIGH_WATER: usize = 512 * 1024;
/// Resume channel reads when the TX queue drains below this many bytes.
pub const LINK_LOW_WATER:  usize = 256 * 1024;
/// TX ring-buffer capacity: high-water + one max frame + guard margin.
pub const LINK_TXBUF_SIZE: usize = LINK_HIGH_WATER + MAX_FRAME + 64;

// ── Parser buffer capacity ───────────────────────────────────────────────────
/// Fixed capacity of the [`FrameParser`] internal buffer (one max frame + guard).
/// Matches the C `frame_parser_t.buf[MAX_FRAME + 64]` field.
const PARSER_BUF_CAP: usize = MAX_FRAME + 64;

// ─────────────────────────────────────────────────────────────────────────────
// Frame builder
// ─────────────────────────────────────────────────────────────────────────────

/// Build a complete framed packet ready to push into the TX queue.
///
/// Layout: `[0xAA 0x55][ftype][channel][len_lo][len_hi][payload…][crc0..crc3]`
pub fn build_frame(ftype: u8, channel: u8, payload: &[u8]) -> Vec<u8> {
    let length = payload.len();
    // CRC32 with the standard reflected polynomial (zlib / ISO 3309), matching
    // the C `crc32_buf()` implementation (poly 0xEDB88320).
    let crc = if length > 0 { crc32fast::hash(payload) } else { 0 };
    let mut out = Vec::with_capacity(HDR_SIZE + length + CRC_SIZE);
    out.extend_from_slice(&[0xAA, 0x55, ftype, channel]);
    out.extend_from_slice(&(length as u16).to_le_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Frame parser
// ─────────────────────────────────────────────────────────────────────────────

/// Streaming frame parser with a fixed-capacity internal buffer.
///
/// Feed arbitrary byte chunks via [`FrameParser::feed`]; the callback is
/// invoked for every CRC-valid complete frame.  The parser re-syncs
/// automatically after corruption or noise by skipping invalid magic bytes.
///
/// Implementation mirrors the C `fp_feed()` function, including the outer
/// chunking loop that prevents the fixed buffer from overflowing on large
/// single `read()` returns.
pub struct FrameParser {
    /// Fixed-capacity staging buffer.  `len` bytes are valid starting at index 0.
    buf: Box<[u8]>,
    /// Number of valid bytes in `buf`.
    len: usize,
}

impl Default for FrameParser {
    fn default() -> Self { Self::new() }
}

impl FrameParser {
    /// Create a new parser with an empty buffer pre-allocated to
    /// [`PARSER_BUF_CAP`] bytes.
    pub fn new() -> Self {
        FrameParser {
            buf: vec![0u8; PARSER_BUF_CAP].into_boxed_slice(),
            len: 0,
        }
    }

    /// Reset parser state.  Call after a link reconnect.
    pub fn reset(&mut self) {
        self.len = 0;
    }

    /// Feed a chunk of raw link data.
    ///
    /// Because the internal buffer has a fixed capacity (`PARSER_BUF_CAP`),
    /// large inputs are consumed in multiple passes — each pass fills the
    /// buffer, then drains all complete frames before accepting more input.
    /// This matches the C `fp_feed()` outer loop and prevents silent byte
    /// drops on reads larger than 16 KiB.
    ///
    /// `cb(ftype, channel, payload)` is invoked for each valid frame.
    pub fn feed<F>(&mut self, data: &[u8], mut cb: F)
    where
        F: FnMut(u8, u8, &[u8]),
    {
        let mut pos = 0usize;
        while pos < data.len() {
            // Fill: copy only as much as fits in the fixed-size buffer.
            let space = PARSER_BUF_CAP - self.len;
            let copy  = std::cmp::min(data.len() - pos, space);
            if copy == 0 {
                // Buffer full with no progress — should not happen under normal
                // backpressure, but guard to avoid an infinite loop.
                break;
            }
            self.buf[self.len..self.len + copy].copy_from_slice(&data[pos..pos + copy]);
            self.len += copy;
            pos      += copy;

            // Drain all complete frames from the buffer before the next fill.
            self.drain_frames(&mut cb);
        }
    }

    /// Parse and dispatch all complete frames from the internal buffer.
    fn drain_frames<F>(&mut self, cb: &mut F)
    where
        F: FnMut(u8, u8, &[u8]),
    {
        loop {
            if self.len < 2 { break; }

            // Search for magic 0xAA 0x55.
            let mut i = 0usize;
            while i + 1 < self.len
                && !(self.buf[i] == 0xAA && self.buf[i + 1] == 0x55)
            {
                i += 1;
            }

            if i + 1 >= self.len {
                // No complete magic found.  Keep the last byte (may be 0xAA,
                // the first byte of the next magic), matching C behaviour.
                self.buf[0] = self.buf[self.len - 1];
                self.len = 1;
                break;
            }

            // Slide magic to front.
            if i > 0 {
                self.buf.copy_within(i..self.len, 0);
                self.len -= i;
            }

            if self.len < HDR_SIZE { break; }

            // Decode header: magic(2) + type(1) + channel(1) + length(2 LE).
            let ftype   = self.buf[2];
            let channel = self.buf[3];
            let length  = (self.buf[4] as usize) | ((self.buf[5] as usize) << 8);

            if length > MAX_PAYLOAD {
                // Bad length field — skip the magic pair and rescan.
                self.buf.copy_within(2..self.len, 0);
                self.len -= 2;
                continue;
            }

            let total = HDR_SIZE + length + CRC_SIZE;
            if self.len < total { break; }

            // Verify CRC32.
            let crc_rx = u32::from_le_bytes(
                self.buf[HDR_SIZE + length..HDR_SIZE + length + 4]
                    .try_into()
                    .unwrap(),
            );
            let crc_calc = crc32fast::hash(&self.buf[HDR_SIZE..HDR_SIZE + length]);
            if crc_calc != crc_rx {
                self.buf.copy_within(2..self.len, 0);
                self.len -= 2;
                continue;
            }

            // Frame is valid.  Copy payload before consuming buffer bytes.
            let payload: Vec<u8> = self.buf[HDR_SIZE..HDR_SIZE + length].to_vec();
            self.buf.copy_within(total..self.len, 0);
            self.len -= total;
            cb(ftype, channel, &payload);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TX ring buffer
// ─────────────────────────────────────────────────────────────────────────────

/// Outbound frame ring buffer for the CDC link.
///
/// Frames are pushed at the tail and drained from the head with a single
/// `write()` per drain call.  The underlying buffer is fixed at
/// [`LINK_TXBUF_SIZE`] bytes and pre-allocated at construction; no further
/// heap allocation occurs during normal operation.
///
/// This mirrors the C `link_txq_t` ring buffer (same capacity, same head/tail
/// arithmetic, same `txq_push` / `txq_drain` semantics).
pub struct TxQueue {
    /// Fixed-size backing store.
    buf:  Box<[u8]>,
    /// Read position (bytes available from `head` to `tail`).
    head: usize,
    /// Write position.
    tail: usize,
}

impl Default for TxQueue {
    fn default() -> Self { Self::new() }
}

impl TxQueue {
    /// Allocate a new ring buffer of capacity [`LINK_TXBUF_SIZE`].
    pub fn new() -> Self {
        TxQueue {
            buf:  vec![0u8; LINK_TXBUF_SIZE].into_boxed_slice(),
            head: 0,
            tail: 0,
        }
    }

    /// Reset to empty.  Call after a link reconnect.
    pub fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
    }

    /// Number of bytes currently queued.
    #[inline]
    pub fn used(&self) -> usize {
        (self.tail + LINK_TXBUF_SIZE - self.head) % LINK_TXBUF_SIZE
    }

    /// Whether the queue is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    /// Enqueue `data` bytes at the tail.
    ///
    /// If there is insufficient space (i.e. the caller did not apply
    /// backpressure correctly), the frame is silently dropped and a log
    /// message is emitted — matching the C `txq_push` overflow behaviour.
    pub fn enqueue(&mut self, data: &[u8]) {
        let n = data.len();
        if n == 0 { return; }
        if n > LINK_TXBUF_SIZE - 1 - self.used() {
            crate::serial::log("TxQueue: overflow — dropping frame (backpressure missed)");
            return;
        }
        let space_to_end = LINK_TXBUF_SIZE - self.tail;
        if n <= space_to_end {
            self.buf[self.tail..self.tail + n].copy_from_slice(data);
        } else {
            self.buf[self.tail..].copy_from_slice(&data[..space_to_end]);
            self.buf[..n - space_to_end].copy_from_slice(&data[space_to_end..]);
        }
        self.tail = (self.tail + n) % LINK_TXBUF_SIZE;
    }

    /// Write the next contiguous segment to `fd` with a single `write()`.
    ///
    /// Returns `Ok(true)` if the queue is now empty, `Ok(false)` if bytes
    /// remain (including `EAGAIN` / `EWOULDBLOCK`), or `Err(_)` on a real
    /// write error.
    pub fn drain_to_fd(&mut self, fd: libc::c_int) -> std::io::Result<bool> {
        if self.is_empty() { return Ok(true); }

        // Write the contiguous segment from `head`.  If the data wraps around,
        // only the first segment is written; the caller loops back on the next
        // writable event.
        let (head, tail) = (self.head, self.tail);
        let avail = if tail >= head { tail - head } else { LINK_TXBUF_SIZE - head };

        // SAFETY: `fd` is a valid open non-blocking file descriptor; the slice
        // `buf[head..head+avail]` lies within the allocated buffer.
        let written = unsafe {
            libc::write(
                fd,
                self.buf[head..].as_ptr() as *const libc::c_void,
                avail,
            )
        };
        if written < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error()
                .map_or(false, |c| c == libc::EAGAIN || c == libc::EWOULDBLOCK)
            {
                return Ok(false);
            }
            return Err(e);
        }
        self.head = (self.head + written as usize) % LINK_TXBUF_SIZE;
        Ok(self.is_empty())
    }
}

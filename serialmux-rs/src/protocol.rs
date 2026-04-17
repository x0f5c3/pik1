// Frame format:  [ 0xAA 0x55 ][ type:1 ][ channel:1 ][ length:2 LE ][ payload:N ][ crc32:4 LE ]

pub const MAGIC: [u8; 2] = [0xAA, 0x55];
pub const HDR_SIZE: usize = 6; // magic:2 + type:1 + channel:1 + length:2 LE
pub const CRC_SIZE: usize = 4;

// Frame type constants
pub const F_DATA:   u8 = 0x01;
pub const F_FLUSH:  u8 = 0x02;
pub const F_READY:  u8 = 0x03;
pub const F_HELLO:  u8 = 0x05;
pub const F_ACK:    u8 = 0x06;
pub const F_TCONN:  u8 = 0x10;
pub const F_TDATA:  u8 = 0x11;
pub const F_TCLOSE: u8 = 0x12;
pub const F_PING:   u8 = 0x20;
pub const F_PONG:   u8 = 0x21;

pub const MAX_PAYLOAD: usize = 16 * 1024;

// Klipper firmware sync byte: seeing this after silence means MCU has booted.
pub const KLIPPER_SYNC: u8 = 0x7E;

/// Build a complete framed packet from (ftype, channel, payload).
pub fn build_frame(ftype: u8, channel: u8, payload: &[u8]) -> Vec<u8> {
    let length = payload.len();
    let crc = crc32fast::hash(payload);
    let mut out = Vec::with_capacity(HDR_SIZE + length + CRC_SIZE);
    out.extend_from_slice(&MAGIC);
    out.push(ftype);
    out.push(channel);
    out.push((length & 0xFF) as u8);
    out.push(((length >> 8) & 0xFF) as u8);
    out.extend_from_slice(payload);
    out.push((crc & 0xFF) as u8);
    out.push(((crc >> 8) & 0xFF) as u8);
    out.push(((crc >> 16) & 0xFF) as u8);
    out.push(((crc >> 24) & 0xFF) as u8);
    out
}

/// Streaming frame parser.  Feed arbitrary byte chunks; fires `on_frame` for
/// every CRC-valid complete frame.  Re-syncs automatically on corruption.
pub struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    pub fn new() -> Self {
        FrameParser { buf: Vec::new() }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// Feed a chunk of data.  For each complete valid frame, `cb(ftype, channel, payload)` is
    /// called with a slice into the parser's internal buffer (no heap allocation for the callback).
    pub fn feed<F>(&mut self, data: &[u8], mut cb: F)
    where
        F: FnMut(u8, u8, &[u8]),
    {
        self.buf.extend_from_slice(data);
        loop {
            // Find magic
            let i = match find_magic(&self.buf) {
                Some(i) => i,
                None => {
                    let keep = HDR_SIZE + CRC_SIZE;
                    if self.buf.len() > keep {
                        let drain = self.buf.len() - keep;
                        self.buf.drain(..drain);
                    }
                    break;
                }
            };
            if i > 0 {
                self.buf.drain(..i);
            }
            if self.buf.len() < HDR_SIZE {
                break;
            }
            let ftype = self.buf[2];
            let channel = self.buf[3];
            let length = (self.buf[4] as usize) | ((self.buf[5] as usize) << 8);
            if length > MAX_PAYLOAD {
                self.buf.drain(..2); // skip magic, rescan
                continue;
            }
            let total = HDR_SIZE + length + CRC_SIZE;
            if self.buf.len() < total {
                break;
            }
            let crc_rx = u32::from_le_bytes([
                self.buf[HDR_SIZE + length],
                self.buf[HDR_SIZE + length + 1],
                self.buf[HDR_SIZE + length + 2],
                self.buf[HDR_SIZE + length + 3],
            ]);
            let crc_calc = crc32fast::hash(&self.buf[HDR_SIZE..HDR_SIZE + length]);
            if crc_calc != crc_rx {
                self.buf.drain(..2);
                continue;
            }
            // CRC valid — call the user callback with a slice into our buffer.
            // We then drain the consumed bytes.
            // Copy out the payload slice *before* draining.
            let payload_end = HDR_SIZE + length;
            // SAFETY: we only hold &self.buf here, no aliasing.
            // We use a Vec copy to hand a stable slice to the callback.
            let payload = self.buf[HDR_SIZE..payload_end].to_vec();
            self.buf.drain(..total);
            cb(ftype, channel, &payload);
        }
    }
}

fn find_magic(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 {
        return None;
    }
    buf.windows(2).position(|w| w == MAGIC)
}

// ---------------------------------------------------------------------------
// TX queue for the CDC link
// ---------------------------------------------------------------------------

const COMPACT_THRESHOLD: usize = 65_536;

/// Outbound frame queue.  Frames are appended to a Vec<u8> and drained with
/// a single write().  The dead prefix is tracked with an offset and only
/// reclaimed when it grows past COMPACT_THRESHOLD to avoid O(n) copies on
/// every partial drain.
pub struct TxQueue {
    buf: Vec<u8>,
    offset: usize,
    pub queued_bytes: usize,
}

impl TxQueue {
    pub fn new() -> Self {
        TxQueue { buf: Vec::new(), offset: 0, queued_bytes: 0 }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.offset = 0;
        self.queued_bytes = 0;
    }

    pub fn enqueue(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        self.queued_bytes += data.len();
    }

    pub fn is_empty(&self) -> bool {
        self.offset >= self.buf.len()
    }

    /// Write as much as possible to `fd`.  Returns the number of bytes written.
    /// A return of 0 means EAGAIN / EWOULDBLOCK; the caller should wait for
    /// the fd to become writable again.
    pub fn drain_to_fd(&mut self, fd: libc::c_int) -> std::io::Result<usize> {
        let slice = &self.buf[self.offset..];
        if slice.is_empty() {
            return Ok(0);
        }
        let n = unsafe { libc::write(fd, slice.as_ptr() as *const _, slice.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error().map_or(false, |c| c == libc::EAGAIN || c == libc::EWOULDBLOCK) {
                return Ok(0);
            }
            return Err(e);
        }
        let written = n as usize;
        self.offset += written;
        self.queued_bytes -= written;
        if self.offset >= COMPACT_THRESHOLD {
            self.buf.drain(..self.offset);
            self.offset = 0;
        }
        Ok(written)
    }
}

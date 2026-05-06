//! Klipper transport terminator for the exporter (smart-proxy mode).
//!
//! [`McuTransport`] implements the Klipper wire protocol from the **host** side
//! — it terminates the MCU's UART session, consuming the raw `[len][seq][payload…][crc_hi][crc_lo][0x7E]`
//! frames, delivering the decoded payload bytes to the caller, and sending
//! properly-framed commands back to the MCU.
//!
//! This is a self-contained re-implementation of the same protocol state
//! machine as `windlass::Transport` (which is `pub(crate)` and therefore
//! unavailable to us).  The logic is taken verbatim from the windlass source.
//!
//! [`fetch_dictionary`] performs the Klipper `identify`/`identify_response`
//! exchange to retrieve the MCU's compressed data dictionary.  After the
//! exchange completes the same [`McuTransport`] and [`McuPayloadReceiver`]
//! remain live and are ready for the relay phase.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    pin, select,
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::{spawn, JoinHandle},
    time::{sleep_until, timeout, Instant},
};
use tokio_util::sync::CancellationToken;

// ─────────────────────────────────────────────────────────────────────────────
// Wire-protocol constants (identical to the Klipper firmware + windlass)
// ─────────────────────────────────────────────────────────────────────────────

const MESSAGE_HEADER_SIZE: usize = 2;
const MESSAGE_TRAILER_SIZE: usize = 3;
const MESSAGE_LENGTH_MIN: usize = MESSAGE_HEADER_SIZE + MESSAGE_TRAILER_SIZE;
const MESSAGE_LENGTH_MAX: usize = 64;
const MESSAGE_POSITION_SEQ: usize = 1;
const MESSAGE_TRAILER_CRC: usize = 3;
const MESSAGE_VALUE_SYNC: u8 = 0x7E;
const MESSAGE_DEST: u8 = 0x10;
const MESSAGE_SEQ_MASK: u8 = 0x0F;

// ─────────────────────────────────────────────────────────────────────────────
// Public API types
// ─────────────────────────────────────────────────────────────────────────────

/// Decoded MCU payload receiver.
///
/// Each item received from this channel is one decoded Klipper message payload
/// (the variable-length bytes between the two-byte header and the three-byte
/// trailer).  Wire framing (CRC, sequence number, sync byte) has already been
/// validated and stripped.
pub type McuPayloadReceiver = UnboundedReceiver<Vec<u8>>;

/// Klipper transport handle for communicating with an MCU UART.
///
/// Constructed with [`McuTransport::connect`].  Drives the Klipper wire
/// protocol (CRC-16, ACK/NAK, retransmit) in background tokio tasks.
pub struct McuTransport {
    _task_rdr: JoinHandle<()>,
    _task_wr: JoinHandle<()>,
    _task_inner: JoinHandle<()>,
    outbox_tx: UnboundedSender<McuCommand>,
}

#[derive(Debug)]
enum McuCommand {
    SendMessage(Vec<u8>),
    Exit,
}

impl McuTransport {
    /// Connect to the UART stream and start the Klipper transport state machine.
    ///
    /// Returns `(transport, payload_rx)`.  `payload_rx` yields decoded MCU
    /// payloads; `transport.send(payload)` queues a command payload for
    /// transmission to the MCU.
    pub async fn connect<S>(stream: S) -> (Self, McuPayloadReceiver)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (rdr, wr) = tokio::io::split(stream);

        let (raw_recv_tx, raw_recv_rx) = unbounded_channel::<Frame>();
        let (raw_send_tx, raw_send_rx) = unbounded_channel::<Arc<Vec<u8>>>();
        let (app_inbox_tx, app_inbox_rx) = unbounded_channel::<Vec<u8>>();
        let (app_outbox_tx, app_outbox_rx) = unbounded_channel::<McuCommand>();

        let cancel = CancellationToken::new();

        let c1 = cancel.clone();
        let task_rdr = spawn(async move {
            let _ = LowlevelReader::run(raw_recv_tx, rdr, c1).await;
        });

        let c2 = cancel.clone();
        let task_wr = spawn(async move {
            let _ = LowlevelWriter::run(raw_send_rx, wr, c2).await;
        });

        let task_inner = spawn(async move {
            let mut ts = TransportState::new(
                raw_recv_rx,
                raw_send_tx,
                app_inbox_tx,
                app_outbox_rx,
                cancel,
            );
            ts.protocol_handler().await;
        });

        (
            McuTransport {
                _task_rdr: task_rdr,
                _task_wr: task_wr,
                _task_inner: task_inner,
                outbox_tx: app_outbox_tx,
            },
            app_inbox_rx,
        )
    }

    /// Queue a command payload for transmission to the MCU.
    ///
    /// `payload` is the raw Klipper message bytes (VLQ command ID + arguments,
    /// no framing header/trailer).  The transport adds framing, CRC, and
    /// handles retransmission automatically.
    pub fn send(&self, payload: Vec<u8>) {
        let _ = self.outbox_tx.send(McuCommand::SendMessage(payload));
    }

    /// Signal the transport to shut down its background tasks.
    pub fn close(self) {
        let _ = self.outbox_tx.send(McuCommand::Exit);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Dictionary fetch
// ─────────────────────────────────────────────────────────────────────────────

/// Fetch the MCU's compressed data dictionary via the Klipper `identify`
/// exchange.
///
/// Sends `identify` requests (cmd=1, increasing offsets) via `transport` and
/// collects `identify_response` payloads (cmd=0) from `payload_rx` until the
/// full dictionary is assembled.  Other (non-identify) payloads that arrive
/// during the exchange are silently discarded.
///
/// Returns the raw compressed dictionary bytes.  After this function returns
/// `transport` and `payload_rx` are still live and can be used for the relay
/// phase.
///
/// Times out individual identify round-trips after 5 seconds.
pub async fn fetch_dictionary(
    transport: &McuTransport,
    payload_rx: &mut McuPayloadReceiver,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    const CHUNK_SIZE: u32 = 40;
    const TIMEOUT: Duration = Duration::from_secs(5);

    let mut dict: Vec<u8> = Vec::new();
    let mut offset: u32 = 0;

    loop {
        // Send: identify(offset, count=CHUNK_SIZE)
        let mut req = Vec::new();
        encode_vlq(&mut req, 1);           // cmd = 1 (identify)
        encode_vlq(&mut req, offset);      // offset
        encode_vlq(&mut req, CHUNK_SIZE);  // count
        transport.send(req);

        // Wait for the matching identify_response (cmd = 0).
        let chunk = 'wait: loop {
            let payload = timeout(TIMEOUT, payload_rx.recv())
                .await
                .map_err(|_| "timeout waiting for identify_response")?
                .ok_or("transport closed")?;

            let mut data: &[u8] = &payload;
            let cmd = match parse_vlq(&mut data) {
                Some(v) => v,
                None => continue 'wait,
            };
            if cmd != 0 {
                continue 'wait; // not identify_response
            }
            let resp_offset = parse_vlq(&mut data).unwrap_or(u32::MAX);
            let data_len = parse_vlq(&mut data).unwrap_or(0) as usize;
            if data.len() < data_len || resp_offset != offset {
                continue 'wait;
            }
            break 'wait data[..data_len].to_vec();
        };

        let chunk_len = chunk.len() as u32;
        dict.extend_from_slice(&chunk);
        offset += chunk_len;

        if chunk_len < CHUNK_SIZE {
            // Last chunk — dictionary is complete.
            return Ok(dict);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VLQ helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Encode `v` as a signed VLQ integer and append it to `buf`.
pub(crate) fn encode_vlq(buf: &mut Vec<u8>, v: u32) {
    let sv = v as i32;
    if !((-(1 << 26))..(3 << 26)).contains(&sv) {
        buf.push(((sv >> 28) & 0x7F) as u8 | 0x80);
    }
    if !((-(1 << 19))..(3 << 19)).contains(&sv) {
        buf.push(((sv >> 21) & 0x7F) as u8 | 0x80);
    }
    if !((-(1 << 12))..(3 << 12)).contains(&sv) {
        buf.push(((sv >> 14) & 0x7F) as u8 | 0x80);
    }
    if !((-(1 << 5))..(3 << 5)).contains(&sv) {
        buf.push(((sv >> 7) & 0x7F) as u8 | 0x80);
    }
    buf.push((sv & 0x7F) as u8);
}

/// Decode a signed VLQ integer from the front of `data`, advancing the slice.
///
/// Returns `None` if `data` is empty or the encoding is malformed.
pub(crate) fn parse_vlq(data: &mut &[u8]) -> Option<u32> {
    let first = *data.first()?;
    *data = &data[1..];
    let mut c = first as u32;
    let mut v = c & 0x7F;
    if (c & 0x60) == 0x60 {
        // sign-extend
        v |= (-0x20i32) as u32;
    }
    while c & 0x80 != 0 {
        let b = *data.first()?;
        *data = &data[1..];
        c = b as u32;
        v = (v << 7) | (c & 0x7F);
    }
    Some(v)
}

// ─────────────────────────────────────────────────────────────────────────────
// CRC-16 (same polynomial as Klipper firmware)
// ─────────────────────────────────────────────────────────────────────────────

fn crc16(buf: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for b in buf {
        let b = *b ^ ((crc & 0xFF) as u8);
        let b = b ^ (b << 4);
        let b16 = b as u16;
        // Operator precedence: ^ binds tighter than | (same as Klipper C firmware).
        // Evaluates as: (b16 << 8) | ((crc >> 8) ^ (b16 >> 4) ^ (b16 << 3))
        crc = b16 << 8 | crc >> 8 ^ b16 >> 4 ^ b16 << 3;
    }
    crc
}

// ─────────────────────────────────────────────────────────────────────────────
// Low-level reader task
// ─────────────────────────────────────────────────────────────────────────────

struct LowlevelReader<R> {
    rdr: BufReader<R>,
    synced: bool,
}

impl<R: AsyncRead + Unpin> LowlevelReader<R> {
    async fn read_frame(&mut self) -> std::io::Result<Option<Frame>> {
        let mut buf = [0u8; MESSAGE_LENGTH_MAX];
        let next_byte = self.rdr.read_u8().await?;
        if next_byte == MESSAGE_VALUE_SYNC {
            if !self.synced {
                self.synced = true;
            }
            return Ok(None);
        }
        if !self.synced {
            return Ok(None);
        }

        let receive_time = Instant::now();
        let len = next_byte as usize;
        if !(MESSAGE_LENGTH_MIN..=MESSAGE_LENGTH_MAX).contains(&len) {
            self.synced = false;
            return Ok(None);
        }

        self.rdr.read_exact(&mut buf[1..len]).await?;
        buf[0] = len as u8;
        let buf = &buf[..len];
        let seq = buf[MESSAGE_POSITION_SEQ];

        if seq & !MESSAGE_SEQ_MASK != MESSAGE_DEST {
            self.synced = false;
            return Ok(None);
        }

        let actual_crc = crc16(&buf[0..len - MESSAGE_TRAILER_SIZE]);
        let frame_crc = (buf[len - MESSAGE_TRAILER_CRC] as u16) << 8
            | (buf[len - MESSAGE_TRAILER_CRC + 1] as u16);
        if frame_crc != actual_crc {
            self.synced = false;
            return Ok(None);
        }

        Ok(Some(Frame {
            receive_time,
            sequence: seq & MESSAGE_SEQ_MASK,
            payload: buf[MESSAGE_HEADER_SIZE..len - MESSAGE_TRAILER_SIZE].to_vec(),
        }))
    }

    async fn run(
        outbox: UnboundedSender<Frame>,
        rdr: R,
        cancel: CancellationToken,
    ) -> std::io::Result<()>
    where
        R: AsyncRead + Unpin,
    {
        let mut state = Self { rdr: BufReader::new(rdr), synced: false };
        loop {
            select! {
                result = state.read_frame() => {
                    match result {
                        Ok(None) => {}
                        Ok(Some(frame)) => {
                            if outbox.send(frame).is_err() {
                                break Ok(());
                            }
                        }
                        Err(_) if cancel.is_cancelled() => break Ok(()),
                        Err(e) => break Err(e),
                    }
                }
                _ = cancel.cancelled() => break Ok(()),
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Low-level writer task
// ─────────────────────────────────────────────────────────────────────────────

struct LowlevelWriter;

impl LowlevelWriter {
    async fn run<W>(
        mut inbox: UnboundedReceiver<Arc<Vec<u8>>>,
        mut wr: W,
        cancel: CancellationToken,
    ) -> std::io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        loop {
            select! {
                msg = inbox.recv() => {
                    match msg {
                        Some(msg) => {
                            wr.write_all(&msg).await?;
                            wr.flush().await?;
                        }
                        None => break,
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Frame encoding helper
// ─────────────────────────────────────────────────────────────────────────────

fn encode_frame(sequence: u64, payload: &[u8]) -> Option<Vec<u8>> {
    let len = MESSAGE_LENGTH_MIN + payload.len();
    if len > MESSAGE_LENGTH_MAX {
        return None;
    }
    let mut buf = Vec::with_capacity(len);
    buf.push(len as u8);
    buf.push(MESSAGE_DEST | ((sequence as u8) & MESSAGE_SEQ_MASK));
    buf.extend_from_slice(payload);
    let crc = crc16(&buf[0..len - MESSAGE_TRAILER_SIZE]);
    buf.push(((crc >> 8) & 0xFF) as u8);
    buf.push((crc & 0xFF) as u8);
    buf.push(MESSAGE_VALUE_SYNC);
    Some(buf)
}

// ─────────────────────────────────────────────────────────────────────────────
// Frame type
// ─────────────────────────────────────────────────────────────────────────────

struct Frame {
    receive_time: Instant,
    sequence: u8,
    payload: Vec<u8>,
}

#[allow(dead_code)]
struct InflightFrame {
    sent_at: Instant,
    sequence: u64,
    payload: Arc<Vec<u8>>,
    is_retransmit: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// RTT estimator
// ─────────────────────────────────────────────────────────────────────────────

const MIN_RTO: f32 = 0.025;
const MAX_RTO: f32 = 5.000;

struct RttState {
    srtt: f32,
    rttvar: f32,
    rto: f32,
}

impl RttState {
    fn new() -> Self {
        Self { srtt: 0.0, rttvar: 0.0, rto: MIN_RTO }
    }

    fn rto(&self) -> Duration {
        Duration::from_secs_f32(self.rto)
    }

    fn update(&mut self, rtt: Duration) {
        let r = rtt.as_secs_f32();
        if self.srtt == 0.0 {
            self.rttvar = r / 2.0;
            self.srtt = r * 10.0;
        } else {
            self.rttvar = (3.0 * self.rttvar + (self.srtt - r).abs()) / 4.0;
            self.srtt = (7.0 * self.srtt + r) / 8.0;
        }
        let rttvar4 = (self.rttvar * 4.0).max(0.001);
        self.rto = (self.srtt + rttvar4).clamp(MIN_RTO, MAX_RTO);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Transport state machine
// ─────────────────────────────────────────────────────────────────────────────

struct MessageQueue<T> {
    sender: UnboundedSender<T>,
    receiver: UnboundedReceiver<T>,
    peeked: Option<T>,
}

impl<T> MessageQueue<T> {
    fn new() -> Self {
        let (sender, receiver) = unbounded_channel();
        Self { sender, receiver, peeked: None }
    }

    async fn recv_peek(&mut self) -> Option<&T> {
        if self.peeked.is_some() {
            self.peeked.as_ref()
        } else {
            self.peeked = self.receiver.recv().await;
            self.peeked.as_ref()
        }
    }

    fn try_recv(&mut self) -> Option<T> {
        if let Some(v) = self.peeked.take() {
            Some(v)
        } else {
            self.receiver.try_recv().ok()
        }
    }

    fn try_peek(&mut self) -> Option<&T> {
        if self.peeked.is_some() {
            self.peeked.as_ref()
        } else {
            self.peeked = self.receiver.try_recv().ok();
            self.peeked.as_ref()
        }
    }

    fn send(&mut self, msg: T) {
        let _ = self.sender.send(msg);
    }
}

struct TransportState {
    link_inbox: UnboundedReceiver<Frame>,
    link_outbox: UnboundedSender<Arc<Vec<u8>>>,
    app_inbox: UnboundedSender<Vec<u8>>,
    app_outbox: UnboundedReceiver<McuCommand>,
    cancel: CancellationToken,

    is_synchronized: bool,
    rtt_state: RttState,
    receive_sequence: u64,
    send_sequence: u64,
    last_ack_sequence: u64,
    ignore_nak_seq: u64,
    retransmit_seq: u64,
    retransmit_now: bool,

    inflight_messages: VecDeque<InflightFrame>,
    ready_messages: MessageQueue<Vec<u8>>,
}

impl TransportState {
    fn new(
        link_inbox: UnboundedReceiver<Frame>,
        link_outbox: UnboundedSender<Arc<Vec<u8>>>,
        app_inbox: UnboundedSender<Vec<u8>>,
        app_outbox: UnboundedReceiver<McuCommand>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            link_inbox,
            link_outbox,
            app_inbox,
            app_outbox,
            cancel,
            is_synchronized: false,
            rtt_state: RttState::new(),
            receive_sequence: 1,
            send_sequence: 1,
            last_ack_sequence: 0,
            ignore_nak_seq: 0,
            retransmit_seq: 0,
            retransmit_now: false,
            inflight_messages: VecDeque::new(),
            ready_messages: MessageQueue::new(),
        }
    }

    fn update_receive_seq(&mut self, receive_time: Instant, sequence: u64) {
        let mut sent_seq = self.receive_sequence;
        loop {
            if let Some(msg) = self.inflight_messages.pop_front() {
                sent_seq += 1;
                if sequence == sent_seq {
                    if !msg.is_retransmit {
                        let elapsed = receive_time.saturating_duration_since(msg.sent_at);
                        self.rtt_state.update(elapsed);
                    }
                    break;
                }
            } else {
                self.send_sequence = sequence;
                break;
            }
        }
        self.receive_sequence = sequence;
        self.is_synchronized = true;
    }

    fn handle_frame(&mut self, frame: Frame) {
        let rseq = self.receive_sequence;
        let mut sequence = (rseq & !(MESSAGE_SEQ_MASK as u64)) | (frame.sequence as u64);
        if sequence < rseq {
            sequence += (MESSAGE_SEQ_MASK as u64) + 1;
        }
        if !self.is_synchronized || sequence != rseq {
            if sequence > self.send_sequence && self.is_synchronized {
                return; // ack for unsent message
            }
            self.update_receive_seq(frame.receive_time, sequence);
        }

        if frame.payload.is_empty() {
            if self.last_ack_sequence < sequence {
                self.last_ack_sequence = sequence;
            } else if sequence > self.ignore_nak_seq && !self.inflight_messages.is_empty() {
                self.retransmit_now = true;
            }
        } else {
            let _ = self.app_inbox.send(frame.payload);
        }
    }

    fn can_send(&self) -> bool {
        self.inflight_messages.len() < 12
    }

    fn send_new_frame(&mut self, mut initial: Vec<u8>) {
        let max_payload = MESSAGE_LENGTH_MAX - MESSAGE_LENGTH_MIN;
        while let Some(next) = self.ready_messages.try_peek() {
            if initial.len() + next.len() <= max_payload {
                let mut next = self.ready_messages.try_recv().unwrap();
                initial.append(&mut next);
            } else {
                break;
            }
        }
        if let Some(frame) = encode_frame(self.send_sequence, &initial) {
            let frame = Arc::new(frame);
            self.send_sequence += 1;
            self.inflight_messages.push_back(InflightFrame {
                sent_at: Instant::now(),
                sequence: self.send_sequence,
                payload: frame.clone(),
                is_retransmit: false,
            });
            let _ = self.link_outbox.send(frame);
        }
    }

    fn send_more_frames(&mut self) {
        while self.can_send() && self.ready_messages.try_peek().is_some() {
            let msg = self.ready_messages.try_recv().unwrap();
            self.send_new_frame(msg);
        }
    }

    fn retransmit_pending(&mut self) {
        let total_len: usize = self.inflight_messages.iter().map(|m| m.payload.len()).sum();
        let mut buf = Vec::with_capacity(1 + total_len);
        buf.push(MESSAGE_VALUE_SYNC);
        let now = Instant::now();
        for msg in self.inflight_messages.iter_mut() {
            buf.extend_from_slice(&msg.payload);
            msg.is_retransmit = true;
            msg.sent_at = now;
        }
        let _ = self.link_outbox.send(Arc::new(buf));

        if self.retransmit_now {
            self.ignore_nak_seq = self.receive_sequence;
            if self.receive_sequence < self.retransmit_seq {
                self.ignore_nak_seq = self.retransmit_seq;
            }
            self.retransmit_now = false;
        } else {
            self.rtt_state.rto = (self.rtt_state.rto * 2.0).clamp(MIN_RTO, MAX_RTO);
            self.ignore_nak_seq = self.send_sequence;
        }
        self.retransmit_seq = self.send_sequence;
    }

    async fn protocol_handler(&mut self) {
        loop {
            if self.retransmit_now {
                self.retransmit_pending();
            }

            let retransmit_deadline = self
                .inflight_messages
                .front()
                .map(|msg| msg.sent_at + self.rtt_state.rto());
            let retransmit_timeout: futures::future::OptionFuture<_> =
                retransmit_deadline.map(sleep_until).into();
            pin!(retransmit_timeout);

            select! {
                frame = self.link_inbox.recv() => {
                    match frame {
                        Some(f) => self.handle_frame(f),
                        None => break,
                    }
                }
                msg = self.app_outbox.recv() => {
                    match msg {
                        Some(McuCommand::SendMessage(m)) => self.ready_messages.send(m),
                        Some(McuCommand::Exit) => {
                            self.cancel.cancel();
                            break;
                        }
                        None => break,
                    }
                }
                _ = self.ready_messages.recv_peek(), if self.can_send() => {
                    self.send_more_frames();
                }
                _ = &mut retransmit_timeout, if retransmit_deadline.is_some() => {
                    self.retransmit_now = true;
                }
                _ = self.cancel.cancelled() => break,
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_vlq_single_byte_values() {
        let mut buf = Vec::new();
        encode_vlq(&mut buf, 0);
        assert_eq!(buf, &[0x00]);

        buf.clear();
        encode_vlq(&mut buf, 1);
        assert_eq!(buf, &[0x01]);

        buf.clear();
        encode_vlq(&mut buf, 40);
        assert_eq!(buf, &[0x28]);

        buf.clear();
        encode_vlq(&mut buf, 63);
        assert_eq!(buf, &[0x3F]);
    }

    #[test]
    fn encode_vlq_multi_byte_value() {
        // 128 requires two bytes in VLQ
        let mut buf = Vec::new();
        encode_vlq(&mut buf, 128);
        assert_eq!(buf.len(), 2);
        // round-trip
        let mut data: &[u8] = &buf;
        let decoded = parse_vlq(&mut data).unwrap();
        assert_eq!(decoded, 128);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_vlq_empty_returns_none() {
        let mut data: &[u8] = &[];
        assert!(parse_vlq(&mut data).is_none());
    }

    #[test]
    fn vlq_roundtrip_values() {
        for &v in &[0u32, 1, 40, 64, 127, 128, 256, 1000, 0x3FFF] {
            let mut buf = Vec::new();
            encode_vlq(&mut buf, v);
            let mut data: &[u8] = &buf;
            let got = parse_vlq(&mut data).unwrap();
            assert_eq!(got, v, "roundtrip failed for {}", v);
        }
    }

    #[test]
    fn crc16_known_value() {
        // CRC of [5, 0x10] using the Klipper CRC-16 formula
        // (same as anchor/windlass: ^ binds tighter than | giving
        // (b16 << 8) | ((crc >> 8) ^ (b16 >> 4) ^ (b16 << 3))).
        let crc = crc16(&[5, 0x10]);
        assert_eq!(crc, 0x9E83);
    }

    #[test]
    fn encode_frame_min_length() {
        // Empty payload → min frame length = 5
        let frame = encode_frame(0, &[]).unwrap();
        assert_eq!(frame.len(), MESSAGE_LENGTH_MIN);
        assert_eq!(frame[0], MESSAGE_LENGTH_MIN as u8);
        assert_eq!(*frame.last().unwrap(), MESSAGE_VALUE_SYNC);
    }

    #[test]
    fn encode_frame_max_payload() {
        let payload = vec![0xAA; MESSAGE_LENGTH_MAX - MESSAGE_LENGTH_MIN];
        let frame = encode_frame(0, &payload).unwrap();
        assert_eq!(frame.len(), MESSAGE_LENGTH_MAX);
        assert_eq!(*frame.last().unwrap(), MESSAGE_VALUE_SYNC);
    }

    #[test]
    fn encode_frame_oversized_returns_none() {
        let payload = vec![0; MESSAGE_LENGTH_MAX - MESSAGE_LENGTH_MIN + 1];
        assert!(encode_frame(0, &payload).is_none());
    }
}

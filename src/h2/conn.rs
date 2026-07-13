// SPDX-License-Identifier: Apache-2.0

/// Shared HTTP/2 connection machinery: the writer goroutine, per-stream
/// inbound buffering, the handler-visible body reader, and CONTINUATION
/// reassembly.  Used by both the server and client connection loops.
///
/// ## Concurrency model (per connection)
///
/// - One **reader** goroutine parses frames and dispatches.
/// - One **writer** goroutine owns the write half and the HPACK encoder and
///   drains a bounded `chan<WriteCmd>`; every other goroutine sends commands.
/// - N **stream** goroutines (server handlers) or waiting `round_trip`
///   callers (client) produce DATA through flow-control reservation.
///
/// The WriteCmd channel is **never closed** (go-lib channels panic on double
/// close and on send-after-close).  The writer exits on the `Shutdown`
/// sentinel, which the teardown path sends exactly once.
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use go_lib::chan::{chan, Receiver, Sender};
use go_lib::sync::Cond;

use crate::header::Header;
use crate::parse::transfer::TrailerRead;

use super::error::{ErrCode, H2Error};
use super::flow::{RecvWindow, WINDOW_UPDATE_THRESHOLD};
use super::frame::{self, flags};
use super::hpack::{Encoder, HeaderField};
use super::io::H2WriteHalf;
use super::settings::DEFAULT_WINDOW;

// ---------------------------------------------------------------------------
// WriteCmd + writer goroutine
// ---------------------------------------------------------------------------

/// A unit of work for the writer goroutine.
pub enum WriteCmd {
    /// Send HEADERS (+CONTINUATIONs).  `fields` must already contain the
    /// pseudo-headers at the front, with lowercase names.
    Headers {
        stream_id:  u32,
        fields:     Vec<HeaderField>,
        end_stream: bool,
    },
    /// Send DATA.  Flow-control window must already be reserved by the
    /// sender; the writer splits at the peer's max frame size.
    Data {
        stream_id:  u32,
        chunk:      Vec<u8>,
        end_stream: bool,
    },
    /// Send trailers: HEADERS with END_STREAM.
    Trailers { stream_id: u32, fields: Vec<HeaderField> },
    Settings { params: Vec<(u16, u32)> },
    SettingsAck,
    Ping { ack: bool, payload: [u8; 8] },
    /// `stream_id` 0 updates the connection window.
    WindowUpdate { stream_id: u32, increment: u32 },
    RstStream { stream_id: u32, code: ErrCode },
    GoAway { last_stream_id: u32, code: ErrCode, debug: Vec<u8> },
    /// Flush and exit the writer goroutine.
    Shutdown,
}

/// Close-race-safe send.  The channel is never closed, but `send` panics if
/// it ever were; the catch_unwind converts that into an error, matching the
/// established pattern in server.rs / client.rs.
pub fn send_cmd(tx: &Sender<WriteCmd>, cmd: WriteCmd) -> Result<(), H2Error> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tx.send(cmd)))
        .map_err(|_| H2Error::Closed)
}

/// Shared writer-side state handed to `spawn_writer`.
pub struct WriterHandle {
    pub tx: Sender<WriteCmd>,
    /// Set once the writer has exited (socket error or Shutdown processed).
    pub dead: Arc<AtomicBool>,
    /// Receives one message when the writer goroutine exits.
    pub done_rx: Receiver<()>,
}

/// Spawn the writer goroutine.  It owns `w` and the HPACK encoder.
///
/// On a socket write error it stops writing but keeps draining commands
/// (discarding them) until `Shutdown` arrives, so producers never park
/// forever on a full channel.
pub fn spawn_writer(mut w: H2WriteHalf, peer_max_frame: Arc<AtomicU32>) -> WriterHandle {
    let (tx, rx) = chan::<WriteCmd>(32);
    let (done_tx, done_rx) = chan::<()>(1);
    let dead = Arc::new(AtomicBool::new(false));
    let dead2 = Arc::clone(&dead);

    // TLS_IO_STACK: on TLS connections this goroutine encrypts every outgoing
    // record; give it headroom so record processing never grows the stack.
    go_lib::spawn_with_stack(crate::tls::TLS_IO_STACK, move || {
        let mut enc = Encoder::new();
        let mut buf = Vec::with_capacity(16 * 1024);
        let mut broken = false;

        while let Some(cmd) = rx.recv() {
            if matches!(cmd, WriteCmd::Shutdown) {
                break;
            }
            if broken {
                continue; // drain-and-discard until Shutdown
            }
            buf.clear();
            let max_frame = peer_max_frame.load(Ordering::Relaxed) as usize;
            encode_cmd(&mut enc, &mut buf, cmd, max_frame);

            // Batch any immediately available commands into the same write.
            while buf.len() < 64 * 1024 {
                match rx.try_recv() {
                    Some(Some(WriteCmd::Shutdown)) => {
                        let _ = w.write_all(&buf);
                        let _ = w.flush();
                        dead2.store(true, Ordering::Release);
                        let _ = done_tx.try_send(());
                        return;
                    }
                    Some(Some(next)) => encode_cmd(&mut enc, &mut buf, next, max_frame),
                    _ => break,
                }
            }

            if w.write_all(&buf).is_err() || w.flush().is_err() {
                broken = true;
                dead2.store(true, Ordering::Release);
            }
        }
        dead2.store(true, Ordering::Release);
        let _ = done_tx.try_send(());
    });

    WriterHandle { tx, dead, done_rx }
}

/// Serialize one command into `buf`, splitting DATA / header blocks at
/// `max_frame`.
fn encode_cmd(enc: &mut Encoder, buf: &mut Vec<u8>, cmd: WriteCmd, max_frame: usize) {
    match cmd {
        WriteCmd::Headers { stream_id, fields, end_stream } => {
            let mut block = Vec::new();
            enc.encode(&fields, &mut block);
            write_header_block(
                buf,
                stream_id,
                &block,
                if end_stream { flags::END_STREAM } else { 0 },
                max_frame,
            );
        }
        WriteCmd::Trailers { stream_id, fields } => {
            let mut block = Vec::new();
            enc.encode(&fields, &mut block);
            write_header_block(buf, stream_id, &block, flags::END_STREAM, max_frame);
        }
        WriteCmd::Data { stream_id, chunk, end_stream } => {
            if chunk.is_empty() {
                let fl = if end_stream { flags::END_STREAM } else { 0 };
                frame::write_frame(buf, frame::TYPE_DATA, fl, stream_id, &[]);
                return;
            }
            let mut off = 0;
            while off < chunk.len() {
                let n = (chunk.len() - off).min(max_frame);
                let last = off + n == chunk.len();
                let fl = if last && end_stream { flags::END_STREAM } else { 0 };
                frame::write_frame(buf, frame::TYPE_DATA, fl, stream_id, &chunk[off..off + n]);
                off += n;
            }
        }
        WriteCmd::Settings { params } => {
            let mut payload = Vec::with_capacity(params.len() * 6);
            for (id, value) in params {
                payload.extend_from_slice(&id.to_be_bytes());
                payload.extend_from_slice(&value.to_be_bytes());
            }
            frame::write_frame(buf, frame::TYPE_SETTINGS, 0, 0, &payload);
        }
        WriteCmd::SettingsAck => {
            frame::write_frame(buf, frame::TYPE_SETTINGS, flags::ACK, 0, &[]);
        }
        WriteCmd::Ping { ack, payload } => {
            let fl = if ack { flags::ACK } else { 0 };
            frame::write_frame(buf, frame::TYPE_PING, fl, 0, &payload);
        }
        WriteCmd::WindowUpdate { stream_id, increment } => {
            frame::write_frame(
                buf,
                frame::TYPE_WINDOW_UPDATE,
                0,
                stream_id,
                &(increment & 0x7fff_ffff).to_be_bytes(),
            );
        }
        WriteCmd::RstStream { stream_id, code } => {
            frame::write_frame(
                buf,
                frame::TYPE_RST_STREAM,
                0,
                stream_id,
                &(code as u32).to_be_bytes(),
            );
        }
        WriteCmd::GoAway { last_stream_id, code, debug } => {
            let mut payload = Vec::with_capacity(8 + debug.len());
            payload.extend_from_slice(&last_stream_id.to_be_bytes());
            payload.extend_from_slice(&(code as u32).to_be_bytes());
            payload.extend_from_slice(&debug);
            frame::write_frame(buf, frame::TYPE_GOAWAY, 0, 0, &payload);
        }
        WriteCmd::Shutdown => unreachable!("Shutdown handled by the writer loop"),
    }
}

/// Split a header block into HEADERS + CONTINUATION frames.
fn write_header_block(
    buf: &mut Vec<u8>,
    stream_id: u32,
    block: &[u8],
    extra_flags: u8,
    max_frame: usize,
) {
    if block.len() <= max_frame {
        frame::write_frame(
            buf,
            frame::TYPE_HEADERS,
            flags::END_HEADERS | extra_flags,
            stream_id,
            block,
        );
        return;
    }
    frame::write_frame(buf, frame::TYPE_HEADERS, extra_flags, stream_id, &block[..max_frame]);
    let mut off = max_frame;
    while off < block.len() {
        let n = (block.len() - off).min(max_frame);
        let last = off + n == block.len();
        let fl = if last { flags::END_HEADERS } else { 0 };
        frame::write_frame(buf, frame::TYPE_CONTINUATION, fl, stream_id, &block[off..off + n]);
        off += n;
    }
}

// ---------------------------------------------------------------------------
// GoMutex — a goroutine-aware mutex
// ---------------------------------------------------------------------------

/// A mutex whose waiters park as goroutines (via `Cond`) instead of blocking
/// OS threads, and which therefore MAY be held across parking operations
/// like a channel send.
///
/// The client connection uses this to make "allocate stream id + enqueue
/// HEADERS" atomic: stream ids must reach the wire in increasing order
/// (RFC 9113 §5.1.1), and the enqueue can park when the writer channel is
/// full — holding a `std::sync::Mutex` there could stall every OS thread
/// and deadlock the scheduler.
pub struct GoMutex {
    locked: Mutex<bool>,
    cond:   Cond,
}

impl GoMutex {
    pub fn new() -> GoMutex {
        GoMutex { locked: Mutex::new(false), cond: Cond::new() }
    }

    pub fn lock(&self) {
        let mut g = self.locked.lock().unwrap();
        while *g {
            g = self.cond.wait(&self.locked, g);
        }
        *g = true;
    }

    pub fn unlock(&self) {
        *self.locked.lock().unwrap() = false;
        self.cond.notify_one();
    }
}

impl Default for GoMutex {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// StreamInbound — per-stream inbound data buffer
// ---------------------------------------------------------------------------

struct InboundState {
    buf: VecDeque<u8>,
    trailers: Option<Header>,
    end_stream: bool,
    /// Stream failed: RST received, connection died, or protocol error.
    reset: Option<ErrCode>,
    /// The body reader was dropped: discard (but window-credit) late DATA.
    discarded: bool,
    /// Our per-stream receive window remaining for the peer.
    recv_remaining: i64,
    /// Consumed bytes not yet returned via WINDOW_UPDATE.
    recv_pending: u32,
    /// Declared content-length remaining, if any, for §8.1.1 validation.
    content_remaining: Option<i64>,
}

/// Inbound side of one stream: DATA accumulates here (bounded by the receive
/// flow-control window — we only send WINDOW_UPDATE as bytes are consumed),
/// and the body reader parks on `cond` until data, END_STREAM, or reset.
pub struct StreamInbound {
    m:    Mutex<InboundState>,
    cond: Cond,
}

impl StreamInbound {
    pub fn new(content_length: Option<i64>) -> StreamInbound {
        StreamInbound {
            m: Mutex::new(InboundState {
                buf: VecDeque::new(),
                trailers: None,
                end_stream: false,
                reset: None,
                discarded: false,
                recv_remaining: DEFAULT_WINDOW as i64,
                recv_pending: 0,
                content_remaining: content_length,
            }),
            cond: Cond::new(),
        }
    }

    /// Reader-side: append a DATA frame's bytes.  `flow_len` is the
    /// flow-controlled length (incl. padding).  Returns `Ok(accepted)`;
    /// `accepted == false` means the body was discarded and the caller must
    /// credit the connection window itself.
    ///
    /// Never parks: the buffer is bounded by our advertised stream window.
    pub fn push_data(&self, data: &[u8], flow_len: u32, end: bool) -> Result<bool, H2Error> {
        let mut s = self.m.lock().unwrap();
        if s.end_stream {
            return Err(H2Error::Stream(0, ErrCode::StreamClosed));
        }
        s.recv_remaining -= flow_len as i64;
        if s.recv_remaining < 0 {
            return Err(H2Error::Stream(0, ErrCode::FlowControl));
        }
        if let Some(rem) = s.content_remaining.as_mut() {
            *rem -= data.len() as i64;
            if *rem < 0 {
                // More DATA than Content-Length declared: malformed (§8.1.1).
                return Err(H2Error::Stream(0, ErrCode::Protocol));
            }
            if end && *rem != 0 {
                return Err(H2Error::Stream(0, ErrCode::Protocol));
            }
        }
        if s.reset.is_some() || s.discarded {
            // Late data on an abandoned stream: swallow, caller credits.
            return Ok(false);
        }
        s.buf.extend(data);
        if end {
            s.end_stream = true;
        }
        drop(s);
        self.cond.notify_all();
        Ok(true)
    }

    /// Reader-side: the stream ended (END_STREAM on HEADERS/DATA already
    /// handled) with optional trailers from a trailing HEADERS frame.
    pub fn finish(&self, trailers: Option<Header>) -> Result<(), H2Error> {
        let mut s = self.m.lock().unwrap();
        if let Some(rem) = s.content_remaining
            && rem != 0
            && !s.end_stream
        {
            return Err(H2Error::Stream(0, ErrCode::Protocol));
        }
        if let Some(t) = trailers {
            s.trailers = Some(t);
        }
        s.end_stream = true;
        drop(s);
        self.cond.notify_all();
        Ok(())
    }

    /// Fail the stream (RST_STREAM received or connection torn down).
    pub fn fail(&self, code: ErrCode) {
        let mut s = self.m.lock().unwrap();
        if s.reset.is_none() {
            s.reset = Some(code);
        }
        s.buf.clear();
        drop(s);
        self.cond.notify_all();
    }

    /// True once END_STREAM has been observed.
    pub fn is_ended(&self) -> bool {
        self.m.lock().unwrap().end_stream
    }
}

// ---------------------------------------------------------------------------
// H2Body — the Read handed to handlers / stored in Response
// ---------------------------------------------------------------------------

/// A flow-controlled HTTP/2 message body.
///
/// Reading consumes buffered DATA and returns receive window to the peer in
/// batched WINDOW_UPDATEs.  Dropping the body before END_STREAM sends
/// RST_STREAM(CANCEL), matching Go's behavior when a handler ignores a body.
pub struct H2Body {
    inbound:   Arc<StreamInbound>,
    conn_recv: Arc<RecvWindow>,
    writer:    Sender<WriteCmd>,
    stream_id: u32,
}

impl H2Body {
    pub fn new(
        inbound:   Arc<StreamInbound>,
        conn_recv: Arc<RecvWindow>,
        writer:    Sender<WriteCmd>,
        stream_id: u32,
    ) -> H2Body {
        H2Body { inbound, conn_recv, writer, stream_id }
    }
}

impl Read for H2Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut s = self.inbound.m.lock().unwrap();
        loop {
            if let Some(code) = s.reset {
                return Err(io::Error::other(format!("stream reset: {code}")));
            }
            if !s.buf.is_empty() {
                break;
            }
            if s.end_stream {
                return Ok(0);
            }
            s = self.inbound.cond.wait(&self.inbound.m, s);
        }

        let n = buf.len().min(s.buf.len());
        for (i, b) in s.buf.drain(..n).enumerate() {
            buf[i] = b;
        }

        // Stream-level window credit (batched).
        s.recv_pending += n as u32;
        let stream_update = if s.recv_pending >= WINDOW_UPDATE_THRESHOLD && !s.end_stream {
            let inc = s.recv_pending;
            s.recv_pending = 0;
            s.recv_remaining += inc as i64;
            Some(inc)
        } else {
            None
        };
        drop(s);

        // WINDOW_UPDATE sends happen strictly AFTER dropping the inbound
        // lock — send may park on a full writer channel.
        if let Some(inc) = stream_update {
            let _ = send_cmd(&self.writer, WriteCmd::WindowUpdate {
                stream_id: self.stream_id,
                increment: inc,
            });
        }
        if let Some(inc) = self.conn_recv.consumed(n as u32) {
            let _ = send_cmd(&self.writer, WriteCmd::WindowUpdate {
                stream_id: 0,
                increment: inc,
            });
        }
        Ok(n)
    }
}

impl TrailerRead for H2Body {
    fn trailers(&self) -> Header {
        self.inbound
            .m
            .lock()
            .unwrap()
            .trailers
            .clone()
            .unwrap_or_default()
    }
}

impl Drop for H2Body {
    fn drop(&mut self) {
        let mut s = self.inbound.m.lock().unwrap();
        let abandoned = !(s.end_stream && s.buf.is_empty()) && s.reset.is_none();
        s.discarded = true;
        let unread = s.buf.len() as u32;
        s.buf.clear();
        drop(s);

        // Credit the connection window for anything we buffered but never
        // read, so an abandoned body cannot stall the peer.
        if unread > 0
            && let Some(inc) = self.conn_recv.consumed(unread)
        {
            let _ = send_cmd(&self.writer, WriteCmd::WindowUpdate { stream_id: 0, increment: inc });
        }
        if abandoned {
            let _ = send_cmd(&self.writer, WriteCmd::RstStream {
                stream_id: self.stream_id,
                code:      ErrCode::Cancel,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// HeaderBlockAssembler — HEADERS + CONTINUATION reassembly
// ---------------------------------------------------------------------------

/// Accumulates a header block across HEADERS/CONTINUATION frames.  While a
/// block is open, ANY other frame — a different stream, a different type —
/// is a connection error (RFC 9113 §6.10).
pub struct HeaderBlockAssembler {
    pub stream_id:  u32,
    pub end_stream: bool,
    fragments:      Vec<u8>,
    max_bytes:      usize,
}

impl HeaderBlockAssembler {
    pub fn new(stream_id: u32, end_stream: bool, max_bytes: usize) -> HeaderBlockAssembler {
        HeaderBlockAssembler { stream_id, end_stream, fragments: Vec::new(), max_bytes }
    }

    /// Append a fragment (from the initial HEADERS or a CONTINUATION).
    pub fn push(&mut self, stream_id: u32, fragment: &[u8]) -> Result<(), H2Error> {
        if stream_id != self.stream_id {
            return Err(H2Error::Connection(
                ErrCode::Protocol,
                "CONTINUATION for a different stream".into(),
            ));
        }
        if self.fragments.len() + fragment.len() > self.max_bytes {
            return Err(H2Error::Connection(
                ErrCode::EnhanceYourCalm,
                "header block too large".into(),
            ));
        }
        self.fragments.extend_from_slice(fragment);
        Ok(())
    }

    /// The complete block, once END_HEADERS has been seen.
    pub fn into_block(self) -> Vec<u8> {
        self.fragments
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h2::frame::{parse_frame, read_frame, Frame};
    use crate::h2::hpack::Decoder;
    use std::io::Cursor;

    /// Decode every frame in a byte buffer.
    fn frames_of(bytes: &[u8]) -> Vec<Frame> {
        let mut cursor = Cursor::new(bytes.to_vec());
        let mut out = Vec::new();
        while (cursor.position() as usize) < bytes.len() {
            let (h, payload) = read_frame(&mut cursor, 1 << 24).unwrap();
            out.push(parse_frame(h, payload).unwrap());
        }
        out
    }

    #[test]
    fn encode_cmd_data_splits_at_max_frame() {
        let mut enc = Encoder::new();
        let mut buf = Vec::new();
        encode_cmd(
            &mut enc,
            &mut buf,
            WriteCmd::Data { stream_id: 1, chunk: vec![0xaa; 40_000], end_stream: true },
            16_384,
        );
        let frames = frames_of(&buf);
        assert_eq!(frames.len(), 3);
        match &frames[0] {
            Frame::Data { data, end_stream, .. } => {
                assert_eq!(data.len(), 16_384);
                assert!(!end_stream);
            }
            other => panic!("wrong frame: {other:?}"),
        }
        match &frames[2] {
            Frame::Data { data, end_stream, .. } => {
                assert_eq!(data.len(), 40_000 - 2 * 16_384);
                assert!(end_stream, "END_STREAM only on the last chunk");
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn encode_cmd_headers_roundtrip() {
        let mut enc = Encoder::new();
        let mut buf = Vec::new();
        let fields = vec![
            HeaderField::new(":status", "200"),
            HeaderField::new("content-type", "text/plain"),
        ];
        encode_cmd(
            &mut enc,
            &mut buf,
            WriteCmd::Headers { stream_id: 5, fields: fields.clone(), end_stream: false },
            16_384,
        );
        let frames = frames_of(&buf);
        assert_eq!(frames.len(), 1);
        match &frames[0] {
            Frame::Headers { stream_id, fragment, end_headers, end_stream } => {
                assert_eq!(*stream_id, 5);
                assert!(end_headers);
                assert!(!end_stream);
                let decoded = Decoder::new(1 << 20).decode(fragment).unwrap();
                assert_eq!(decoded, fields);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn oversized_header_block_uses_continuation() {
        let mut enc = Encoder::new();
        let mut buf = Vec::new();
        // A header value far larger than a 128-byte max frame.
        let fields = vec![HeaderField::new("x-big", "v".repeat(500))];
        encode_cmd(
            &mut enc,
            &mut buf,
            WriteCmd::Headers { stream_id: 3, fields: fields.clone(), end_stream: true },
            128,
        );
        let frames = frames_of(&buf);
        assert!(frames.len() > 1, "expected CONTINUATION frames");
        let mut block = Vec::new();
        match &frames[0] {
            Frame::Headers { end_headers, end_stream, fragment, .. } => {
                assert!(!end_headers);
                assert!(end_stream, "END_STREAM travels on the HEADERS frame");
                block.extend_from_slice(fragment);
            }
            other => panic!("wrong frame: {other:?}"),
        }
        for (i, f) in frames[1..].iter().enumerate() {
            match f {
                Frame::Continuation { fragment, end_headers, .. } => {
                    block.extend_from_slice(fragment);
                    assert_eq!(*end_headers, i == frames.len() - 2);
                }
                other => panic!("wrong frame: {other:?}"),
            }
        }
        let decoded = Decoder::new(1 << 20).decode(&block).unwrap();
        assert_eq!(decoded, fields);
    }

    #[test]
    fn assembler_rejects_cross_stream_fragments() {
        let mut a = HeaderBlockAssembler::new(1, false, 1 << 20);
        a.push(1, b"ok").unwrap();
        assert!(matches!(
            a.push(3, b"other").unwrap_err(),
            H2Error::Connection(ErrCode::Protocol, _)
        ));
    }

    #[test]
    fn assembler_caps_total_size() {
        let mut a = HeaderBlockAssembler::new(1, false, 10);
        a.push(1, b"12345").unwrap();
        assert!(matches!(
            a.push(1, b"678901").unwrap_err(),
            H2Error::Connection(ErrCode::EnhanceYourCalm, _)
        ));
    }

    #[test]
    fn inbound_content_length_mismatch() {
        let s = StreamInbound::new(Some(5));
        assert!(s.push_data(b"abc", 3, false).is_ok());
        // Ends with only 3 of 5 promised bytes.
        assert!(matches!(
            s.push_data(b"", 0, true).unwrap_err(),
            H2Error::Stream(_, ErrCode::Protocol)
        ));

        let s2 = StreamInbound::new(Some(2));
        // More data than promised.
        assert!(matches!(
            s2.push_data(b"abc", 3, false).unwrap_err(),
            H2Error::Stream(_, ErrCode::Protocol)
        ));
    }

    #[test]
    fn inbound_stream_window_overrun() {
        let s = StreamInbound::new(None);
        // Default stream window is 65535; a 70000-byte flow length overruns.
        assert!(matches!(
            s.push_data(&[0u8; 1], 70_000, false).unwrap_err(),
            H2Error::Stream(_, ErrCode::FlowControl)
        ));
    }

    /// Full body pipeline across goroutines: reader pushes, body consumes,
    /// window updates flow to the writer channel.
    #[test]
    #[go_lib::main]
    fn h2body_reads_across_goroutines() {
        let inbound = Arc::new(StreamInbound::new(None));
        let conn_recv = Arc::new(RecvWindow::new());
        let (tx, _rx) = chan::<WriteCmd>(32);

        let producer = Arc::clone(&inbound);
        go_lib::go!(move || {
            go_lib::sleep(std::time::Duration::from_millis(10));
            producer.push_data(b"hello ", 6, false).unwrap();
            go_lib::sleep(std::time::Duration::from_millis(10));
            producer.push_data(b"world", 5, true).unwrap();
        });

        let mut body = H2Body::new(inbound, conn_recv, tx, 1);
        let mut out = String::new();
        body.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    #[go_lib::main]
    fn h2body_reset_surfaces_as_error() {
        let inbound = Arc::new(StreamInbound::new(None));
        let conn_recv = Arc::new(RecvWindow::new());
        let (tx, _rx) = chan::<WriteCmd>(32);

        let failer = Arc::clone(&inbound);
        go_lib::go!(move || {
            go_lib::sleep(std::time::Duration::from_millis(10));
            failer.fail(ErrCode::Cancel);
        });

        let mut body = H2Body::new(inbound, conn_recv, tx, 1);
        let mut out = Vec::new();
        let err = body.read_to_end(&mut out).unwrap_err();
        assert!(err.to_string().contains("CANCEL"), "{err}");
    }

    #[test]
    #[go_lib::main]
    fn h2body_drop_sends_cancel() {
        let inbound = Arc::new(StreamInbound::new(None));
        let conn_recv = Arc::new(RecvWindow::new());
        let (tx, rx) = chan::<WriteCmd>(32);

        inbound.push_data(b"unread", 6, false).unwrap();
        let body = H2Body::new(Arc::clone(&inbound), conn_recv, tx, 7);
        drop(body);

        match rx.try_recv() {
            Some(Some(WriteCmd::RstStream { stream_id: 7, code: ErrCode::Cancel })) => {}
            other => panic!("expected RST_STREAM(CANCEL), got {:?}",
                other.map(|o| o.map(|c| match c {
                    WriteCmd::RstStream { stream_id, code } => format!("rst {stream_id} {code}"),
                    _ => "other cmd".into(),
                }))),
        }
        // Late data after the drop is swallowed but reported as discarded.
        assert!(!inbound.push_data(b"late", 4, false).unwrap());
    }

    /// The writer goroutine serializes commands to the write half in order.
    #[test]
    #[go_lib::main]
    fn writer_goroutine_end_to_end() {
        // Loopback socket pair via a local listener.
        let listener = go_lib::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (conn_tx, conn_rx) = chan::<go_lib::net::TcpStream>(1);
        go_lib::go!(move || {
            let s = listener.accept().unwrap();
            conn_tx.send(s);
        });
        let client = go_lib::net::TcpStream::connect(addr).unwrap();
        let mut server_side = conn_rx.recv().unwrap();

        let (_r, w, _raw) = crate::h2::io::split_plain(client).unwrap();
        let handle = spawn_writer(w, Arc::new(AtomicU32::new(16_384)));

        send_cmd(&handle.tx, WriteCmd::Ping { ack: false, payload: [7; 8] }).unwrap();
        send_cmd(&handle.tx, WriteCmd::GoAway {
            last_stream_id: 3,
            code: ErrCode::NoError,
            debug: b"bye".to_vec(),
        }).unwrap();
        send_cmd(&handle.tx, WriteCmd::Shutdown).unwrap();
        assert!(handle.done_rx.recv().is_some(), "writer must signal completion");

        let mut bytes = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            match server_side.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => bytes.extend_from_slice(&tmp[..n]),
            }
            if bytes.len() >= 9 + 8 + 9 + 11 {
                break;
            }
        }
        let frames = frames_of(&bytes);
        assert_eq!(frames.len(), 2);
        assert!(matches!(frames[0], Frame::Ping { ack: false, payload } if payload == [7u8; 8]));
        match &frames[1] {
            Frame::GoAway { last_stream_id: 3, code: ErrCode::NoError, debug } => {
                assert_eq!(debug, b"bye");
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }
}

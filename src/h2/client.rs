// SPDX-License-Identifier: Apache-2.0

/// HTTP/2 client connection: one TCP/TLS connection multiplexing many
/// concurrent `round_trip` calls.
///
/// A `ClientConn` spawns a reader goroutine (frame dispatch) and a writer
/// goroutine (via `conn::spawn_writer`).  `round_trip` allocates an odd
/// stream id, sends HEADERS(+DATA), then parks on a one-shot channel until
/// the reader delivers the response head.  Response bodies stream through
/// the shared connection with flow control.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use go_lib::chan::{chan, Sender};
use go_lib::net::TcpStream;

use crate::error::HttpError;
use crate::header::Header;
use crate::parse::transfer::Body;
use crate::request::Request;
use crate::response::Response;

use super::conn::{
    send_cmd, spawn_writer, GoMutex, H2Body, HeaderBlockAssembler, StreamInbound, WriteCmd,
};
use super::error::{ErrCode, H2Error};
use super::flow::{RecvWindow, SendWindows};
use super::frame::{self, Frame};
use super::hpack::{DecodeErr, Decoder, HeaderField};
use super::io::{split_plain, split_tls, H2ReadHalf, H2WriteHalf, RawFdHandle};
use super::settings::Settings;
use super::PREFACE;

// ---------------------------------------------------------------------------
// ClientConn
// ---------------------------------------------------------------------------

/// The response head delivered to a waiting `round_trip`.
struct ResponseHead {
    status:     u16,
    header:     Header,
    end_stream: bool,
}

struct StreamState {
    inbound: Arc<StreamInbound>,
    /// Present until the final (non-1xx) response head has been delivered.
    resp_tx: Option<Sender<Result<ResponseHead, H2Error>>>,
}

struct ClientState {
    next_stream_id: u32,
    streams: HashMap<u32, StreamState>,
    /// Set when the peer sends GOAWAY: its last-processed stream id.
    goaway: Option<u32>,
    dead: bool,
}

/// A multiplexed HTTP/2 client connection.  Safe to share across goroutines.
pub struct ClientConn {
    writer:         Sender<WriteCmd>,
    send_windows:   Arc<SendWindows>,
    conn_recv:      Arc<RecvWindow>,
    peer_max_frame: Arc<AtomicU32>,
    state:          Arc<Mutex<ClientState>>,
    /// Serializes "allocate stream id → enqueue HEADERS", so ids reach the
    /// wire in increasing order (RFC 9113 §5.1.1).  Goroutine-aware: safe to
    /// hold across the writer-channel send.
    wmu:            GoMutex,
    /// Owned control dup of the socket: keeps the fd valid for `close()`
    /// even after the reader/writer goroutines have dropped their halves.
    ctl:            TcpStream,
}

impl ClientConn {
    /// Establish h2 over a plain TCP stream (prior knowledge).
    pub fn new_plain(stream: TcpStream) -> Result<Arc<ClientConn>, H2Error> {
        let (r, w, ctl) = split_plain(stream).map_err(H2Error::Io)?;
        Self::start(r, w, ctl)
    }

    /// Establish h2 over a completed TLS session (ALPN selected `h2`).
    pub fn new_tls(
        conn: rustls::ClientConnection,
        sock: TcpStream,
    ) -> Result<Arc<ClientConn>, H2Error> {
        let (r, w, ctl) = split_tls(rustls::Connection::Client(conn), sock).map_err(H2Error::Io)?;
        Self::start(r, w, ctl)
    }

    fn start(
        r:   H2ReadHalf,
        mut w: H2WriteHalf,
        ctl: TcpStream,
    ) -> Result<Arc<ClientConn>, H2Error> {
        // Client preface goes out before any frame.
        w.write_all(PREFACE).map_err(H2Error::Io)?;

        let peer_max_frame = Arc::new(AtomicU32::new(super::settings::DEFAULT_MAX_FRAME));
        let writer_handle = spawn_writer(w, Arc::clone(&peer_max_frame));

        let ours = Settings::default_ours(crate::parse::request::DEFAULT_MAX_HEADER_BYTES as u32, false);
        send_cmd(&writer_handle.tx, WriteCmd::Settings { params: ours.serialize() })?;

        let cc = Arc::new(ClientConn {
            writer:         writer_handle.tx.clone(),
            send_windows:   Arc::new(SendWindows::new(super::settings::DEFAULT_WINDOW)),
            conn_recv:      Arc::new(RecvWindow::new()),
            peer_max_frame: Arc::clone(&peer_max_frame),
            state: Arc::new(Mutex::new(ClientState {
                next_stream_id: 1,
                streams: HashMap::new(),
                goaway: None,
                dead: false,
            })),
            wmu: GoMutex::new(),
            ctl,
        });

        // Reader goroutine.  TLS_IO_STACK: on TLS connections this goroutine
        // decrypts every incoming record (and handles post-handshake
        // messages such as session tickets).
        let cc2 = Arc::clone(&cc);
        let done_rx = writer_handle.done_rx;
        go_lib::spawn_with_stack(crate::tls::TLS_IO_STACK, move || {
            let error = cc2.read_loop(r, ours);
            cc2.teardown(error);
            let _ = send_cmd(&cc2.writer, WriteCmd::Shutdown);
            let _ = done_rx.recv();
        });

        Ok(cc)
    }

    /// True if new round-trips may use this connection.
    pub fn is_reusable(&self) -> bool {
        let s = self.state.lock().unwrap();
        !s.dead && s.goaway.is_none()
    }

    /// Actively close the connection (pool eviction).  In-flight streams fail.
    pub fn close(&self) {
        // `ctl` is an owned dup, so the fd is valid for as long as `self`
        // lives — the shutdown can never hit a recycled fd number.
        RawFdHandle::of(&self.ctl).shutdown_both();
    }

    // ── round_trip ───────────────────────────────────────────────────────────

    /// Execute one request over this connection.  Callable concurrently.
    pub fn round_trip(&self, mut req: Request) -> Result<Response, HttpError> {
        let fields = request_fields(&req);
        let has_body = req.body.is_some();
        let (resp_tx, resp_rx) = chan::<Result<ResponseHead, H2Error>>(1);
        let inbound = Arc::new(StreamInbound::new(None));

        // Allocate a stream id and enqueue its HEADERS atomically: another
        // round_trip must not enqueue a higher id first.
        self.wmu.lock();
        let stream_id = {
            let mut s = self.state.lock().unwrap();
            if s.dead || s.goaway.is_some() {
                self.wmu.unlock();
                return Err(HttpError::Http2(H2Error::Closed));
            }
            let id = s.next_stream_id;
            s.next_stream_id += 2;
            s.streams.insert(id, StreamState {
                inbound: Arc::clone(&inbound),
                resp_tx: Some(resp_tx),
            });
            id
        };
        self.send_windows.open_stream(stream_id);
        let sent = send_cmd(&self.writer, WriteCmd::Headers {
            stream_id,
            fields,
            end_stream: !has_body,
        });
        self.wmu.unlock();
        sent.map_err(HttpError::Http2)?;

        // Stream the request body under flow control.
        if let Some(mut body) = req.body.take() {
            let mut chunk = vec![0u8; 16 * 1024];
            loop {
                let n = body.read(&mut chunk).map_err(|_| HttpError::BodyRead)?;
                if n == 0 {
                    break;
                }
                let mut off = 0;
                while off < n {
                    let granted = self
                        .send_windows
                        .reserve(stream_id, n - off)
                        .map_err(HttpError::Http2)?;
                    send_cmd(&self.writer, WriteCmd::Data {
                        stream_id,
                        chunk:      chunk[off..off + granted].to_vec(),
                        end_stream: false,
                    })
                    .map_err(HttpError::Http2)?;
                    off += granted;
                }
            }
            send_cmd(&self.writer, WriteCmd::Data {
                stream_id,
                chunk:      Vec::new(),
                end_stream: true,
            })
            .map_err(HttpError::Http2)?;
        }

        // Park until the reader delivers the response head.
        let head = match resp_rx.recv() {
            Some(Ok(head)) => head,
            Some(Err(e))   => return Err(HttpError::Http2(e)),
            None           => return Err(HttpError::Http2(H2Error::Closed)),
        };

        let content_length = head
            .header
            .get("Content-Length")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(-1);

        let body = if head.end_stream {
            None
        } else {
            Some(Body::Reader(Box::new(H2Body::new(
                inbound,
                Arc::clone(&self.conn_recv),
                self.writer.clone(),
                stream_id,
            ))))
        };

        Ok(Response {
            status:            head.status,
            status_text:       crate::status::status_text(head.status).to_owned(),
            proto:             "HTTP/2.0".to_owned(),
            proto_major:       2,
            proto_minor:       0,
            header:            head.header,
            body,
            content_length,
            transfer_encoding: Vec::new(),
            trailer:           Header::new(),
        })
    }

    // ── Reader loop ──────────────────────────────────────────────────────────

    fn read_loop(&self, mut r: H2ReadHalf, ours: Settings) -> Option<H2Error> {
        let mut decoder = Decoder::new(crate::parse::request::DEFAULT_MAX_HEADER_BYTES as u64);
        let mut peer = Settings::default();
        let mut assembler: Option<HeaderBlockAssembler> = None;
        let mut first_frame = true;

        loop {
            let (header, payload) = match frame::read_frame(&mut r, ours.max_frame_size) {
                Ok(fp) => fp,
                Err(H2Error::Io(_)) => return None,
                Err(e) => return Some(e),
            };
            let f = match frame::parse_frame(header, payload) {
                Ok(f) => f,
                Err(H2Error::Stream(id, code)) => {
                    let _ = send_cmd(&self.writer, WriteCmd::RstStream { stream_id: id, code });
                    continue;
                }
                Err(e) => return Some(e),
            };

            if first_frame && !matches!(f, Frame::Settings { ack: false, .. }) {
                return Some(H2Error::Connection(
                    ErrCode::Protocol,
                    "first frame from server was not SETTINGS".into(),
                ));
            }
            first_frame = false;

            if assembler.is_some() && !matches!(f, Frame::Continuation { .. }) {
                return Some(H2Error::Connection(
                    ErrCode::Protocol,
                    "expected CONTINUATION".into(),
                ));
            }

            match f {
                Frame::Headers { stream_id, fragment, end_stream, end_headers } => {
                    let mut a = HeaderBlockAssembler::new(
                        stream_id,
                        end_stream,
                        crate::parse::request::DEFAULT_MAX_HEADER_BYTES,
                    );
                    if let Err(e) = a.push(stream_id, &fragment) {
                        return Some(e);
                    }
                    if end_headers {
                        if let Err(e) = self.dispatch_headers(a, &mut decoder) {
                            match e {
                                H2Error::Stream(id, code) => {
                                    self.fail_stream(id, code);
                                    let _ = send_cmd(&self.writer,
                                        WriteCmd::RstStream { stream_id: id, code });
                                }
                                other => return Some(other),
                            }
                        }
                    } else {
                        assembler = Some(a);
                    }
                }
                Frame::Continuation { stream_id, fragment, end_headers } => {
                    let Some(mut a) = assembler.take() else {
                        return Some(H2Error::Connection(
                            ErrCode::Protocol,
                            "CONTINUATION without open header block".into(),
                        ));
                    };
                    if let Err(e) = a.push(stream_id, &fragment) {
                        return Some(e);
                    }
                    if end_headers {
                        if let Err(e) = self.dispatch_headers(a, &mut decoder) {
                            match e {
                                H2Error::Stream(id, code) => {
                                    self.fail_stream(id, code);
                                    let _ = send_cmd(&self.writer,
                                        WriteCmd::RstStream { stream_id: id, code });
                                }
                                other => return Some(other),
                            }
                        }
                    } else {
                        assembler = Some(a);
                    }
                }
                Frame::Data { stream_id, data, end_stream, flow_len } => {
                    if let Err(e) = self.conn_recv.on_data(flow_len) {
                        return Some(e);
                    }
                    let inbound = {
                        let s = self.state.lock().unwrap();
                        s.streams.get(&stream_id).map(|st| Arc::clone(&st.inbound))
                    };
                    match inbound {
                        None => self.credit_conn_window(flow_len),
                        Some(inbound) => {
                            match inbound.push_data(&data, flow_len, end_stream) {
                                Ok(true) => {}
                                Ok(false) => self.credit_conn_window(flow_len),
                                Err(H2Error::Stream(_, code)) => {
                                    self.fail_stream(stream_id, code);
                                    let _ = send_cmd(&self.writer,
                                        WriteCmd::RstStream { stream_id, code });
                                    self.credit_conn_window(flow_len);
                                }
                                Err(e) => return Some(e),
                            }
                            if end_stream {
                                self.release_stream(stream_id);
                            }
                        }
                    }
                }
                Frame::Settings { ack, params } => {
                    if !ack {
                        let delta = match peer.apply(&params) {
                            Ok(d) => d,
                            Err(e) => return Some(e),
                        };
                        self.peer_max_frame.store(peer.max_frame_size, Ordering::Relaxed);
                        if delta != 0
                            && let Err(e) = self.send_windows.apply_initial_window_delta(delta)
                        {
                            return Some(e);
                        }
                        let _ = send_cmd(&self.writer, WriteCmd::SettingsAck);
                    }
                }
                Frame::Ping { ack, payload } => {
                    if !ack {
                        let _ = send_cmd(&self.writer, WriteCmd::Ping { ack: true, payload });
                    }
                }
                Frame::WindowUpdate { stream_id, increment } => {
                    let result = if stream_id == 0 {
                        self.send_windows.add_conn(increment)
                    } else {
                        self.send_windows.add_stream(stream_id, increment)
                    };
                    match result {
                        Ok(()) => {}
                        Err(H2Error::Stream(id, code)) => {
                            let _ = send_cmd(&self.writer,
                                WriteCmd::RstStream { stream_id: id, code });
                        }
                        Err(e) => return Some(e),
                    }
                }
                Frame::RstStream { stream_id, code } => {
                    self.fail_stream(stream_id, code);
                }
                Frame::GoAway { last_stream_id, code, debug } => {
                    let msg = String::from_utf8_lossy(&debug).into_owned();
                    self.handle_goaway(last_stream_id, code, msg);
                    if code != ErrCode::NoError {
                        return None; // conn is done; teardown fails the rest
                    }
                    // NO_ERROR: streams ≤ last_stream_id may still complete.
                }
                Frame::PushPromise { .. } => {
                    // We advertise ENABLE_PUSH=0; any push is a protocol error.
                    return Some(H2Error::Connection(
                        ErrCode::Protocol,
                        "server pushed with push disabled".into(),
                    ));
                }
                Frame::Priority { .. } | Frame::Unknown { .. } => {}
            }
        }
    }

    /// HEADERS complete: response head, interim response, or trailers.
    fn dispatch_headers(
        &self,
        assembler: HeaderBlockAssembler,
        decoder:   &mut Decoder,
    ) -> Result<(), H2Error> {
        let stream_id  = assembler.stream_id;
        let end_stream = assembler.end_stream;
        let block      = assembler.into_block();

        let fields = match decoder.decode(&block) {
            Ok(f) => f,
            Err(DecodeErr::Compression(msg)) => {
                return Err(H2Error::Connection(ErrCode::Compression, msg));
            }
            Err(DecodeErr::ListTooLarge) => {
                return Err(H2Error::Stream(stream_id, ErrCode::EnhanceYourCalm));
            }
        };

        let (inbound, has_pending) = {
            let s = self.state.lock().unwrap();
            match s.streams.get(&stream_id) {
                None => return Ok(()), // stream already gone; ignore
                Some(st) => (Arc::clone(&st.inbound), st.resp_tx.is_some()),
            }
        };

        if has_pending {
            // Response head (possibly interim).
            let mut status: Option<u16> = None;
            let mut header = Header::new();
            let mut pseudo_done = false;
            for f in &fields {
                if let Some(p) = f.name.strip_prefix(':') {
                    if pseudo_done || p != "status" || status.is_some() {
                        return Err(H2Error::Stream(stream_id, ErrCode::Protocol));
                    }
                    status = Some(
                        f.value
                            .parse()
                            .map_err(|_| H2Error::Stream(stream_id, ErrCode::Protocol))?,
                    );
                } else {
                    pseudo_done = true;
                    header.add(&f.name, f.value.as_str());
                }
            }
            let status = status.ok_or(H2Error::Stream(stream_id, ErrCode::Protocol))?;

            // Interim responses (1xx) are skipped; wait for the final head.
            if (100..200).contains(&status) {
                return Ok(());
            }

            let tx = {
                let mut s = self.state.lock().unwrap();
                s.streams.get_mut(&stream_id).and_then(|st| st.resp_tx.take())
            };
            if end_stream {
                let _ = inbound.finish(None);
                self.release_stream(stream_id);
            }
            if let Some(tx) = tx {
                let _ = tx.try_send(Ok(ResponseHead { status, header, end_stream }));
            }
            return Ok(());
        }

        // Trailers.
        if !end_stream || fields.iter().any(|f| f.name.starts_with(':')) {
            return Err(H2Error::Stream(stream_id, ErrCode::Protocol));
        }
        let mut trailers = Header::new();
        for f in &fields {
            trailers.add(&f.name, f.value.as_str());
        }
        let result = inbound.finish(Some(trailers));
        self.release_stream(stream_id);
        result.map_err(|e| match e {
            H2Error::Stream(_, code) => H2Error::Stream(stream_id, code),
            other => other,
        })
    }

    /// Remove a completed stream from the routing table.
    fn release_stream(&self, stream_id: u32) {
        self.state.lock().unwrap().streams.remove(&stream_id);
        self.send_windows.close_stream(stream_id);
    }

    /// Fail one stream: notify a waiting round_trip and/or the body reader.
    fn fail_stream(&self, stream_id: u32, code: ErrCode) {
        let st = self.state.lock().unwrap().streams.remove(&stream_id);
        if let Some(st) = st {
            st.inbound.fail(code);
            if let Some(tx) = st.resp_tx {
                let _ = tx.try_send(Err(H2Error::Stream(stream_id, code)));
            }
        }
        self.send_windows.fail_stream(stream_id);
    }

    /// Record a GOAWAY and fail streams the server will not process.
    fn handle_goaway(&self, last_stream_id: u32, code: ErrCode, msg: String) {
        let failed: Vec<(u32, StreamState)> = {
            let mut s = self.state.lock().unwrap();
            s.goaway = Some(last_stream_id);
            let doomed: Vec<u32> = s
                .streams
                .keys()
                .copied()
                .filter(|&id| id > last_stream_id || code != ErrCode::NoError)
                .collect();
            doomed
                .into_iter()
                .filter_map(|id| s.streams.remove(&id).map(|st| (id, st)))
                .collect()
        };
        for (id, st) in failed {
            st.inbound.fail(code);
            if let Some(tx) = st.resp_tx {
                let _ = tx.try_send(Err(H2Error::GoAway(last_stream_id, code, msg.clone())));
            }
            self.send_windows.fail_stream(id);
        }
    }

    fn credit_conn_window(&self, flow_len: u32) {
        if flow_len > 0
            && let Some(inc) = self.conn_recv.consumed(flow_len)
        {
            let _ = send_cmd(&self.writer, WriteCmd::WindowUpdate {
                stream_id: 0,
                increment: inc,
            });
        }
    }

    /// Connection is over: fail everything still in flight.
    fn teardown(&self, error: Option<H2Error>) {
        let code = match &error {
            Some(H2Error::Connection(code, _)) => *code,
            _ => ErrCode::Cancel,
        };
        let streams: Vec<StreamState> = {
            let mut s = self.state.lock().unwrap();
            s.dead = true;
            s.streams.drain().map(|(_, st)| st).collect()
        };
        for st in streams {
            st.inbound.fail(code);
            if let Some(tx) = st.resp_tx {
                let _ = tx.try_send(Err(H2Error::Closed));
            }
        }
        self.send_windows.fail_all();
        if let Some(H2Error::Connection(code, msg)) = error {
            let _ = send_cmd(&self.writer, WriteCmd::GoAway {
                last_stream_id: 0,
                code,
                debug: msg.into_bytes(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Request serialization
// ---------------------------------------------------------------------------

/// Convert a `Request` into an HTTP/2 header list: pseudo-headers first,
/// then lowercased regular headers minus connection-specific ones
/// (RFC 9113 §8.2.2).  The Host header folds into `:authority`.
fn request_fields(req: &Request) -> Vec<HeaderField> {
    // `Request::new` fills `host` with the URL's hostname only (no port), so
    // the URL is the authoritative source unless `host` was overridden to
    // something else (e.g. a virtual host).
    let url_host = req.url.host_str().unwrap_or("localhost");
    let authority = if !req.host.is_empty() && req.host != url_host {
        req.host.clone()
    } else {
        match req.url.port() {
            Some(p) => format!("{url_host}:{p}"),
            None    => url_host.to_owned(),
        }
    };
    let mut path = req.url.path().to_owned();
    if let Some(q) = req.url.query() {
        path.push('?');
        path.push_str(q);
    }

    let mut fields = vec![
        HeaderField::new(":method", req.method.as_str()),
        HeaderField::new(":scheme", req.url.scheme()),
        HeaderField::new(":authority", authority),
        HeaderField::new(":path", path),
    ];

    let mut has_content_length = false;
    for (name, values) in req.header.iter() {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            // Hop-by-hop / connection-specific headers never cross into h2.
            "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding"
            | "upgrade" | "host" => continue,
            "te" => {
                // Only "trailers" is legal.
                for v in values {
                    if v.eq_ignore_ascii_case("trailers") {
                        fields.push(HeaderField::new("te", "trailers"));
                    }
                }
                continue;
            }
            "content-length" => has_content_length = true,
            _ => {}
        }
        for v in values {
            fields.push(HeaderField::new(lower.clone(), v.as_str()));
        }
    }

    // Propagate a known body length.
    if !has_content_length && req.body.is_some() && req.content_length > 0 {
        fields.push(HeaderField::new("content-length", req.content_length.to_string()));
    }

    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_fields_pseudo_and_hop_by_hop() {
        let mut req = Request::new("GET", "http://example.com:8080/a/b?x=1", None).unwrap();
        req.header.set("Accept", "*/*");
        req.header.set("Connection", "keep-alive");
        req.header.set("TE", "trailers");
        req.header.set("Upgrade", "h2c");

        let fields = request_fields(&req);
        assert_eq!(fields[0], HeaderField::new(":method", "GET"));
        assert_eq!(fields[1], HeaderField::new(":scheme", "http"));
        assert_eq!(fields[2], HeaderField::new(":authority", "example.com:8080"));
        assert_eq!(fields[3], HeaderField::new(":path", "/a/b?x=1"));
        assert!(fields.iter().any(|f| f.name == "accept"));
        assert!(fields.iter().any(|f| f.name == "te" && f.value == "trailers"));
        assert!(fields.iter().all(|f| f.name != "connection"));
        assert!(fields.iter().all(|f| f.name != "upgrade"));
        assert!(fields.iter().all(|f| f.name != "host"));
    }

    #[test]
    fn request_fields_default_port_omitted() {
        let req = Request::new("GET", "https://example.com/", None).unwrap();
        let fields = request_fields(&req);
        // Request::new populates req.host from the URL.
        assert!(fields[2].value.starts_with("example.com"));
        assert_eq!(fields[3], HeaderField::new(":path", "/"));
    }
}

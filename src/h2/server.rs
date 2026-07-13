// SPDX-License-Identifier: Apache-2.0

/// HTTP/2 server connection handling.
///
/// `serve_h2c` (cleartext, preface already consumed by the sniffer in
/// `crate::server`) and `serve_h2_tls` (ALPN-negotiated) converge on
/// [`serve_h2`], which runs the frame reader loop in the connection's
/// goroutine and spawns one handler goroutine per stream.
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use go_lib::chan::{chan, Sender};
use go_lib::net::TcpStream;
use go_lib::sync::WaitGroup;
use url::Url;

use crate::error::HttpError;
use crate::handler::Handler;
use crate::header::Header;
use crate::parse::transfer::Body;
use crate::request::Request;
use crate::response::ResponseWriter;

use super::conn::{
    send_cmd, spawn_writer, H2Body, HeaderBlockAssembler, StreamInbound, WriteCmd,
};
use super::error::{ErrCode, H2Error};
use super::flow::{RecvWindow, SendWindows};
use super::frame::{self, Frame};
use super::hpack::{DecodeErr, Decoder, HeaderField};
use super::io::{split_plain, split_tls, H2ReadHalf, RawFdHandle};
use super::settings::{Settings, DEFAULT_MAX_CONCURRENT_STREAMS};
use super::PREFACE;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Server-side knobs threaded down from `crate::server::Server`.
pub struct H2ServerConfig {
    pub max_header_bytes: usize,
    pub max_body_bytes:   Option<u64>,
    pub idle_timeout:     Option<Duration>,
    /// Cancelled when `Server::shutdown()` runs; triggers a graceful GOAWAY.
    pub shutdown_ctx:     go_lib::context::Context,
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Serve h2c on a plain TCP connection.  The 24-byte client preface has
/// already been consumed by the preface sniffer.
pub fn serve_h2c(stream: TcpStream, handler: Arc<dyn Handler>, cfg: H2ServerConfig) {
    let remote_addr = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let Ok((r, w, raw)) = split_plain(stream) else { return };
    serve_h2(r, w, raw, false, remote_addr, handler, cfg);
}

/// Serve h2 over TLS.  The handshake must be complete and ALPN must have
/// selected `h2`.  Reads the client preface first.
pub fn serve_h2_tls(
    conn:    rustls::ServerConnection,
    sock:    TcpStream,
    handler: Arc<dyn Handler>,
    cfg:     H2ServerConfig,
) {
    let remote_addr = sock.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let Ok((r, w, raw)) = split_tls(rustls::Connection::Server(conn), sock) else { return };
    serve_h2(r, w, raw, true, remote_addr, handler, cfg);
}

// ---------------------------------------------------------------------------
// Shared per-connection state
// ---------------------------------------------------------------------------

struct StreamEntry {
    inbound: Arc<StreamInbound>,
    /// No handler goroutine owns this stream (rejected request); the reader
    /// removes it once END_STREAM arrives.
    orphan: bool,
}

struct StreamsState {
    map: HashMap<u32, StreamEntry>,
    max_seen: u32,
    /// Streams with a running handler goroutine.
    open: u32,
    goaway_sent: bool,
}

struct ServerShared {
    writer:         Sender<WriteCmd>,
    send_windows:   Arc<SendWindows>,
    conn_recv:      Arc<RecvWindow>,
    peer_max_frame: Arc<AtomicU32>,
    streams:        Mutex<StreamsState>,
    handlers:       WaitGroup,
    max_body_bytes: Option<u64>,
}

impl ServerShared {
    fn open_count(&self) -> u32 {
        self.streams.lock().unwrap().open
    }

    /// Send GOAWAY(NO_ERROR) once, advertising the highest stream we accepted.
    fn begin_goaway(&self) {
        let mut s = self.streams.lock().unwrap();
        if s.goaway_sent {
            return;
        }
        s.goaway_sent = true;
        let last = s.max_seen;
        drop(s);
        let _ = send_cmd(&self.writer, WriteCmd::GoAway {
            last_stream_id: last,
            code:           ErrCode::NoError,
            debug:          Vec::new(),
        });
    }

    /// Handler goroutine finished: release the stream.
    fn stream_done(&self, id: u32) {
        let mut s = self.streams.lock().unwrap();
        s.map.remove(&id);
        s.open = s.open.saturating_sub(1);
    }
}

// ---------------------------------------------------------------------------
// serve_h2 — the reader loop
// ---------------------------------------------------------------------------

fn serve_h2(
    mut r:        H2ReadHalf,
    w:            super::io::H2WriteHalf,
    raw:          RawFdHandle,
    expect_magic: bool,
    remote_addr:  String,
    handler:      Arc<dyn Handler>,
    cfg:          H2ServerConfig,
) {
    // Our settings never change after the initial frame.
    let ours = Settings::default_ours(cfg.max_header_bytes as u32, true);

    let peer_max_frame = Arc::new(AtomicU32::new(super::settings::DEFAULT_MAX_FRAME));
    let writer_handle = spawn_writer(w, Arc::clone(&peer_max_frame));

    let shared = Arc::new(ServerShared {
        writer:         writer_handle.tx.clone(),
        send_windows:   Arc::new(SendWindows::new(super::settings::DEFAULT_WINDOW)),
        conn_recv:      Arc::new(RecvWindow::new()),
        peer_max_frame: Arc::clone(&peer_max_frame),
        streams: Mutex::new(StreamsState {
            map: HashMap::new(),
            max_seen: 0,
            open: 0,
            goaway_sent: false,
        }),
        handlers:       WaitGroup::new(),
        max_body_bytes: cfg.max_body_bytes,
    });

    // Advertise our settings before anything else.
    let _ = send_cmd(&shared.writer, WriteCmd::Settings { params: ours.serialize() });

    // ── Client preface (TLS path reads it here; h2c sniffer already did) ────
    if expect_magic && !read_preface(&mut r) {
        let _ = send_cmd(&shared.writer, WriteCmd::Shutdown);
        let _ = writer_handle.done_rx.recv();
        return;
    }

    // ── Shutdown watchdog ────────────────────────────────────────────────────
    // Waits for Server::shutdown() (context cancel); sends GOAWAY, lets open
    // streams drain, then unblocks the parked reader via socket shutdown.
    let (conn_done_tx, conn_done_rx) = chan::<()>(1);
    {
        let shared = Arc::clone(&shared);
        let ctx = cfg.shutdown_ctx.clone();
        go_lib::go!(move || {
            go_lib::select! {
                recv(ctx.done()) -> _sig => {
                    shared.begin_goaway();
                    while shared.open_count() > 0 {
                        go_lib::sleep(Duration::from_millis(20));
                    }
                    raw.shutdown_read();
                }
                recv(conn_done_rx) -> _sig => {}
            }
        });
    }

    // ── Idle timeout ─────────────────────────────────────────────────────────
    // A connection is idle when it has no open streams.  Checked at the
    // configured interval; two consecutive idle observations with no
    // intervening stream activity close the connection.
    let last_activity = Arc::new(AtomicU32::new(0)); // generation counter
    if let Some(idle) = cfg.idle_timeout {
        let shared = Arc::clone(&shared);
        let last_activity = Arc::clone(&last_activity);
        go_lib::go!(move || {
            let mut seen = last_activity.load(Ordering::Relaxed);
            loop {
                go_lib::sleep(idle);
                let now = last_activity.load(Ordering::Relaxed);
                if shared.open_count() == 0 && now == seen {
                    shared.begin_goaway();
                    raw.shutdown_read();
                    return;
                }
                seen = now;
            }
        });
    }

    // ── Frame dispatch loop ──────────────────────────────────────────────────
    let error = read_loop(
        &mut r,
        &shared,
        &handler,
        &cfg,
        &remote_addr,
        &last_activity,
        ours,
    );

    // ── Teardown ─────────────────────────────────────────────────────────────
    let _ = conn_done_tx.try_send(());

    // Fail every stream so parked handler bodies/writers error out.
    {
        let s = shared.streams.lock().unwrap();
        for entry in s.map.values() {
            entry.inbound.fail(ErrCode::Cancel);
        }
    }
    shared.send_windows.fail_all();

    // Let running handlers finish (their sends drain into the live writer).
    shared.handlers.wait();

    // Final GOAWAY: a protocol error reports its code, otherwise NO_ERROR.
    let (last, already_sent) = {
        let s = shared.streams.lock().unwrap();
        (s.max_seen, s.goaway_sent)
    };
    match error {
        Some(H2Error::Connection(code, msg)) => {
            let _ = send_cmd(&shared.writer, WriteCmd::GoAway {
                last_stream_id: last,
                code,
                debug: msg.into_bytes(),
            });
        }
        _ if !already_sent => {
            let _ = send_cmd(&shared.writer, WriteCmd::GoAway {
                last_stream_id: last,
                code: ErrCode::NoError,
                debug: Vec::new(),
            });
        }
        _ => {}
    }
    let _ = send_cmd(&shared.writer, WriteCmd::Shutdown);
    let _ = writer_handle.done_rx.recv();
}

/// Read and verify the 24-byte client connection preface.
fn read_preface(r: &mut impl Read) -> bool {
    let mut buf = [0u8; 24];
    if r.read_exact(&mut buf).is_err() {
        return false;
    }
    buf == PREFACE
}

/// The frame dispatch loop.  Returns `Some(err)` on a connection error,
/// `None` on clean EOF / GOAWAY / shutdown.
fn read_loop(
    r:             &mut H2ReadHalf,
    shared:        &Arc<ServerShared>,
    handler:       &Arc<dyn Handler>,
    cfg:           &H2ServerConfig,
    remote_addr:   &str,
    last_activity: &Arc<AtomicU32>,
    ours:          Settings,
) -> Option<H2Error> {
    let mut decoder = Decoder::new(cfg.max_header_bytes as u64);
    let mut peer = Settings::default();
    let mut assembler: Option<HeaderBlockAssembler> = None;
    let mut first_frame = true;
    let mut goaway_received = false;

    loop {
        let (header, payload) = match frame::read_frame(r, ours.max_frame_size) {
            Ok(fp) => fp,
            Err(H2Error::Io(_)) => return None, // EOF / reset / shutdown_read
            Err(e) => return Some(e),
        };
        last_activity.fetch_add(1, Ordering::Relaxed);

        let f = match frame::parse_frame(header, payload) {
            Ok(f) => f,
            Err(H2Error::Stream(id, code)) => {
                let _ = send_cmd(&shared.writer, WriteCmd::RstStream { stream_id: id, code });
                continue;
            }
            Err(e) => return Some(e),
        };

        // The first frame from the client must be SETTINGS.
        if first_frame && !matches!(f, Frame::Settings { ack: false, .. }) {
            return Some(H2Error::Connection(
                ErrCode::Protocol,
                "first frame was not SETTINGS".into(),
            ));
        }
        first_frame = false;

        // While a header block is open, only its CONTINUATIONs are legal.
        if assembler.is_some() && !matches!(f, Frame::Continuation { .. }) {
            return Some(H2Error::Connection(
                ErrCode::Protocol,
                "expected CONTINUATION".into(),
            ));
        }

        match f {
            Frame::Headers { stream_id, fragment, end_stream, end_headers } => {
                // Flood cap: well above MAX_HEADER_LIST_SIZE so ordinary
                // oversized requests reach HPACK decoding and get a clean
                // 431 instead of a connection error.
                let mut a = HeaderBlockAssembler::new(
                    stream_id,
                    end_stream,
                    cfg.max_header_bytes.saturating_mul(4).max(64 * 1024),
                );
                if let Err(e) = a.push(stream_id, &fragment) {
                    return Some(e);
                }
                if end_headers {
                    if let Err(e) = dispatch_headers(
                        a, shared, handler, cfg, remote_addr, &mut decoder,
                    ) {
                        match e {
                            H2Error::Stream(id, code) => {
                                let _ = send_cmd(&shared.writer,
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
                    if let Err(e) = dispatch_headers(
                        a, shared, handler, cfg, remote_addr, &mut decoder,
                    ) {
                        match e {
                            H2Error::Stream(id, code) => {
                                let _ = send_cmd(&shared.writer,
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
                if let Err(e) = shared.conn_recv.on_data(flow_len) {
                    return Some(e);
                }
                let entry_info = {
                    let s = shared.streams.lock().unwrap();
                    match s.map.get(&stream_id) {
                        Some(e) => Some((Arc::clone(&e.inbound), e.orphan)),
                        None if stream_id <= s.max_seen => None, // closed stream
                        None => {
                            return Some(H2Error::Connection(
                                ErrCode::Protocol,
                                "DATA on an unopened stream".into(),
                            ));
                        }
                    }
                };
                match entry_info {
                    None => {
                        // Stream already closed: swallow, credit the window.
                        credit_conn_window(shared, flow_len);
                    }
                    Some((inbound, orphan)) => {
                        match inbound.push_data(&data, flow_len, end_stream) {
                            Ok(true) => {}
                            Ok(false) => credit_conn_window(shared, flow_len),
                            Err(H2Error::Stream(_, code)) => {
                                inbound.fail(code);
                                shared.send_windows.fail_stream(stream_id);
                                let _ = send_cmd(&shared.writer,
                                    WriteCmd::RstStream { stream_id, code });
                                credit_conn_window(shared, flow_len);
                            }
                            Err(e) => return Some(e),
                        }
                        if end_stream && orphan {
                            shared.streams.lock().unwrap().map.remove(&stream_id);
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
                    shared.peer_max_frame.store(peer.max_frame_size, Ordering::Relaxed);
                    if delta != 0
                        && let Err(e) = shared.send_windows.apply_initial_window_delta(delta)
                    {
                        return Some(e);
                    }
                    let _ = send_cmd(&shared.writer, WriteCmd::SettingsAck);
                }
            }
            Frame::Ping { ack, payload } => {
                if !ack {
                    let _ = send_cmd(&shared.writer, WriteCmd::Ping { ack: true, payload });
                }
            }
            Frame::WindowUpdate { stream_id, increment } => {
                let result = if stream_id == 0 {
                    shared.send_windows.add_conn(increment)
                } else {
                    shared.send_windows.add_stream(stream_id, increment)
                };
                match result {
                    Ok(())                          => {}
                    Err(H2Error::Stream(id, code)) => {
                        let _ = send_cmd(&shared.writer,
                            WriteCmd::RstStream { stream_id: id, code });
                    }
                    Err(e) => return Some(e),
                }
            }
            Frame::RstStream { stream_id, code } => {
                let entry = {
                    let mut s = shared.streams.lock().unwrap();
                    if let Some(e) = s.map.get(&stream_id) {
                        let inbound = Arc::clone(&e.inbound);
                        if e.orphan {
                            s.map.remove(&stream_id);
                        }
                        Some(inbound)
                    } else {
                        None
                    }
                };
                if let Some(inbound) = entry {
                    inbound.fail(code);
                }
                shared.send_windows.fail_stream(stream_id);
            }
            Frame::GoAway { .. } => {
                // Client is going away: no new streams.  Keep reading so open
                // streams can finish (their DATA / WINDOW_UPDATEs still flow);
                // once nothing is open, close down.
                goaway_received = true;
            }
            Frame::PushPromise { .. } => {
                return Some(H2Error::Connection(
                    ErrCode::Protocol,
                    "PUSH_PROMISE from a client".into(),
                ));
            }
            Frame::Priority { .. } | Frame::Unknown { .. } => {}
        }

        if goaway_received && shared.open_count() == 0 {
            return None;
        }
    }
}

/// Credit the connection receive window for bytes that were consumed by the
/// reader itself (discarded or closed-stream DATA).
fn credit_conn_window(shared: &ServerShared, flow_len: u32) {
    if flow_len > 0
        && let Some(inc) = shared.conn_recv.consumed(flow_len)
    {
        let _ = send_cmd(&shared.writer, WriteCmd::WindowUpdate { stream_id: 0, increment: inc });
    }
}

// ---------------------------------------------------------------------------
// HEADERS dispatch — trailers or a new request
// ---------------------------------------------------------------------------

fn dispatch_headers(
    assembler:   HeaderBlockAssembler,
    shared:      &Arc<ServerShared>,
    handler:     &Arc<dyn Handler>,
    cfg:         &H2ServerConfig,
    remote_addr: &str,
    decoder:     &mut Decoder,
) -> Result<(), H2Error> {
    let stream_id  = assembler.stream_id;
    let end_stream = assembler.end_stream;
    let block      = assembler.into_block();

    // Decode before anything else: HPACK state must stay synchronized even
    // for streams we reject.
    let fields = match decoder.decode(&block) {
        Ok(f) => f,
        Err(DecodeErr::Compression(msg)) => {
            return Err(H2Error::Connection(ErrCode::Compression, msg));
        }
        Err(DecodeErr::ListTooLarge) => {
            // Reject the request without killing the connection.
            respond_status(shared, stream_id, 431);
            register_orphan(shared, stream_id, end_stream);
            return Ok(());
        }
    };

    // Trailers for an open stream?
    let existing = {
        let s = shared.streams.lock().unwrap();
        s.map.get(&stream_id).map(|e| Arc::clone(&e.inbound))
    };
    if let Some(inbound) = existing {
        if !end_stream {
            return Err(H2Error::Stream(stream_id, ErrCode::Protocol));
        }
        if fields.iter().any(|f| f.name.starts_with(':')) {
            return Err(H2Error::Stream(stream_id, ErrCode::Protocol));
        }
        let mut trailers = Header::new();
        for f in &fields {
            trailers.add(&f.name, f.value.as_str());
        }
        return inbound
            .finish(Some(trailers))
            .map_err(|e| match e {
                H2Error::Stream(_, code) => H2Error::Stream(stream_id, code),
                other => other,
            });
    }

    // New stream: validate the id.
    {
        let mut s = shared.streams.lock().unwrap();
        if stream_id.is_multiple_of(2) {
            return Err(H2Error::Connection(
                ErrCode::Protocol,
                "client stream id must be odd".into(),
            ));
        }
        if stream_id <= s.max_seen {
            return Err(H2Error::Connection(
                ErrCode::Protocol,
                "stream id not greater than previous".into(),
            ));
        }
        s.max_seen = stream_id;

        if s.goaway_sent || s.open >= DEFAULT_MAX_CONCURRENT_STREAMS {
            drop(s);
            let _ = send_cmd(&shared.writer, WriteCmd::RstStream {
                stream_id,
                code: ErrCode::RefusedStream,
            });
            return Ok(());
        }
    }

    // Build the request.
    let (mut req, content_length, expect_continue) =
        match build_h2_request(fields, remote_addr, &cfg.shutdown_ctx) {
            Ok(t)  => t,
            Err(_) => return Err(H2Error::Stream(stream_id, ErrCode::Protocol)),
        };

    // Content-Length body-size pre-check (mirrors the HTTP/1.1 serve loop).
    if let Some(max) = shared.max_body_bytes
        && content_length > 0
        && content_length as u64 > max
    {
        respond_status(shared, stream_id, crate::status::REQUEST_ENTITY_TOO_LARGE);
        register_orphan(shared, stream_id, end_stream);
        return Ok(());
    }

    // Register the stream and wire up the body.
    let inbound = Arc::new(StreamInbound::new(
        (content_length >= 0).then_some(content_length),
    ));
    {
        let mut s = shared.streams.lock().unwrap();
        s.map.insert(stream_id, StreamEntry { inbound: Arc::clone(&inbound), orphan: false });
        s.open += 1;
    }
    shared.send_windows.open_stream(stream_id);

    if end_stream {
        let _ = inbound.finish(None);
    } else {
        let body = H2Body::new(
            Arc::clone(&inbound),
            Arc::clone(&shared.conn_recv),
            shared.writer.clone(),
            stream_id,
        );
        let mut body = Body::Reader(Box::new(body));
        if let Some(max) = shared.max_body_bytes {
            body = body.capped(max);
        }
        req.body = Some(body);
    }

    // 100-continue: send the interim response before the handler runs.
    if expect_continue {
        let _ = send_cmd(&shared.writer, WriteCmd::Headers {
            stream_id,
            fields:     vec![HeaderField::new(":status", "100")],
            end_stream: false,
        });
    }

    // ── Handler goroutine ────────────────────────────────────────────────────
    shared.handlers.add(1);
    let shared2  = Arc::clone(shared);
    let handler2 = Arc::clone(handler);
    go_lib::go!(move || {
        let mut w = H2ResponseWriter::new(
            stream_id,
            shared2.writer.clone(),
            Arc::clone(&shared2.send_windows),
            Arc::clone(&shared2.peer_max_frame),
        );
        w.header().set("Server", "go-http/0.1");

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handler2.serve_http(&mut w, &mut req);
        }))
        .is_err();

        if panicked && !w.header_written {
            w.write_header(crate::status::INTERNAL_SERVER_ERROR);
        }
        let _ = w.finish();
        if panicked {
            let _ = send_cmd(&shared2.writer, WriteCmd::RstStream {
                stream_id,
                code: ErrCode::Internal,
            });
        }

        // Drain leftover request body so the peer's window stays sane, then
        // release the stream.  Dropping the body sends RST(CANCEL) if the
        // client is still sending.
        drop(req);
        shared2.send_windows.close_stream(stream_id);
        shared2.stream_done(stream_id);
        shared2.handlers.done();
    });

    Ok(())
}

/// Send a headers-only response with the given status and END_STREAM.
fn respond_status(shared: &ServerShared, stream_id: u32, status: u16) {
    let _ = send_cmd(&shared.writer, WriteCmd::Headers {
        stream_id,
        fields:     vec![HeaderField::new(":status", status.to_string())],
        end_stream: true,
    });
}

/// Register a rejected stream so its DATA frames are swallowed and
/// window-credited until END_STREAM.
fn register_orphan(shared: &ServerShared, stream_id: u32, end_stream: bool) {
    if end_stream {
        let mut s = shared.streams.lock().unwrap();
        if stream_id > s.max_seen {
            s.max_seen = stream_id;
        }
        return;
    }
    let inbound = Arc::new(StreamInbound::new(None));
    inbound.fail(ErrCode::RefusedStream); // swallow + credit late DATA
    let mut s = shared.streams.lock().unwrap();
    if stream_id > s.max_seen {
        s.max_seen = stream_id;
    }
    s.map.insert(stream_id, StreamEntry { inbound, orphan: true });
}

// ---------------------------------------------------------------------------
// Request construction from pseudo-headers
// ---------------------------------------------------------------------------

/// Build a `Request` from a decoded header list (RFC 9113 §8.3).
/// Returns `(request, content_length, expect_continue)`.
fn build_h2_request(
    fields:       Vec<HeaderField>,
    remote_addr:  &str,
    shutdown_ctx: &go_lib::context::Context,
) -> Result<(Request, i64, bool), HttpError> {
    let mut method    = None;
    let mut scheme    = None;
    let mut path      = None;
    let mut authority = None;
    let mut header    = Header::new();
    let mut cookies: Vec<String> = Vec::new();
    let mut pseudo_done = false;
    let mut expect_continue = false;

    for f in &fields {
        if f.name.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(HttpError::Http2(H2Error::Stream(0, ErrCode::Protocol)));
        }
        if let Some(pseudo) = f.name.strip_prefix(':') {
            if pseudo_done {
                return Err(malformed("pseudo-header after regular header"));
            }
            let slot = match pseudo {
                "method"    => &mut method,
                "scheme"    => &mut scheme,
                "path"      => &mut path,
                "authority" => &mut authority,
                _ => return Err(malformed("unknown request pseudo-header")),
            };
            if slot.is_some() {
                return Err(malformed("duplicate pseudo-header"));
            }
            *slot = Some(f.value.clone());
        } else {
            pseudo_done = true;
            match f.name.as_str() {
                // Connection-specific headers are illegal in HTTP/2 (§8.2.2).
                "connection" | "keep-alive" | "proxy-connection" | "upgrade"
                | "transfer-encoding" => {
                    return Err(malformed("connection-specific header"));
                }
                "te" if !f.value.eq_ignore_ascii_case("trailers") => {
                    return Err(malformed("TE other than trailers"));
                }
                "cookie" => cookies.push(f.value.clone()),
                "expect" if f.value.eq_ignore_ascii_case("100-continue") => {
                    expect_continue = true;
                }
                _ => header.add(&f.name, f.value.as_str()),
            }
        }
    }

    // Cookie crumbs are concatenated with "; " (§8.2.3).
    if !cookies.is_empty() {
        header.set("Cookie", cookies.join("; "));
    }

    let method = method.ok_or_else(|| malformed("missing :method"))?;
    if method == "CONNECT" {
        // CONNECT is not supported in v1.
        return Err(malformed("CONNECT not supported"));
    }
    let scheme = scheme.ok_or_else(|| malformed("missing :scheme"))?;
    let path   = path.ok_or_else(|| malformed("missing :path"))?;
    if path.is_empty() {
        return Err(malformed("empty :path"));
    }
    let authority = authority
        .or_else(|| header.get("Host").map(str::to_owned))
        .unwrap_or_else(|| "localhost".to_owned());

    // OPTIONS * carries path "*" which is not a parseable URL path.
    let url_path = if path == "*" { "/" } else { &path };
    let url_str  = format!("{scheme}://{authority}{url_path}");
    let url = Url::parse(&url_str).map_err(|e| HttpError::InvalidUrl(e.to_string()))?;

    let content_length = match header.get("Content-Length") {
        Some(v) => v.trim().parse::<i64>().map_err(|_| malformed("bad content-length"))?,
        None    => -1,
    };

    let mut req = Request::new_with_context(&method, url.as_str(), None, shutdown_ctx.clone())?;
    req.proto          = "HTTP/2.0".to_owned();
    req.proto_major    = 2;
    req.proto_minor    = 0;
    req.header         = header;
    req.host           = authority;
    req.content_length = content_length;
    req.remote_addr    = remote_addr.to_owned();
    Ok((req, content_length, expect_continue))
}

fn malformed(msg: &str) -> HttpError {
    HttpError::Http2(H2Error::Connection(ErrCode::Protocol, msg.to_owned()))
}

// ---------------------------------------------------------------------------
// H2ResponseWriter
// ---------------------------------------------------------------------------

/// The `ResponseWriter` handed to handlers on HTTP/2 streams.
///
/// Headers become a HEADERS frame with `:status`; body writes become
/// flow-controlled DATA frames; `finish()` sends END_STREAM.
pub struct H2ResponseWriter {
    stream_id:      u32,
    writer:         Sender<WriteCmd>,
    windows:        Arc<SendWindows>,
    peer_max_frame: Arc<AtomicU32>,
    header:         Header,
    status:         u16,
    header_written: bool,
    finished:       bool,
}

impl H2ResponseWriter {
    pub fn new(
        stream_id:      u32,
        writer:         Sender<WriteCmd>,
        windows:        Arc<SendWindows>,
        peer_max_frame: Arc<AtomicU32>,
    ) -> H2ResponseWriter {
        let mut header = Header::new();
        header.set("Content-Type", "text/plain; charset=utf-8");
        H2ResponseWriter {
            stream_id,
            writer,
            windows,
            peer_max_frame,
            header,
            status: 200,
            header_written: false,
            finished: false,
        }
    }

    fn flush_headers(&mut self, end_stream: bool) -> Result<(), HttpError> {
        if self.header_written {
            return Ok(());
        }
        self.header_written = true;
        if end_stream {
            self.finished = true;
        }

        let mut fields = vec![HeaderField::new(":status", self.status.to_string())];
        for (name, values) in self.header.iter() {
            let lower = name.to_ascii_lowercase();
            // Connection-specific headers must not appear in HTTP/2.
            if matches!(
                lower.as_str(),
                "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
            ) {
                continue;
            }
            for v in values {
                fields.push(HeaderField::new(lower.clone(), v.as_str()));
            }
        }
        send_cmd(&self.writer, WriteCmd::Headers {
            stream_id: self.stream_id,
            fields,
            end_stream,
        })
        .map_err(HttpError::Http2)
    }

    /// Send END_STREAM.  Called by the serve loop after the handler returns
    /// (`finish` is not part of the `ResponseWriter` trait).
    pub fn finish(&mut self) -> Result<(), HttpError> {
        if !self.header_written {
            return self.flush_headers(true);
        }
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        send_cmd(&self.writer, WriteCmd::Data {
            stream_id:  self.stream_id,
            chunk:      Vec::new(),
            end_stream: true,
        })
        .map_err(HttpError::Http2)
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> u16 {
        self.status
    }
}

impl ResponseWriter for H2ResponseWriter {
    fn header(&mut self) -> &mut Header {
        &mut self.header
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, HttpError> {
        self.flush_headers(false)?;
        if self.finished {
            return Err(HttpError::Http2(H2Error::Closed));
        }
        let mut off = 0;
        while off < buf.len() {
            let max_frame = self.peer_max_frame.load(Ordering::Relaxed) as usize;
            let want = (buf.len() - off).min(max_frame);
            let n = self
                .windows
                .reserve(self.stream_id, want)
                .map_err(HttpError::Http2)?;
            send_cmd(&self.writer, WriteCmd::Data {
                stream_id:  self.stream_id,
                chunk:      buf[off..off + n].to_vec(),
                end_stream: false,
            })
            .map_err(HttpError::Http2)?;
            off += n;
        }
        Ok(buf.len())
    }

    fn write_header(&mut self, status_code: u16) {
        if !self.header_written {
            self.status = status_code;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use go_lib::chan::chan;

    #[test]
    fn build_request_basic() {
        let fields = vec![
            HeaderField::new(":method", "POST"),
            HeaderField::new(":scheme", "http"),
            HeaderField::new(":path", "/items?q=1"),
            HeaderField::new(":authority", "example.com:8080"),
            HeaderField::new("content-type", "application/json"),
            HeaderField::new("content-length", "12"),
            HeaderField::new("cookie", "a=1"),
            HeaderField::new("cookie", "b=2"),
        ];
        let ctx = go_lib::context::background();
        let (req, cl, expect) = build_h2_request(fields, "1.2.3.4:5", &ctx).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.proto, "HTTP/2.0");
        assert_eq!(req.proto_major, 2);
        assert_eq!(req.host, "example.com:8080");
        assert_eq!(req.url.path(), "/items");
        assert_eq!(req.url.query(), Some("q=1"));
        assert_eq!(req.header.get("Cookie"), Some("a=1; b=2"));
        assert_eq!(req.remote_addr, "1.2.3.4:5");
        assert_eq!(cl, 12);
        assert!(!expect);
    }

    #[test]
    fn build_request_rejects_malformed() {
        let ctx = go_lib::context::background();

        // Missing :method.
        let missing = vec![
            HeaderField::new(":scheme", "http"),
            HeaderField::new(":path", "/"),
        ];
        assert!(build_h2_request(missing, "", &ctx).is_err());

        // Pseudo-header after a regular one.
        let late_pseudo = vec![
            HeaderField::new(":method", "GET"),
            HeaderField::new("accept", "*/*"),
            HeaderField::new(":path", "/"),
        ];
        assert!(build_h2_request(late_pseudo, "", &ctx).is_err());

        // Duplicate pseudo-header.
        let dup = vec![
            HeaderField::new(":method", "GET"),
            HeaderField::new(":method", "GET"),
            HeaderField::new(":scheme", "http"),
            HeaderField::new(":path", "/"),
        ];
        assert!(build_h2_request(dup, "", &ctx).is_err());

        // Connection-specific header.
        let connection = vec![
            HeaderField::new(":method", "GET"),
            HeaderField::new(":scheme", "http"),
            HeaderField::new(":path", "/"),
            HeaderField::new("connection", "keep-alive"),
        ];
        assert!(build_h2_request(connection, "", &ctx).is_err());

        // Uppercase header name.
        let upper = vec![
            HeaderField::new(":method", "GET"),
            HeaderField::new(":scheme", "http"),
            HeaderField::new(":path", "/"),
            HeaderField::new("Accept", "*/*"),
        ];
        assert!(build_h2_request(upper, "", &ctx).is_err());
    }

    #[test]
    fn build_request_expect_continue() {
        let ctx = go_lib::context::background();
        let fields = vec![
            HeaderField::new(":method", "PUT"),
            HeaderField::new(":scheme", "http"),
            HeaderField::new(":path", "/upload"),
            HeaderField::new("expect", "100-continue"),
        ];
        let (_req, _cl, expect) = build_h2_request(fields, "", &ctx).unwrap();
        assert!(expect);
    }

    #[test]
    fn response_writer_defaults_and_status() {
        let (tx, rx) = chan::<WriteCmd>(8);
        let windows = Arc::new(SendWindows::new(65_535));
        windows.open_stream(1);
        let mut w = H2ResponseWriter::new(1, tx, windows, Arc::new(AtomicU32::new(16_384)));

        w.write_header(404);
        w.write(b"nope").unwrap(); // flushes headers with status 404
        w.write_header(500);       // ignored — headers already sent
        assert_eq!(w.status(), 404);
        w.finish().unwrap();

        match rx.try_recv() {
            Some(Some(WriteCmd::Headers { stream_id: 1, fields, end_stream: false })) => {
                assert_eq!(fields[0], HeaderField::new(":status", "404"));
                assert!(fields.iter().any(|f| f.name == "content-type"));
            }
            _ => panic!("expected Headers first"),
        }
        match rx.try_recv() {
            Some(Some(WriteCmd::Data { chunk, end_stream: false, .. })) => {
                assert_eq!(chunk, b"nope");
            }
            _ => panic!("expected Data"),
        }
        match rx.try_recv() {
            Some(Some(WriteCmd::Data { chunk, end_stream: true, .. })) => {
                assert!(chunk.is_empty());
            }
            _ => panic!("expected final empty Data with END_STREAM"),
        }
    }

    #[test]
    fn response_writer_strips_connection_headers() {
        let (tx, rx) = chan::<WriteCmd>(8);
        let windows = Arc::new(SendWindows::new(65_535));
        windows.open_stream(1);
        let mut w = H2ResponseWriter::new(1, tx, windows, Arc::new(AtomicU32::new(16_384)));
        w.header().set("Connection", "close");
        w.header().set("Transfer-Encoding", "chunked");
        w.header().set("X-Ok", "yes");
        w.finish().unwrap();

        match rx.try_recv() {
            Some(Some(WriteCmd::Headers { fields, end_stream: true, .. })) => {
                assert!(fields.iter().all(|f| f.name != "connection"));
                assert!(fields.iter().all(|f| f.name != "transfer-encoding"));
                assert!(fields.iter().any(|f| f.name == "x-ok"));
            }
            _ => panic!("expected headers-only response"),
        }
    }
}

// SPDX-License-Identifier: Apache-2.0

//! HTTP/2 integration tests.
//!
//! Raw-socket tests speak frames directly (using the crate's own codecs) at a
//! `Server` with `enable_h2c` — proving the server end-to-end without the
//! client.  Client-based tests exercise the full public API on both sides.

use std::io::Write;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use go_http::h2::frame::{self, flags, Frame};
use go_http::h2::hpack::{Decoder, Encoder, HeaderField};
use go_http::h2::PREFACE;
use go_http::handler::ServeMux;
use go_http::server::Server;

static PORT: AtomicU16 = AtomicU16::new(19400);

fn next_port() -> u16 {
    PORT.fetch_add(1, Ordering::SeqCst)
}

/// Start an h2c-enabled server on its own goroutine and wait until it
/// accepts connections (a fixed sleep races on slow CI runners).
fn start_h2c_server(mux: Arc<ServeMux>) -> String {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let addr2 = addr.clone();
    go_lib::go!(move || {
        let mut srv = Server::new(addr2);
        srv.handler = Some(mux);
        srv.enable_h2c = true;
        let _ = srv.listen_and_serve();
    });
    wait_until_ready(&addr);
    addr
}

/// Probe-connect until the listener is up.  The probe connection is dropped
/// immediately; the server sees a zero-byte connection and moves on.
fn wait_until_ready(addr: &str) {
    for _ in 0..250 {
        if go_lib::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        go_lib::sleep(Duration::from_millis(20));
    }
    panic!("server at {addr} did not become ready");
}

/// A minimal raw h2 client for driving the server directly.
struct RawH2 {
    stream:  go_lib::net::TcpStream,
    encoder: Encoder,
    decoder: Decoder,
}

impl RawH2 {
    fn connect(addr: &str) -> RawH2 {
        let mut stream = go_lib::net::TcpStream::connect(addr).unwrap();
        stream.write_all(PREFACE).unwrap();
        // Client SETTINGS (empty) — must be the first frame.
        let mut buf = Vec::new();
        frame::write_frame(&mut buf, frame::TYPE_SETTINGS, 0, 0, &[]);
        stream.write_all(&buf).unwrap();
        RawH2 { stream, encoder: Encoder::new(), decoder: Decoder::new(1 << 20) }
    }

    fn send_headers(&mut self, stream_id: u32, fields: &[HeaderField], end_stream: bool) {
        let mut block = Vec::new();
        self.encoder.encode(fields, &mut block);
        let mut fl = flags::END_HEADERS;
        if end_stream {
            fl |= flags::END_STREAM;
        }
        let mut buf = Vec::new();
        frame::write_frame(&mut buf, frame::TYPE_HEADERS, fl, stream_id, &block);
        self.stream.write_all(&buf).unwrap();
    }

    fn send_data(&mut self, stream_id: u32, data: &[u8], end_stream: bool) {
        let fl = if end_stream { flags::END_STREAM } else { 0 };
        let mut buf = Vec::new();
        frame::write_frame(&mut buf, frame::TYPE_DATA, fl, stream_id, data);
        self.stream.write_all(&buf).unwrap();
    }

    fn read_frame(&mut self) -> Frame {
        let (h, payload) = frame::read_frame(&mut self.stream, 1 << 24).unwrap();
        frame::parse_frame(h, payload).unwrap()
    }

    /// Read frames until the response for `stream_id` is complete.
    /// Returns (status, body bytes).  Acks SETTINGS along the way.
    fn read_response(&mut self, stream_id: u32) -> (u16, Vec<u8>) {
        let mut status = 0u16;
        let mut body = Vec::new();
        loop {
            match self.read_frame() {
                Frame::Settings { ack: false, .. } => {
                    let mut buf = Vec::new();
                    frame::write_frame(&mut buf, frame::TYPE_SETTINGS, flags::ACK, 0, &[]);
                    self.stream.write_all(&buf).unwrap();
                }
                Frame::Settings { ack: true, .. } => {}
                Frame::Ping { ack: false, payload } => {
                    let mut buf = Vec::new();
                    frame::write_frame(&mut buf, frame::TYPE_PING, flags::ACK, 0, &payload);
                    self.stream.write_all(&buf).unwrap();
                }
                Frame::WindowUpdate { .. } => {}
                Frame::Headers { stream_id: sid, fragment, end_stream, end_headers } => {
                    assert!(end_headers, "test helper does not reassemble CONTINUATION");
                    if sid != stream_id {
                        continue;
                    }
                    let fields = self.decoder.decode(&fragment).unwrap();
                    let s: u16 = fields
                        .iter()
                        .find(|f| f.name == ":status")
                        .expect("response must carry :status")
                        .value
                        .parse()
                        .unwrap();
                    // Skip interim (1xx) responses.
                    if (100..200).contains(&s) {
                        continue;
                    }
                    status = s;
                    if end_stream {
                        return (status, body);
                    }
                }
                Frame::Data { stream_id: sid, data, end_stream, .. } => {
                    if sid == stream_id {
                        body.extend_from_slice(&data);
                        if end_stream {
                            return (status, body);
                        }
                    }
                }
                Frame::GoAway { code, .. } => {
                    panic!("unexpected GOAWAY ({code}) while awaiting stream {stream_id}");
                }
                Frame::RstStream { stream_id: sid, code } => {
                    panic!("unexpected RST_STREAM({code}) on stream {sid}");
                }
                _ => {}
            }
        }
    }
}

fn request_fields(method: &str, addr: &str, path: &str) -> Vec<HeaderField> {
    vec![
        HeaderField::new(":method", method),
        HeaderField::new(":scheme", "http"),
        HeaderField::new(":authority", addr),
        HeaderField::new(":path", path),
    ]
}

// ---------------------------------------------------------------------------
// Raw-socket tests (server side only)
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn h2c_get_basic() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/hello", |w, r| {
        assert_eq!(r.proto, "HTTP/2.0");
        assert_eq!(r.proto_major, 2);
        w.header().set("Content-Type", "text/plain");
        let _ = w.write(b"Hello, h2!");
    });
    let addr = start_h2c_server(mux);

    let mut c = RawH2::connect(&addr);
    c.send_headers(1, &request_fields("GET", &addr, "/hello"), true);
    let (status, body) = c.read_response(1);
    assert_eq!(status, 200);
    assert_eq!(body, b"Hello, h2!");
}

#[test]
#[go_lib::main]
fn h2c_post_echo() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/echo", |w, r| {
        let body = r.body_bytes().unwrap();
        let _ = w.write(&body);
    });
    let addr = start_h2c_server(mux);

    let mut c = RawH2::connect(&addr);
    let mut fields = request_fields("POST", &addr, "/echo");
    fields.push(HeaderField::new("content-length", "11"));
    c.send_headers(1, &fields, false);
    c.send_data(1, b"hello world", true);
    let (status, body) = c.read_response(1);
    assert_eq!(status, 200);
    assert_eq!(body, b"hello world");
}

#[test]
#[go_lib::main]
fn h2c_multiple_streams_one_connection() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/n", |w, r| {
        let n = r.url.query().unwrap_or("?").to_owned();
        let _ = w.write(n.as_bytes());
    });
    let addr = start_h2c_server(mux);

    let mut c = RawH2::connect(&addr);
    // Interleave three streams before reading any response.
    for (i, sid) in [1u32, 3, 5].iter().enumerate() {
        c.send_headers(*sid, &request_fields("GET", &addr, &format!("/n?i={i}")), true);
    }
    // Responses can arrive in any order; collect by stream id.
    let mut results = std::collections::HashMap::new();
    for _ in 0..3 {
        // read_response tracks one stream; instead read frames manually.
        // Simplest: read until all three bodies are complete.
        loop {
            match c.read_frame() {
                Frame::Settings { ack: false, .. } => {
                    let mut buf = Vec::new();
                    frame::write_frame(&mut buf, frame::TYPE_SETTINGS, flags::ACK, 0, &[]);
                    c.stream.write_all(&buf).unwrap();
                }
                Frame::Headers { stream_id, fragment, end_stream, .. } => {
                    let fields = c.decoder.decode(&fragment).unwrap();
                    let status: u16 = fields.iter().find(|f| f.name == ":status")
                        .unwrap().value.parse().unwrap();
                    assert_eq!(status, 200);
                    if end_stream {
                        results.insert(stream_id, Vec::new());
                        break;
                    }
                }
                Frame::Data { stream_id, data, end_stream, .. } => {
                    results.entry(stream_id).or_insert_with(Vec::new)
                        .extend_from_slice(&data);
                    if end_stream {
                        break;
                    }
                }
                _ => {}
            }
        }
        if results.len() == 3
            && results.values().all(|v| !v.is_empty())
        {
            break;
        }
    }
    assert_eq!(results.get(&1).map(|v| v.as_slice()), Some(b"i=0".as_slice()));
    assert_eq!(results.get(&3).map(|v| v.as_slice()), Some(b"i=1".as_slice()));
    assert_eq!(results.get(&5).map(|v| v.as_slice()), Some(b"i=2".as_slice()));
}

#[test]
#[go_lib::main]
fn h2c_large_response_flow_control() {
    // 200 KiB response: requires the server to respect our connection window
    // and our WINDOW_UPDATEs to unblock it.
    const SIZE: usize = 200 * 1024;
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/big", move |w, _r| {
        let chunk = vec![b'x'; SIZE];
        let _ = w.write(&chunk);
    });
    let addr = start_h2c_server(mux);

    let mut c = RawH2::connect(&addr);
    c.send_headers(1, &request_fields("GET", &addr, "/big"), true);

    let mut body = Vec::new();
    let mut received_since_update = 0u32;
    loop {
        match c.read_frame() {
            Frame::Settings { ack: false, .. } => {
                let mut buf = Vec::new();
                frame::write_frame(&mut buf, frame::TYPE_SETTINGS, flags::ACK, 0, &[]);
                c.stream.write_all(&buf).unwrap();
            }
            Frame::Settings { ack: true, .. }
            | Frame::WindowUpdate { .. }
            | Frame::Headers { .. } => {}
            Frame::Data { data, end_stream, flow_len, .. } => {
                body.extend_from_slice(&data);
                received_since_update += flow_len;
                if end_stream {
                    break;
                }
                // Return window so the server can keep sending.
                if received_since_update >= 16 * 1024 {
                    let inc = received_since_update.to_be_bytes();
                    let mut buf = Vec::new();
                    frame::write_frame(&mut buf, frame::TYPE_WINDOW_UPDATE, 0, 0, &inc);
                    frame::write_frame(&mut buf, frame::TYPE_WINDOW_UPDATE, 0, 1, &inc);
                    c.stream.write_all(&buf).unwrap();
                    received_since_update = 0;
                }
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert_eq!(body.len(), SIZE);
    assert!(body.iter().all(|&b| b == b'x'));
}

#[test]
#[go_lib::main]
fn h2c_ping_pong() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/", |w, _r| {
        let _ = w.write(b"ok");
    });
    let addr = start_h2c_server(mux);

    let mut c = RawH2::connect(&addr);
    let mut buf = Vec::new();
    frame::write_frame(&mut buf, frame::TYPE_PING, 0, 0, &[9; 8]);
    c.stream.write_all(&buf).unwrap();

    loop {
        match c.read_frame() {
            Frame::Ping { ack: true, payload } => {
                assert_eq!(payload, [9; 8]);
                break;
            }
            Frame::Settings { .. } | Frame::WindowUpdate { .. } => {}
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

#[test]
#[go_lib::main]
fn h2c_bad_first_frame_gets_goaway() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/", |w, _r| {
        let _ = w.write(b"ok");
    });
    let addr = start_h2c_server(mux);

    // Preface, then PING instead of SETTINGS.
    let mut stream = go_lib::net::TcpStream::connect(addr.as_str()).unwrap();
    stream.write_all(PREFACE).unwrap();
    let mut buf = Vec::new();
    frame::write_frame(&mut buf, frame::TYPE_PING, 0, 0, &[0; 8]);
    stream.write_all(&buf).unwrap();

    // Expect the server's SETTINGS then GOAWAY(PROTOCOL_ERROR), then EOF.
    let mut saw_goaway = false;
    while let Ok((h, p)) = frame::read_frame(&mut stream, 1 << 24) {
        if let Frame::GoAway { code, .. } = frame::parse_frame(h, p).unwrap() {
            assert_eq!(code, go_http::h2::ErrCode::Protocol);
            saw_goaway = true;
        }
    }
    assert!(saw_goaway, "server must send GOAWAY on a protocol error");
}

#[test]
#[go_lib::main]
fn h2c_fallback_to_http1() {
    // The same enable_h2c server must still serve plain HTTP/1.1.
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/hello", |w, _r| {
        let _ = w.write(b"still h1");
    });
    let addr = start_h2c_server(mux);

    let mut stream = go_lib::net::TcpStream::connect(addr.as_str()).unwrap();
    stream
        .write_all(format!("GET /hello HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes())
        .unwrap();
    let mut response = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => response.extend_from_slice(&tmp[..n]),
        }
    }
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "got: {text}");
    assert!(text.contains("still h1"), "got: {text}");
}

#[test]
#[go_lib::main]
fn h2c_request_trailers() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/tr", |w, r| {
        let _ = r.body_bytes().unwrap();
        let checksum = r.trailers().get("X-Checksum").unwrap_or("missing").to_owned();
        w.header().set("X-Got-Checksum", checksum);
        let _ = w.write(b"ok");
    });
    let addr = start_h2c_server(mux);

    let mut c = RawH2::connect(&addr);
    c.send_headers(1, &request_fields("POST", &addr, "/tr"), false);
    c.send_data(1, b"payload", false);
    // Trailers: HEADERS with END_STREAM, no pseudo-headers.
    let mut block = Vec::new();
    c.encoder.encode(&[HeaderField::new("x-checksum", "abc123")], &mut block);
    let mut buf = Vec::new();
    frame::write_frame(
        &mut buf,
        frame::TYPE_HEADERS,
        flags::END_HEADERS | flags::END_STREAM,
        1,
        &block,
    );
    c.stream.write_all(&buf).unwrap();

    // Find the response HEADERS and check the echoed trailer.
    loop {
        match c.read_frame() {
            Frame::Settings { ack: false, .. } => {
                let mut b = Vec::new();
                frame::write_frame(&mut b, frame::TYPE_SETTINGS, flags::ACK, 0, &[]);
                c.stream.write_all(&b).unwrap();
            }
            Frame::Headers { fragment, .. } => {
                let fields = c.decoder.decode(&fragment).unwrap();
                let got = fields.iter().find(|f| f.name == "x-got-checksum");
                assert_eq!(got.map(|f| f.value.as_str()), Some("abc123"));
                break;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Full public-API tests: go-http h2 client <-> go-http h2c server
// ---------------------------------------------------------------------------

use go_http::client::{Client, Transport};
use go_http::parse::transfer::Body;

/// A `Client` whose transport speaks h2c with prior knowledge.
fn h2c_client() -> Client {
    let mut transport = Transport::new();
    transport.h2c_prior_knowledge = true;
    let mut client = Client::new();
    client.transport = Arc::new(transport);
    client
}

#[test]
#[go_lib::main]
fn client_get_over_h2c() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/hello", |w, r| {
        assert_eq!(r.proto_major, 2);
        w.header().set("X-Proto", r.proto.clone());
        let _ = w.write(b"Hello, client!");
    });
    let addr = start_h2c_server(mux);

    let client = h2c_client();
    let mut resp = client.get(&format!("http://{addr}/hello")).unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.proto, "HTTP/2.0");
    assert_eq!(resp.proto_major, 2);
    assert_eq!(resp.header.get("X-Proto"), Some("HTTP/2.0"));
    assert_eq!(resp.body_string().unwrap(), "Hello, client!");
}

#[test]
#[go_lib::main]
fn client_post_echo_over_h2c() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/echo", |w, r| {
        let body = r.body_bytes().unwrap();
        w.header().set("Content-Type", "application/octet-stream");
        let _ = w.write(&body);
    });
    let addr = start_h2c_server(mux);

    let client = h2c_client();
    let payload = b"the quick brown fox".to_vec();
    let body = Body::Unbounded(Box::new(std::io::Cursor::new(payload.clone())));
    let mut resp = client
        .post(&format!("http://{addr}/echo"), "application/octet-stream", body)
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_bytes().unwrap(), payload);
}

#[test]
#[go_lib::main]
fn client_large_bodies_both_ways() {
    // > 64 KiB in both directions forces WINDOW_UPDATE exchange on both ends.
    const SIZE: usize = 300 * 1024;
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/big-echo", |w, r| {
        let body = r.body_bytes().unwrap();
        let _ = w.write(&body);
    });
    let addr = start_h2c_server(mux);

    let client = h2c_client();
    let payload: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    let body = Body::Unbounded(Box::new(std::io::Cursor::new(payload.clone())));
    let mut resp = client
        .post(&format!("http://{addr}/big-echo"), "application/octet-stream", body)
        .unwrap();
    assert_eq!(resp.status, 200);
    let echoed = resp.body_bytes().unwrap();
    assert_eq!(echoed.len(), SIZE);
    assert_eq!(echoed, payload);
}

#[test]
#[go_lib::main]
fn client_concurrent_round_trips_share_connection() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/work", |w, r| {
        // Small stagger so streams genuinely overlap.
        go_lib::sleep(Duration::from_millis(5));
        let q = r.url.query().unwrap_or("").to_owned();
        let _ = w.write(q.as_bytes());
    });
    let addr = start_h2c_server(mux);

    let transport = {
        let mut t = Transport::new();
        t.h2c_prior_knowledge = true;
        Arc::new(t)
    };

    let wg = Arc::new(go_lib::sync::WaitGroup::new());
    let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for i in 0..50 {
        wg.add(1);
        let wg = Arc::clone(&wg);
        let failures = Arc::clone(&failures);
        let transport = Arc::clone(&transport);
        let addr = addr.clone();
        go_lib::go!(move || {
            let mut client = Client::new();
            client.transport = transport;
            match client.get(&format!("http://{addr}/work?id={i}")) {
                Ok(mut resp) => {
                    let body = resp.body_string().unwrap_or_default();
                    if resp.status != 200 || body != format!("id={i}") {
                        failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                Err(_) => {
                    failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            wg.done();
        });
    }
    wg.wait();
    assert_eq!(failures.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
#[go_lib::main]
fn client_404_and_headers() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/known", |w, _r| {
        let _ = w.write(b"yes");
    });
    let addr = start_h2c_server(mux);

    let client = h2c_client();
    let mut resp = client.get(&format!("http://{addr}/unknown")).unwrap();
    assert_eq!(resp.status, 404);
    let _ = resp.body_bytes();
}

#[test]
#[go_lib::main]
fn client_sequential_requests_reuse_connection() {
    let mux = Arc::new(ServeMux::new());
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits2 = Arc::clone(&hits);
    mux.handle_func("/count", move |w, _r| {
        let n = hits2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = w.write(format!("{n}").as_bytes());
    });
    let addr = start_h2c_server(mux);

    let client = h2c_client();
    for expect in 0..5 {
        let mut resp = client.get(&format!("http://{addr}/count")).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_string().unwrap(), format!("{expect}"));
    }
}

#[test]
#[go_lib::main]
fn client_response_trailers() {
    // Server handlers cannot send trailers yet (H2ResponseWriter has no
    // trailer API), so drive the server side with the raw helper instead:
    // this test exercises the CLIENT's trailer path via a raw h2 server.
    let listener = go_lib::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();

    go_lib::go!(move || {
        let mut s = listener.accept().unwrap();
        // Read preface.
        let mut preface = [0u8; 24];
        {
            let mut got = 0;
            while got < 24 {
                let n = s.read(&mut preface[got..]).unwrap();
                assert!(n > 0);
                got += n;
            }
        }
        assert_eq!(&preface, PREFACE);
        // Server SETTINGS + read frames until request HEADERS arrive.
        let mut out = Vec::new();
        frame::write_frame(&mut out, frame::TYPE_SETTINGS, 0, 0, &[]);
        s.write_all(&out).unwrap();

        let mut enc = Encoder::new();
        loop {
            let (h, p) = frame::read_frame(&mut s, 1 << 24).unwrap();
            match frame::parse_frame(h, p).unwrap() {
                Frame::Settings { ack: false, .. } => {
                    let mut b = Vec::new();
                    frame::write_frame(&mut b, frame::TYPE_SETTINGS, flags::ACK, 0, &[]);
                    s.write_all(&b).unwrap();
                }
                Frame::Headers { stream_id, .. } => {
                    // Respond: HEADERS, DATA, then trailers.
                    let mut b = Vec::new();
                    let mut block = Vec::new();
                    enc.encode(&[HeaderField::new(":status", "200")], &mut block);
                    frame::write_frame(&mut b, frame::TYPE_HEADERS, flags::END_HEADERS, stream_id, &block);
                    frame::write_frame(&mut b, frame::TYPE_DATA, 0, stream_id, b"payload");
                    let mut tblock = Vec::new();
                    enc.encode(&[HeaderField::new("x-checksum", "xyz789")], &mut tblock);
                    frame::write_frame(
                        &mut b,
                        frame::TYPE_HEADERS,
                        flags::END_HEADERS | flags::END_STREAM,
                        stream_id,
                        &tblock,
                    );
                    s.write_all(&b).unwrap();
                }
                _ => {}
            }
        }
    });
    go_lib::sleep(Duration::from_millis(50));

    let client = h2c_client();
    let mut resp = client.get(&format!("http://{addr}/t")).unwrap();
    assert_eq!(resp.status, 200);
    let body = resp.body_bytes().unwrap();
    assert_eq!(body, b"payload");
    assert_eq!(resp.trailer.get("X-Checksum"), Some("xyz789"));
}

#[test]
#[go_lib::main]
fn client_timeout_applies_to_h2() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/slow", |w, _r| {
        go_lib::sleep(Duration::from_millis(500));
        let _ = w.write(b"late");
    });
    let addr = start_h2c_server(mux);

    let mut client = h2c_client();
    client.timeout = Some(Duration::from_millis(80));
    match client.get(&format!("http://{addr}/slow")) {
        Err(err) => assert!(matches!(err, go_http::error::HttpError::Timeout), "{err:?}"),
        Ok(_)    => panic!("expected a timeout error"),
    }
}

/// KNOWN WINDOWS GAP: go-lib's IOCP backend cannot interrupt a parked
/// overlapped WSARecv via socket shutdown, so the idle-timeout and
/// shutdown-drain paths cannot unblock the connection reader on Windows —
/// these two tests hang there.  Needs a go-lib fix (e.g. a TcpStream
/// shutdown API that posts a completion / CancelIoEx); see the tracking
/// note in the PR.
#[cfg(not(windows))]
#[test]
#[go_lib::main]
fn server_shutdown_sends_goaway_and_drains() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/slow", |w, _r| {
        go_lib::sleep(Duration::from_millis(100));
        let _ = w.write(b"finished");
    });
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let srv = Arc::new({
        let mut s = Server::new(addr.clone());
        s.handler = Some(mux);
        s.enable_h2c = true;
        s
    });
    let srv2 = Arc::clone(&srv);
    go_lib::go!(move || {
        let _ = srv2.listen_and_serve();
    });
    wait_until_ready(&addr);

    // Kick off an in-flight request, then shut the server down mid-handler.
    let (result_tx, result_rx) = go_lib::chan::chan::<Option<String>>(1);
    let addr2 = addr.clone();
    go_lib::go!(move || {
        let client = h2c_client();
        let body = client
            .get(&format!("http://{addr2}/slow"))
            .ok()
            .and_then(|mut r| r.body_string().ok());
        let _ = result_tx.try_send(body);
    });
    go_lib::sleep(Duration::from_millis(30)); // request is now in the handler

    srv.shutdown();

    // The in-flight request must still complete.
    let body = result_rx.recv().flatten();
    assert_eq!(body.as_deref(), Some("finished"), "in-flight request must finish");
}

// ---------------------------------------------------------------------------
// Hardening: idle timeout and header-list limits
// ---------------------------------------------------------------------------

/// KNOWN WINDOWS GAP: go-lib's IOCP backend cannot interrupt a parked
/// overlapped WSARecv via socket shutdown, so the idle-timeout and
/// shutdown-drain paths cannot unblock the connection reader on Windows —
/// these two tests hang there.  Needs a go-lib fix (e.g. a TcpStream
/// shutdown API that posts a completion / CancelIoEx); see the tracking
/// note in the PR.
#[cfg(not(windows))]
#[test]
#[go_lib::main]
fn h2c_idle_timeout_closes_connection() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/", |w, _r| {
        let _ = w.write(b"ok");
    });
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let addr2 = addr.clone();
    go_lib::go!(move || {
        let mut srv = Server::new(addr2);
        srv.handler = Some(Arc::new(ServeMux::new()));
        srv.enable_h2c = true;
        srv.idle_timeout = Some(Duration::from_millis(80));
        let _ = srv.listen_and_serve();
    });
    wait_until_ready(&addr);

    // Connect (preface + SETTINGS) and then stay completely silent: any
    // later write (e.g. a SETTINGS ack) races the idle close — unread bytes
    // at close turn the FIN into an RST, discarding the buffered GOAWAY.
    let mut c = RawH2::connect(&addr);

    // With no streams, the idle watchdog must GOAWAY and close within a
    // couple of intervals.
    let mut saw_goaway = false;
    while let Ok((h, p)) = frame::read_frame(&mut c.stream, 1 << 24) {
        if let Frame::GoAway { .. } = frame::parse_frame(h, p).unwrap() {
            saw_goaway = true;
        }
    }
    assert!(saw_goaway, "expected GOAWAY before idle close");
}

#[test]
#[go_lib::main]
fn h2c_oversized_header_list_gets_431() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/", |w, _r| {
        let _ = w.write(b"ok");
    });
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let addr2 = addr.clone();
    let mux2 = Arc::clone(&mux);
    go_lib::go!(move || {
        let mut srv = Server::new(addr2);
        srv.handler = Some(mux2);
        srv.enable_h2c = true;
        srv.max_header_bytes = 4096; // advertised as MAX_HEADER_LIST_SIZE
        let _ = srv.listen_and_serve();
    });
    wait_until_ready(&addr);

    let mut c = RawH2::connect(&addr);
    let mut fields = request_fields("GET", &addr, "/");
    // One header far larger than the 4 KiB limit.
    fields.push(HeaderField::new("x-big", "v".repeat(8192)));
    c.send_headers(1, &fields, true);

    let (status, _body) = c.read_response(1);
    assert_eq!(status, 431, "oversized header list must be rejected with 431");

    // The connection must survive: a normal request still works.
    c.send_headers(3, &request_fields("GET", &addr, "/"), true);
    let (status2, body2) = c.read_response(3);
    assert_eq!(status2, 200);
    assert_eq!(body2, b"ok");
}

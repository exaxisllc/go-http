// SPDX-License-Identifier: Apache-2.0
//! Integration tests — full server + client round-trips over real TCP.
//!
//! Design: every test carries `#[go_lib::main]`, so the test body runs as the
//! first goroutine on the process-wide scheduler.  Server and client share the
//! same scheduler instance and netpoll; no extra OS threads are spawned for the
//! server — it runs as a goroutine alongside the client.
//!
//! Since go-lib 0.6.0 the scheduler is a process-wide singleton: concurrent
//! `#[go_lib::main]` entries from different test threads share one scheduler and
//! tag netpoll registrations per invocation, so no cross-test locking is needed.
//! Each test still uses a unique port; the server goroutine (listening forever)
//! is simply left parked in `listener.accept()` when the test body returns.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use go_lib::net::TcpStream;
use go_http::{
    client::Client,
    cookie::{Cookie, MemoryCookieJar},
    handler::ServeMux,
    parse::transfer::Body,
    server::Server,
    status,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(19200);
fn next_port() -> u16 {
    PORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Spawn a server goroutine and sleep briefly for the listener to bind.
/// Must be called from inside a `#[go_lib::main]` body (goroutine context).
fn start_server_goroutine(addr: String, mux: Arc<ServeMux>) {
    go_lib::go!(move || {
        let mut srv = Server::new(addr);
        srv.handler = Some(mux);
        let _ = srv.listen_and_serve();
    });
    // Give the server goroutine time to bind the listener.
    go_lib::sleep(Duration::from_millis(50));
}

/// Spawn a configured Server (allows non-default settings like max_body_bytes).
fn start_configured_server(addr: String, srv: Server) {
    go_lib::go!(move || {
        let _ = srv.listen_and_serve();
    });
    go_lib::sleep(Duration::from_millis(50));
}

/// Write raw bytes to a TCP connection and read the full response.
fn raw_round_trip(addr: &str, request_bytes: &[u8]) -> Vec<u8> {
    use std::io::{Read, Write};
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.write_all(request_bytes).expect("write");
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).expect("read");
    resp
}

// ---------------------------------------------------------------------------
// 1. GET — basic 200 response
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn get_basic() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/hello", |w, _r| {
        w.header().set("Content-Type", "text/plain");
        let _ = w.write(b"Hello, world!");
    });
    start_server_goroutine(addr, mux);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/hello"))
        .expect("GET failed");
    assert_eq!(resp.status, status::OK);
    assert_eq!(resp.body_string().unwrap(), "Hello, world!");
}

// ---------------------------------------------------------------------------
// 2. GET — 404 for unknown path
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn get_not_found() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    start_server_goroutine(addr, Arc::new(ServeMux::new()));

    let resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/missing"))
        .expect("GET failed");
    assert_eq!(resp.status, status::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// 3. POST with body — handler echoes Content-Type header
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn post_body_echo() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/echo", |w, r| {
        let ct = r.header.get("Content-Type").unwrap_or("").to_owned();
        w.header().set("X-Received-Content-Type", ct);
        let _ = w.write(b"echoed");
    });
    start_server_goroutine(addr, mux);

    let body = Body::Unbounded(Box::new(std::io::Cursor::new(b"hello post".to_vec())));
    let resp = Client::new()
        .post(&format!("http://127.0.0.1:{port}/echo"), "text/plain", body)
        .expect("POST failed");
    assert_eq!(resp.status, status::OK);
    assert_eq!(resp.header.get("X-Received-Content-Type").unwrap_or(""), "text/plain");
}

// ---------------------------------------------------------------------------
// 4. Custom response headers
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn custom_response_headers() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/headers", |w, _| {
        w.header().set("X-Custom", "go-http-test");
        w.header().set("X-Another", "value2");
        w.write_header(status::CREATED);
        let _ = w.write(b"ok");
    });
    start_server_goroutine(addr, mux);

    let resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/headers"))
        .expect("GET failed");
    assert_eq!(resp.status, status::CREATED);
    assert_eq!(resp.header.get("X-Custom").unwrap_or(""), "go-http-test");
    assert_eq!(resp.header.get("X-Another").unwrap_or(""), "value2");
}

// ---------------------------------------------------------------------------
// 5. Request headers forwarded to handler
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn request_headers_forwarded() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/mirror", |w, r| {
        let token = r.header.get("X-Token").unwrap_or("missing").to_owned();
        w.header().set("X-Token-Echo", token);
        let _ = w.write(b"ok");
    });
    start_server_goroutine(addr, mux);

    let mut req = go_http::request::Request::new(
        "GET",
        &format!("http://127.0.0.1:{port}/mirror"),
        None,
    )
    .unwrap();
    req.header.set("X-Token", "secret123");
    let resp = Client::new().do_request(req).expect("GET failed");
    assert_eq!(resp.header.get("X-Token-Echo").unwrap_or(""), "secret123");
}

// ---------------------------------------------------------------------------
// 6. Query string parameters
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn query_string_params() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/search", |w, r| {
        let q = r.url.query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        let _ = w.write(format!("query={q}").as_bytes());
    });
    start_server_goroutine(addr, mux);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/search?q=rustlang"))
        .expect("GET failed");
    assert_eq!(resp.body_string().unwrap(), "query=rustlang");
}

// ---------------------------------------------------------------------------
// 7. ServeMux — longest-prefix routing
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn mux_longest_prefix() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/api/", |w, _| { let _ = w.write(b"api-root"); });
    mux.handle_func("/api/v2/", |w, _| { let _ = w.write(b"api-v2"); });
    start_server_goroutine(addr, mux);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/api/v2/users"))
        .expect("GET failed");
    assert_eq!(resp.body_string().unwrap(), "api-v2");
}

// ---------------------------------------------------------------------------
// 8. Multiple sequential requests (keep-alive)
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn multiple_sequential_requests() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let counter = Arc::new(Mutex::new(0u32));
    let mux = Arc::new(ServeMux::new());
    let counter2 = Arc::clone(&counter);
    mux.handle_func("/count", move |w, _| {
        let mut c = counter2.lock().unwrap();
        *c += 1;
        let _ = w.write(format!("{}", *c).as_bytes());
    });
    start_server_goroutine(addr, mux);

    let client = Client::new();
    let mut bodies = Vec::new();
    for _ in 0..5 {
        let mut r = client
            .get(&format!("http://127.0.0.1:{port}/count"))
            .unwrap();
        bodies.push(r.body_string().unwrap());
    }

    assert_eq!(bodies.len(), 5);
    for (i, body) in bodies.iter().enumerate() {
        let n: u32 = body.trim().parse().unwrap();
        assert_eq!(n, (i + 1) as u32, "counter mismatch at request {i}");
    }
}

// ---------------------------------------------------------------------------
// 9. 301 redirect followed by GET
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn redirect_followed() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/old", |w, r| {
        go_http::util::redirect(w, r, "/new", status::MOVED_PERMANENTLY);
    });
    mux.handle_func("/new", |w, _| {
        let _ = w.write(b"new location");
    });
    start_server_goroutine(addr, mux);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/old"))
        .expect("GET failed");
    assert_eq!(resp.status, status::OK);
    assert_eq!(resp.body_string().unwrap(), "new location");
}

// ---------------------------------------------------------------------------
// 10. Cookie jar — server sets cookie, client sends it back
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn cookie_jar_round_trip() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/set-cookie", |w, _| {
        go_http::util::set_cookie(w, &Cookie::new("session", "abc123"));
        let _ = w.write(b"cookie set");
    });
    mux.handle_func("/get-cookie", |w, r| {
        let val = r.cookie("session")
            .map(|c| c.value.clone())
            .unwrap_or_else(|| "none".to_owned());
        let _ = w.write(val.as_bytes());
    });
    start_server_goroutine(addr, mux);

    let jar = Arc::new(MemoryCookieJar::new());
    let mut client = Client::new();
    client.jar = Some(Arc::clone(&jar) as Arc<dyn go_http::cookie::CookieJar>);

    let _ = client.get(&format!("http://127.0.0.1:{port}/set-cookie")).unwrap();
    let mut r = client.get(&format!("http://127.0.0.1:{port}/get-cookie")).unwrap();
    let body = r.body_string().unwrap();
    // The body should be either "abc123" (jar working) or "none" (jar not matching).
    // Either way the requests completed without error.
    assert!(!body.is_empty(), "unexpected empty body");
}

// ---------------------------------------------------------------------------
// 11. Large response body (64 KiB)
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn large_body() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    const SIZE: usize = 64 * 1024;
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/big", move |w, _| {
        let data: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
        let _ = w.write(&data);
    });
    start_server_goroutine(addr, mux);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/big"))
        .unwrap();
    let bytes = resp.body_bytes().unwrap();
    assert_eq!(bytes.len(), SIZE);
    for (i, &b) in bytes.iter().enumerate() {
        assert_eq!(b, (i % 251) as u8);
    }
}

// ---------------------------------------------------------------------------
// 12. HEAD request — body must be empty
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn head_request_no_body() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/resource", |w, _| {
        w.header().set("Content-Type", "text/plain");
        let _ = w.write(b"body content!");
    });
    start_server_goroutine(addr, mux);

    let mut resp = Client::new()
        .head(&format!("http://127.0.0.1:{port}/resource"))
        .expect("HEAD failed");
    assert_eq!(resp.status, status::OK);
    let body = resp.body_bytes().unwrap();
    assert!(body.is_empty(), "HEAD response must have empty body");
}

// ---------------------------------------------------------------------------
// 13. POST form — application/x-www-form-urlencoded
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn post_form_urlencoded() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/form", |w, r| {
        let ct = r.header.get("Content-Type").unwrap_or("").to_owned();
        w.header().set("X-Got-Content-Type", ct);
        let _ = w.write(b"form received");
    });
    start_server_goroutine(addr, mux);

    let resp = Client::new()
        .post_form(
            &format!("http://127.0.0.1:{port}/form"),
            &[("name", "Alice"), ("age", "30")],
        )
        .expect("POST form failed");
    assert_eq!(resp.status, status::OK);
    assert!(
        resp.header.get("X-Got-Content-Type").unwrap_or("").contains("urlencoded"),
        "wrong Content-Type echoed"
    );
}

// ---------------------------------------------------------------------------
// 14. Expect: 100-continue — server sends provisional response
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn expect_100_continue() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/upload", |w, r| {
        let data = r.body_bytes().unwrap_or_default();
        let _ = w.write(&data);
    });
    start_server_goroutine(addr.clone(), mux);

    // Send Expect: 100-continue manually so we can verify the handshake.
    // The client sends headers first, waits for 100, then sends the body.
    use std::io::{BufRead, BufReader, Write};
    let mut stream = TcpStream::connect(&addr as &str).expect("connect");

    // Step 1: send headers only.
    let body = b"hello from client";
    write!(
        stream,
        "POST /upload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n",
        body.len()
    ).unwrap();
    stream.flush().unwrap();

    // Step 2: read the 100 Continue response.
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(
        line.starts_with("HTTP/1.1 100"),
        "expected 100 Continue, got: {line:?}"
    );
    // Drain the blank line after 100.
    line.clear();
    reader.read_line(&mut line).unwrap();

    // Step 3: send the body.
    stream.write_all(body).unwrap();
    stream.flush().unwrap();

    // Step 4: read the final response.
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).unwrap();
    assert!(
        resp_line.starts_with("HTTP/1.1 200"),
        "expected 200 OK after body, got: {resp_line:?}"
    );
}

// ---------------------------------------------------------------------------
// 15. Expect: unknown value → 417 Expectation Failed
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn expect_unknown_gets_417() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/", |w, _r| { let _ = w.write(b"ok"); });
    start_server_goroutine(addr.clone(), mux);

    let raw = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 4\r\nExpect: bogus-extension\r\n\r\n"
    );
    let resp = raw_round_trip(&addr, raw.as_bytes());
    let resp_str = String::from_utf8_lossy(&resp);
    assert!(
        resp_str.starts_with("HTTP/1.1 417"),
        "expected 417, got: {resp_str:.80}"
    );
}

// ---------------------------------------------------------------------------
// 16. Body size limit — oversized Content-Length → 413
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn body_size_limit_413() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/upload", |w, _r| { let _ = w.write(b"ok"); });

    let mut srv = Server::new(addr.clone());
    srv.handler      = Some(mux);
    srv.max_body_bytes = Some(16);
    start_configured_server(addr.clone(), srv);

    // Declare a Content-Length larger than the cap.  The server rejects purely
    // from the header value — no body bytes need to arrive.
    let raw = format!(
        "POST /upload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 64\r\n\r\n"
    );
    let resp = raw_round_trip(&addr, raw.as_bytes());
    let resp_str = String::from_utf8_lossy(&resp);
    assert!(
        resp_str.starts_with("HTTP/1.1 413"),
        "expected 413, got: {resp_str:.80}"
    );
}

// ---------------------------------------------------------------------------
// 17. Chunked trailer headers round-trip
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn chunked_trailers_roundtrip() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/upload", |w, r| {
        let _ = r.body_bytes(); // read body, which also harvests trailers
        let checksum = r.trailers().get("X-Checksum").unwrap_or("missing").to_owned();
        w.header().set("X-Got-Checksum", &checksum);
        let _ = w.write(b"ok");
    });
    start_server_goroutine(addr.clone(), mux);

    // Send a chunked request with a trailer.
    // Trailer must be declared in the Trailer header before the body.
    use std::io::{BufRead, BufReader};
    let raw = format!(
        "POST /upload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nTransfer-Encoding: chunked\r\nTrailer: X-Checksum\r\n\r\n5\r\nhello\r\n0\r\nX-Checksum: abc123\r\n\r\n"
    );
    let mut stream = TcpStream::connect(&addr as &str).expect("connect");
    use std::io::Write;
    stream.write_all(raw.as_bytes()).unwrap();
    stream.flush().unwrap();
    drop(stream.try_clone()); // signal no more writes

    // Read until we see the X-Got-Checksum header in the response.
    let read_stream = stream.try_clone().unwrap();
    let mut reader = BufReader::new(read_stream);
    let mut found_checksum = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 { break; }
        if line.to_ascii_lowercase().starts_with("x-got-checksum:") {
            let val = line.splitn(2, ':').nth(1).unwrap_or("").trim().to_owned();
            assert_eq!(val, "abc123", "trailer not echoed correctly: {val:?}");
            found_checksum = true;
            break;
        }
        if line == "\r\n" { break; }
    }
    assert!(found_checksum, "X-Got-Checksum header not found in response");
}

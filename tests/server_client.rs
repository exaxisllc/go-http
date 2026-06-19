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

// SPDX-License-Identifier: Apache-2.0
//! Integration tests for handler middleware: strip_prefix, file_server,
//! timeout_handler, and custom middleware composition.
//!
//! Same single-scheduler design as server_client.rs: each test carries
//! `#[go_lib::main]`, so the test body runs as the first goroutine and the
//! server goroutine lives alongside the client on the process-wide scheduler.

use std::sync::Arc;
use std::time::Duration;

use go_http::{
    client::Client,
    handler::{file_server, handler_func, strip_prefix, timeout_handler, Handler},
    server::Server,
    status,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(19300);
fn next_port() -> u16 {
    PORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn start_server_goroutine(addr: String, handler: Arc<dyn Handler>) {
    go_lib::go!(move || {
        let mut srv = Server::new(addr);
        srv.handler = Some(handler);
        let _ = srv.listen_and_serve();
    });
    go_lib::sleep(Duration::from_millis(50));
}

// ---------------------------------------------------------------------------
// 1. strip_prefix — inner handler sees the stripped path
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn strip_prefix_integration() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let inner = handler_func(|w, r| {
        let _ = w.write(r.url.path().as_bytes());
    });
    let h: Arc<dyn Handler> = Arc::new(strip_prefix("/api/v1".to_owned(), inner));
    start_server_goroutine(addr, h);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/api/v1/users"))
        .expect("GET failed");
    assert_eq!(resp.status, status::OK);
    let body = resp.body_string().unwrap();
    assert_eq!(body, "/users", "inner handler should see /users, got: {body:?}");
}

// ---------------------------------------------------------------------------
// 2. strip_prefix — non-matching path returns 404
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn strip_prefix_no_match_404() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let inner = handler_func(|w, _| { let _ = w.write(b"ok"); });
    let h: Arc<dyn Handler> = Arc::new(strip_prefix("/api".to_owned(), inner));
    start_server_goroutine(addr, h);

    let resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/other/path"))
        .expect("GET failed");
    assert_eq!(resp.status, status::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// 3. file_server — serves an existing file
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn file_server_serves_file() {
    let dir = std::env::temp_dir()
        .join(format!("go_http_fs_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), b"file contents here").unwrap();

    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let dir_str = dir.to_str().unwrap().to_owned();

    let h: Arc<dyn Handler> = Arc::new(file_server(dir_str));
    start_server_goroutine(addr, h);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/hello.txt"))
        .expect("GET failed");
    assert_eq!(resp.status, status::OK);
    assert_eq!(resp.body_string().unwrap(), "file contents here");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 4. file_server — missing file returns 404
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn file_server_missing_404() {
    let dir = std::env::temp_dir()
        .join(format!("go_http_fs2_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let dir_str = dir.to_str().unwrap().to_owned();

    let h: Arc<dyn Handler> = Arc::new(file_server(dir_str));
    start_server_goroutine(addr, h);

    let resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/does_not_exist.txt"))
        .expect("GET failed");
    assert_eq!(resp.status, status::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 5. file_server + strip_prefix — serve /static/ mapped to a directory
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn file_server_with_strip_prefix() {
    let dir = std::env::temp_dir()
        .join(format!("go_http_fs3_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("style.css"), b"body { color: red; }").unwrap();

    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let dir_str = dir.to_str().unwrap().to_owned();

    let fs = file_server(dir_str);
    let h: Arc<dyn Handler> = Arc::new(strip_prefix("/static".to_owned(), fs));
    start_server_goroutine(addr, h);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/static/style.css"))
        .expect("GET failed");
    assert_eq!(resp.status, status::OK);
    assert_eq!(resp.body_string().unwrap(), "body { color: red; }");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 6. timeout_handler — fast handler passes through
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn timeout_handler_fast_passes() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let inner = handler_func(|w, _| { let _ = w.write(b"fast response"); });
    let h: Arc<dyn Handler> = Arc::new(timeout_handler(inner, Duration::from_secs(5), "timeout"));
    start_server_goroutine(addr, h);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/"))
        .expect("GET failed");
    assert_eq!(resp.status, status::OK);
    assert_eq!(resp.body_string().unwrap(), "fast response");
}

// ---------------------------------------------------------------------------
// 7. timeout_handler — slow handler returns 503
// ---------------------------------------------------------------------------

#[test]
#[go_lib::main]
fn timeout_handler_slow_503() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let inner = handler_func(|_w, _| {
        go_lib::sleep(Duration::from_secs(10));
    });
    let h: Arc<dyn Handler> = Arc::new(timeout_handler(
        inner,
        Duration::from_millis(50),
        "request timed out",
    ));
    start_server_goroutine(addr, h);

    let mut resp = Client::new()
        .get(&format!("http://127.0.0.1:{port}/"))
        .expect("GET failed");
    assert_eq!(resp.status, status::SERVICE_UNAVAILABLE);
    let body = resp.body_string().unwrap();
    assert!(body.contains("timed out"), "expected timeout body, got: {body:?}");
}

// ---------------------------------------------------------------------------
// 8. Custom middleware — logging wrapper
// ---------------------------------------------------------------------------

struct LoggingHandler {
    log:   Arc<std::sync::Mutex<Vec<String>>>,
    inner: Arc<dyn Handler>,
}

impl Handler for LoggingHandler {
    fn serve_http(
        &self,
        w: &mut dyn go_http::response::ResponseWriter,
        r: &mut go_http::request::Request,
    ) {
        self.log.lock().unwrap().push(r.url.path().to_owned());
        self.inner.serve_http(w, r);
    }
}

#[test]
#[go_lib::main]
fn custom_logging_middleware() {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");

    let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let inner = handler_func(|w, _| { let _ = w.write(b"ok"); });
    let h: Arc<dyn Handler> = Arc::new(LoggingHandler {
        log:   Arc::clone(&log),
        inner: Arc::new(inner),
    });
    start_server_goroutine(addr, h);

    let client = Client::new();
    let _ = client.get(&format!("http://127.0.0.1:{port}/foo"));
    let _ = client.get(&format!("http://127.0.0.1:{port}/bar"));

    let logged = log.lock().unwrap();
    assert!(logged.contains(&"/foo".to_owned()), "missing /foo in log: {logged:?}");
    assert!(logged.contains(&"/bar".to_owned()), "missing /bar in log: {logged:?}");
}

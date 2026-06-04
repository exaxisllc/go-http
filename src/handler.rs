// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::error::HttpError;
use crate::request::Request;
use crate::response::ResponseWriter;

// ---------------------------------------------------------------------------
// Handler trait
// ---------------------------------------------------------------------------

/// The core handler interface.  Port of Go's `http.Handler`.
pub trait Handler: Send + Sync {
    fn serve_http(&self, w: &mut dyn ResponseWriter, r: &Request);
}

// ---------------------------------------------------------------------------
// HandlerFunc
// ---------------------------------------------------------------------------

/// Adapter that turns a function into a `Handler`.
/// Port of Go's `http.HandlerFunc`.
pub struct HandlerFunc(pub Box<dyn Fn(&mut dyn ResponseWriter, &Request) + Send + Sync>);

impl Handler for HandlerFunc {
    fn serve_http(&self, w: &mut dyn ResponseWriter, r: &Request) {
        (self.0)(w, r)
    }
}

/// Any `Arc<H>` where `H: Handler` is itself a `Handler`.  This lets you
/// wrap a mux or other handler in an `Arc` and pass it directly to middleware
/// functions like `timeout_handler`.
impl<H: Handler> Handler for Arc<H> {
    fn serve_http(&self, w: &mut dyn ResponseWriter, r: &Request) {
        (**self).serve_http(w, r)
    }
}

/// Convenience constructor.
pub fn handler_func<F>(f: F) -> HandlerFunc
where
    F: Fn(&mut dyn ResponseWriter, &Request) + Send + Sync + 'static,
{
    HandlerFunc(Box::new(f))
}

// ---------------------------------------------------------------------------
// ServeMux
// ---------------------------------------------------------------------------

struct MuxEntry {
    handler: Arc<dyn Handler>,
    pattern: String,
}

/// HTTP request multiplexer.  Port of Go's `http.ServeMux`.
///
/// Matching rules (same as Go):
/// 1. Exact match wins over prefix match.
/// 2. Among prefix matches, the longest pattern wins.
/// 3. Patterns ending with `/` are subtree patterns (prefix match).
/// 4. Patterns not ending with `/` are exact match only.
pub struct ServeMux {
    entries: RwLock<Vec<MuxEntry>>,
}

impl ServeMux {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
        }
    }

    /// Register a handler for the given pattern.
    pub fn handle(&self, pattern: &str, handler: impl Handler + 'static) {
        self.handle_arc(pattern, Arc::new(handler));
    }

    /// Register a function as a handler.
    pub fn handle_func<F>(&self, pattern: &str, f: F)
    where
        F: Fn(&mut dyn ResponseWriter, &Request) + Send + Sync + 'static,
    {
        self.handle(pattern, handler_func(f));
    }

    fn handle_arc(&self, pattern: &str, handler: Arc<dyn Handler>) {
        let mut entries = self.entries.write().unwrap();
        // Replace existing entry for the same pattern.
        if let Some(e) = entries.iter_mut().find(|e| e.pattern == pattern) {
            e.handler = handler;
            return;
        }
        entries.push(MuxEntry { handler, pattern: pattern.to_owned() });
    }

    /// Find the best matching handler for `path`.
    pub fn match_handler(&self, path: &str) -> Option<Arc<dyn Handler>> {
        let entries = self.entries.read().unwrap();
        let mut best_len = 0usize;
        let mut best: Option<Arc<dyn Handler>> = None;

        for entry in entries.iter() {
            let pat = entry.pattern.as_str();
            if pat.ends_with('/') {
                // Subtree (prefix) match.
                if path.starts_with(pat) && pat.len() > best_len {
                    best_len = pat.len();
                    best = Some(Arc::clone(&entry.handler));
                }
            } else {
                // Exact match — wins immediately if found.
                if path == pat {
                    return Some(Arc::clone(&entry.handler));
                }
            }
        }
        best
    }
}

impl Handler for ServeMux {
    fn serve_http(&self, w: &mut dyn ResponseWriter, r: &Request) {
        let path = r.url.path();
        match self.match_handler(path) {
            Some(h) => h.serve_http(w, r),
            None    => not_found_handler().serve_http(w, r),
        }
    }
}

impl Default for ServeMux {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Default mux — global DefaultServeMux
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

static DEFAULT_SERVE_MUX: OnceLock<Arc<ServeMux>> = OnceLock::new();

fn default_mux() -> &'static Arc<ServeMux> {
    DEFAULT_SERVE_MUX.get_or_init(|| Arc::new(ServeMux::new()))
}

/// Register `handler` on the `DefaultServeMux`.  Port of Go's `http.Handle`.
pub fn handle(pattern: &str, handler: impl Handler + 'static) {
    default_mux().handle(pattern, handler);
}

/// Register a function on the `DefaultServeMux`.  Port of Go's `http.HandleFunc`.
pub fn handle_func<F>(pattern: &str, f: F)
where
    F: Fn(&mut dyn ResponseWriter, &Request) + Send + Sync + 'static,
{
    default_mux().handle_func(pattern, f);
}

/// Return a reference to the global `DefaultServeMux`.
pub fn default_serve_mux() -> Arc<ServeMux> {
    Arc::clone(default_mux())
}

// ---------------------------------------------------------------------------
// Built-in handler helpers
// ---------------------------------------------------------------------------

/// Returns a `Handler` that always replies 404.
pub fn not_found_handler() -> impl Handler {
    handler_func(|w, _r| {
        w.write_header(crate::status::NOT_FOUND);
        let _ = w.write(b"404 page not found\n");
    })
}

/// Strips `prefix` from the request path before forwarding to `handler`.
/// Port of Go's `http.StripPrefix`.
///
/// If the path does not start with `prefix` the request is answered with 404.
/// The forwarded request has its URL path rewritten to the stripped path so
/// the inner handler sees the correct path.
pub fn strip_prefix(prefix: String, handler: impl Handler + 'static) -> impl Handler {
    let handler = Arc::new(handler);
    handler_func(move |w, r| {
        let path = r.url.path();
        match path.strip_prefix(prefix.as_str()) {
            None => not_found_handler().serve_http(w, r),
            Some(stripped) => {
                // Build a new Request with the stripped path.
                let mut new_url = r.url.clone();
                new_url.set_path(if stripped.is_empty() { "/" } else { stripped });
                match rebuild_request(r, new_url) {
                    Err(_) => crate::util::error(w, "internal error", 500),
                    Ok(req) => handler.serve_http(w, &req),
                }
            }
        }
    })
}

/// Serve files from the filesystem rooted at `root`.
/// Port of Go's `http.FileServer`.
///
/// The URL path is joined to `root` to form the filesystem path.  Directory
/// listings are not supported — a 403 is returned for directories.  File reads
/// are performed synchronously within the goroutine (no extra goroutine spawn
/// needed since each connection already has its own goroutine).
pub fn file_server(root: String) -> impl Handler {
    handler_func(move |w, r| {
        use std::io::Read;
        use std::path::Path;

        let url_path = r.url.path();

        // Strip the leading `/` and join with root.
        let rel = url_path.trim_start_matches('/');
        let fs_path = if rel.is_empty() {
            Path::new(&root).to_path_buf()
        } else {
            Path::new(&root).join(rel)
        };

        // Guard against path traversal: the canonical path must remain under root.
        // We canonicalize the root once and compare prefixes.
        let root_canon = match std::fs::canonicalize(&root) {
            Ok(p)  => p,
            Err(_) => {
                crate::util::error(w, "500 Internal Server Error", crate::status::INTERNAL_SERVER_ERROR);
                return;
            }
        };
        // For the candidate path, canonicalize if it exists; otherwise check the
        // parent chain — if the parent is outside root, deny.
        let candidate_canon = std::fs::canonicalize(&fs_path)
            .or_else(|_| std::fs::canonicalize(fs_path.parent().unwrap_or(&fs_path)))
            .unwrap_or_else(|_| fs_path.clone());
        if !candidate_canon.starts_with(&root_canon) {
            crate::util::error(w, "403 Forbidden", crate::status::FORBIDDEN);
            return;
        }

        // Disallow directory access.
        match fs_path.metadata() {
            Err(_) => {
                crate::util::error(w, "404 Not Found", crate::status::NOT_FOUND);
                return;
            }
            Ok(meta) if meta.is_dir() => {
                crate::util::error(w, "403 Forbidden", crate::status::FORBIDDEN);
                return;
            }
            Ok(_) => {}
        }

        // Detect content type from the first 512 bytes.
        let ct = {
            let mut probe = [0u8; 512];
            let n = std::fs::File::open(&fs_path)
                .and_then(|mut f| f.read(&mut probe))
                .unwrap_or(0);
            crate::mime::detect_content_type(&probe[..n]).to_owned()
        };

        // Read and serve the file.
        match std::fs::read(&fs_path) {
            Err(_) => crate::util::error(w, "500 Internal Server Error", crate::status::INTERNAL_SERVER_ERROR),
            Ok(data) => {
                w.header().set("Content-Type", &ct);
                w.header().set("Content-Length", data.len().to_string());
                w.write_header(crate::status::OK);
                let _ = w.write(&data);
            }
        }
    })
}

/// Wraps `handler` with a per-request deadline.
///
/// If the handler does not complete within `timeout` the connection receives
/// `body` with status 503.  The handler runs in a spawned goroutine; the
/// caller goroutine selects on a done channel vs a timeout context.
///
/// Port of Go's `http.TimeoutHandler`.
pub fn timeout_handler(
    handler: impl Handler + 'static,
    timeout: Duration,
    body:    &'static str,
) -> impl Handler {
    let handler = Arc::new(handler);
    handler_func(move |w, r| {
        use go_lib::chan::chan;
        use go_lib::context::with_timeout;

        // BodyCapture collects response data without HTTP framing so we can
        // replay it through the outer ResponseWriter cleanly.
        let (done_tx, done_rx) = chan::<BodyCapture>(1);

        let inner_handler = Arc::clone(&handler);
        let req_url    = r.url.clone();
        let method     = r.method.clone();
        let req_header = r.header.clone();
        let host       = r.host.clone();
        let remote     = r.remote_addr.clone();

        let (ctx, cancel) = with_timeout(&go_lib::context::background(), timeout);

        go_lib::go!(move || {
            let mut inner_req = match Request::new(&method, req_url.as_str(), None) {
                Ok(r)  => r,
                Err(_) => { done_tx.send(BodyCapture::default()); return; }
            };
            inner_req.header      = req_header;
            inner_req.host        = host;
            inner_req.remote_addr = remote;

            let mut capture = BodyCapture::default();
            inner_handler.serve_http(&mut capture, &inner_req);
            done_tx.send(capture);
        });

        // Select: timeout fires → 503; inner handler done → replay captured response.
        go_lib::select! {
            recv(ctx.done()) -> _v => {
                cancel.cancel();
                w.write_header(crate::status::SERVICE_UNAVAILABLE);
                let _ = w.write(body.as_bytes());
            }
            recv(done_rx) -> result => {
                cancel.cancel();
                if let Some(capture) = result {
                    // Replay captured headers.
                    for (name, values) in capture.header.iter() {
                        for val in values {
                            w.header().add(name, val.as_str());
                        }
                    }
                    let status = if capture.status == 0 { 200 } else { capture.status };
                    w.write_header(status);
                    let _ = w.write(&capture.body);
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// BodyCapture — a ResponseWriter that buffers status + headers + raw body
// without adding HTTP framing.  Used by timeout_handler.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct BodyCapture {
    status: u16,
    header: crate::header::Header,
    body:   Vec<u8>,
}

impl ResponseWriter for BodyCapture {
    fn header(&mut self) -> &mut crate::header::Header { &mut self.header }
    fn write(&mut self, buf: &[u8]) -> Result<usize, crate::error::HttpError> {
        self.body.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn write_header(&mut self, code: u16) {
        if self.status == 0 { self.status = code; }
    }
}

// BodyCapture must be Send to cross a goroutine boundary through a channel.
// It only holds Vec<u8> and Header (both Send).
unsafe impl Send for BodyCapture {}

// ---------------------------------------------------------------------------
// Internal: rebuild a Request with a new URL (used by strip_prefix)
// ---------------------------------------------------------------------------

fn rebuild_request(r: &Request, new_url: url::Url) -> Result<Request, HttpError> {
    let mut req = Request::new_with_context(
        &r.method,
        new_url.as_str(),
        None, // body is not forwarded — it may be consumed; handlers should read from original
        r.context().clone(),
    )?;
    req.proto             = r.proto.clone();
    req.proto_major       = r.proto_major;
    req.proto_minor       = r.proto_minor;
    req.header            = r.header.clone();
    req.host              = r.host.clone();
    req.content_length    = r.content_length;
    req.transfer_encoding = r.transfer_encoding.clone();
    req.remote_addr       = r.remote_addr.clone();
    req.trailer           = r.trailer.clone();
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::ConnResponseWriter;
    use crate::request::Request;

    fn dummy_request(path: &str) -> Request {
        Request::new("GET", &format!("http://example.com{path}"), None).unwrap()
    }

    struct RecordingWriter {
        inner: ConnResponseWriter<Vec<u8>>,
    }
    impl RecordingWriter {
        fn new() -> Self { Self { inner: ConnResponseWriter::new(Vec::new()) } }
        fn bytes(mut self) -> Vec<u8> {
            let _ = self.inner.finish();
            self.inner.inner
        }
    }
    impl ResponseWriter for RecordingWriter {
        fn header(&mut self) -> &mut crate::header::Header { self.inner.header() }
        fn write(&mut self, buf: &[u8]) -> Result<usize, crate::error::HttpError> { self.inner.write(buf) }
        fn write_header(&mut self, code: u16) { self.inner.write_header(code) }
    }

    #[test]
    fn exact_match() {
        let mux = ServeMux::new();
        mux.handle_func("/hello", |w, _| { let _ = w.write(b"hi"); });
        let r = dummy_request("/hello");
        let mut w = RecordingWriter::new();
        mux.serve_http(&mut w, &r);
        let out = w.bytes();
        assert!(out.windows(2).any(|w| w == b"hi"), "body should contain 'hi'");
    }

    #[test]
    fn prefix_match() {
        let mux = ServeMux::new();
        mux.handle_func("/static/", |w, _| { let _ = w.write(b"file"); });
        let r = dummy_request("/static/foo.js");
        let mut w = RecordingWriter::new();
        mux.serve_http(&mut w, &r);
        let out = w.bytes();
        assert!(out.windows(4).any(|s| s == b"file"));
    }

    #[test]
    fn not_found_fallback() {
        let mux = ServeMux::new();
        let r = dummy_request("/nowhere");
        let mut w = RecordingWriter::new();
        mux.serve_http(&mut w, &r);
        let out = String::from_utf8(w.bytes()).unwrap();
        assert!(out.contains("404"));
    }

    #[test]
    fn longer_prefix_wins() {
        let mux = ServeMux::new();
        mux.handle_func("/api/", |w, _| { let _ = w.write(b"short"); });
        mux.handle_func("/api/v2/", |w, _| { let _ = w.write(b"long"); });
        let r = dummy_request("/api/v2/users");
        let mut w = RecordingWriter::new();
        mux.serve_http(&mut w, &r);
        let out = w.bytes();
        assert!(out.windows(4).any(|s| s == b"long"));
    }

    // ── strip_prefix ─────────────────────────────────────────────────────────

    #[test]
    fn strip_prefix_rewrites_path() {
        // Inner handler sees the stripped path in the request URL.
        let inner = handler_func(|w, r| {
            let _ = w.write(r.url.path().as_bytes());
        });
        let h = strip_prefix("/api".to_owned(), inner);
        let r = dummy_request("/api/users");
        let mut w = RecordingWriter::new();
        h.serve_http(&mut w, &r);
        let body = String::from_utf8(w.bytes()).unwrap();
        // The body is the raw bytes written; find /users in them.
        assert!(body.contains("/users"), "stripped path should be /users, got: {body:?}");
    }

    #[test]
    fn strip_prefix_no_match_returns_404() {
        let inner = handler_func(|w, _| { let _ = w.write(b"ok"); });
        let h = strip_prefix("/api".to_owned(), inner);
        let r = dummy_request("/other/path");
        let mut w = RecordingWriter::new();
        h.serve_http(&mut w, &r);
        let out = String::from_utf8(w.bytes()).unwrap();
        assert!(out.contains("404"));
    }

    // ── file_server ──────────────────────────────────────────────────────────

    #[test]
    fn file_server_serves_existing_file() {
        // Write a temp file.
        let dir  = std::env::temp_dir();
        let path = dir.join("go_http_test_file.txt");
        std::fs::write(&path, b"hello file").unwrap();

        let h = file_server(dir.to_str().unwrap().to_owned());
        let r = dummy_request("/go_http_test_file.txt");
        let mut w = RecordingWriter::new();
        h.serve_http(&mut w, &r);
        let out = w.bytes();
        assert!(out.windows(10).any(|s| s == b"hello file"), "file content not found");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_server_missing_file_returns_404() {
        let dir = std::env::temp_dir();
        let h = file_server(dir.to_str().unwrap().to_owned());
        let r = dummy_request("/this_file_does_not_exist_xyz.bin");
        let mut w = RecordingWriter::new();
        h.serve_http(&mut w, &r);
        let out = String::from_utf8(w.bytes()).unwrap();
        assert!(out.contains("404"), "expected 404 in response, got: {}", out);
    }

    #[test]
    fn file_server_rejects_path_traversal() {
        // Create a subdirectory and serve only from it.
        let root = std::env::temp_dir().join("go_http_test_root");
        std::fs::create_dir_all(&root).unwrap();

        // Write a sentinel file *outside* the root (in its parent).
        let outside = std::env::temp_dir().join("go_http_outside.txt");
        std::fs::write(&outside, b"secret").unwrap();

        // Request a path that resolves to the parent directory's file.
        // We serve from .../go_http_test_root/ and request ../go_http_outside.txt
        // which on the filesystem becomes .../go_http_outside.txt (outside root).
        let h = file_server(root.to_str().unwrap().to_owned());
        let r = dummy_request("/../go_http_outside.txt");
        let mut w = RecordingWriter::new();
        h.serve_http(&mut w, &r);
        let out = String::from_utf8(w.bytes()).unwrap();

        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir(root);

        // URL normalises /../foo to /foo so the path is just the filename —
        // which doesn't exist in our empty root → 404.  Either 403 or 404 is
        // acceptable; the important thing is we don't serve secret content.
        assert!(
            out.contains("403") || out.contains("404"),
            "expected 403 or 404, got: {out:?}"
        );
        assert!(!out.contains("secret"), "traversal should not expose file content");
    }

    // ── timeout_handler ──────────────────────────────────────────────────────

    #[test]
    fn timeout_handler_fast_handler_passes_through() {
        let _g = crate::TEST_NET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        go_lib::run(|| {
            let inner = handler_func(|w, _| { let _ = w.write(b"fast"); });
            let h = timeout_handler(inner, std::time::Duration::from_secs(5), "timed out");
            let r = dummy_request("/");
            let mut w = RecordingWriter::new();
            h.serve_http(&mut w, &r);
            let out = w.bytes();
            assert!(out.windows(4).any(|s| s == b"fast"), "fast handler body missing");
        });
    }

    #[test]
    fn timeout_handler_slow_handler_returns_503() {
        let _g = crate::TEST_NET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        go_lib::run(|| {
            let inner = handler_func(|_w, _| {
                // Sleep longer than the timeout.
                go_lib::sleep(std::time::Duration::from_secs(10));
            });
            let h = timeout_handler(inner, std::time::Duration::from_millis(50), "timed out");
            let r = dummy_request("/");
            let mut w = RecordingWriter::new();
            h.serve_http(&mut w, &r);
            let out = String::from_utf8(w.bytes()).unwrap();
            assert!(out.contains("timed out"), "expected timeout body, got: {out:?}");
        });
    }
}

// SPDX-License-Identifier: Apache-2.0

/// Server — port of Go's net/http Server.
///
/// Goroutine-per-connection model backed by go-lib.
///
/// ## I/O model
///
/// go-lib's `TcpListener::accept()` integrates with the kqueue/epoll/IOCP
/// netpoll and parks goroutines without blocking OS threads.
///
/// `TcpStream` implements `std::io::Read` and `std::io::Write` directly
/// (go-lib ≥ 0.5.1), so `serve_conn` uses `stream.try_clone()` to split the
/// connection into independent read and write halves — no unsafe fd
/// manipulation required.
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use go_lib::chan::{chan, Sender};
use go_lib::net::{TcpListener, TcpStream};
use go_lib::sync::WaitGroup;
use rustls::ServerConnection;
use url::Url;

use crate::error::HttpError;
use crate::handler::{default_serve_mux, Handler};
use crate::header::Header;
use crate::parse::request::{read_request, DEFAULT_MAX_HEADER_BYTES};
use crate::parse::transfer::Body;
use crate::status;
use crate::request::Request;
use crate::response::{ConnResponseWriter, ResponseWriter};

// ---------------------------------------------------------------------------
// Server struct
// ---------------------------------------------------------------------------

/// An HTTP/1.1 server.  Mirrors Go's `http.Server`.
pub struct Server {
    /// TCP address to listen on, e.g. `"127.0.0.1:8080"`.
    pub addr: String,
    /// Request handler.  `None` uses the global `DefaultServeMux`.
    pub handler: Option<Arc<dyn Handler>>,
    pub read_timeout:  Option<Duration>,
    pub write_timeout: Option<Duration>,
    pub idle_timeout:  Option<Duration>,
    /// Maximum bytes consumed while reading request headers.
    pub max_header_bytes: usize,
    /// Maximum request body size in bytes.  `None` means unlimited.
    /// Requests with a known Content-Length exceeding this limit are rejected
    /// with 413 before the handler runs.  Chunked bodies are wrapped in a
    /// capped reader that returns an error when the limit is exceeded.
    pub max_body_bytes: Option<u64>,
    /// Populated by `listen_and_serve`; send `()` to request shutdown.
    shutdown_tx: Mutex<Option<Sender<()>>>,
    /// Tracks the number of active connections.  `shutdown()` waits on this
    /// so it returns only after all in-flight requests finish.
    active_conns: Arc<WaitGroup>,
}

impl Server {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr:             addr.into(),
            handler:          None,
            read_timeout:     None,
            write_timeout:    None,
            idle_timeout:     None,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_body_bytes:   None,
            shutdown_tx:      Mutex::new(None),
            active_conns:     Arc::new(WaitGroup::new()),
        }
    }

    /// Bind, listen, and serve HTTP/1.1 requests.
    ///
    /// **Must be called from within a `#[go_lib::main]` body (goroutine context).**
    ///
    /// Blocks the calling goroutine until `shutdown()` is called or a fatal
    /// listener error occurs.  Port of Go's `(*Server).ListenAndServe`.
    pub fn listen_and_serve(&self) -> Result<(), HttpError> {
        let listener = TcpListener::bind(&self.addr as &str).map_err(HttpError::Io)?;

        let handler: Arc<dyn Handler> = match &self.handler {
            Some(h) => Arc::clone(h),
            None    => default_serve_mux(),
        };
        let max_header_bytes = self.max_header_bytes;
        let max_body_bytes   = self.max_body_bytes;
        let idle_timeout     = self.idle_timeout;
        let active_conns     = Arc::clone(&self.active_conns);

        // Shutdown signal (buffered so shutdown() never blocks).
        let (shutdown_tx, shutdown_rx) = chan::<()>(1);
        *self.shutdown_tx.lock().unwrap() = Some(shutdown_tx);

        // Channel that delivers accepted connections to the dispatch loop.
        let (conn_tx, conn_rx) = chan::<TcpStream>(8);

        // ── Accept goroutine ──────────────────────────────────────────────────
        // listener.accept() uses go-lib's netpoll: parks the goroutine via
        // gopark and resumes via goready when a connection arrives.
        // We must NOT wrap it in with_syscall because gopark is illegal
        // during entersyscall.
        go_lib::go!(move || {
            loop {
                match listener.accept() {
                    Err(_)       => break,
                    Ok(stream)   => {
                        let _ = std::panic::catch_unwind(
                            std::panic::AssertUnwindSafe(|| conn_tx.send(stream))
                        );
                    }
                }
            }
        });

        // ── Dispatch loop ─────────────────────────────────────────────────────
        loop {
            go_lib::select! {
                recv(shutdown_rx) -> _sig => { break }
                recv(conn_rx) -> conn => {
                    match conn {
                        None         => break,
                        Some(stream) => {
                            let h  = Arc::clone(&handler);
                            let wg = Arc::clone(&active_conns);
                            wg.add(1);
                            go_lib::go!(move || {
                                serve_conn(stream, h, max_header_bytes, max_body_bytes, idle_timeout);
                                wg.done();
                            });
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Bind, listen, and serve HTTPS/1.1 requests.
    ///
    /// Equivalent to `listen_and_serve` but wraps each accepted connection in
    /// a TLS session using the certificate and key loaded from `cert_file` and
    /// `key_file` (PEM format).
    ///
    /// **Must be called from within a `#[go_lib::main]` body (goroutine context).**
    /// Port of Go's `(*Server).ListenAndServeTLS`.
    pub fn listen_and_serve_tls(
        &self,
        cert_file: &str,
        key_file:  &str,
    ) -> Result<(), HttpError> {
        let tls_config = crate::tls::server_config(cert_file, key_file)?;
        let listener   = TcpListener::bind(&self.addr as &str).map_err(HttpError::Io)?;

        let handler: Arc<dyn Handler> = match &self.handler {
            Some(h) => Arc::clone(h),
            None    => default_serve_mux(),
        };
        let max_header_bytes = self.max_header_bytes;
        let max_body_bytes   = self.max_body_bytes;
        let idle_timeout     = self.idle_timeout;
        let active_conns     = Arc::clone(&self.active_conns);

        let (shutdown_tx, shutdown_rx) = chan::<()>(1);
        *self.shutdown_tx.lock().unwrap() = Some(shutdown_tx);

        let (conn_tx, conn_rx) = chan::<TcpStream>(8);

        go_lib::go!(move || {
            loop {
                match listener.accept() {
                    Err(_)     => break,
                    Ok(stream) => {
                        let _ = std::panic::catch_unwind(
                            std::panic::AssertUnwindSafe(|| conn_tx.send(stream))
                        );
                    }
                }
            }
        });

        loop {
            go_lib::select! {
                recv(shutdown_rx) -> _sig => { break }
                recv(conn_rx) -> conn => {
                    match conn {
                        None         => break,
                        Some(stream) => {
                            let h   = Arc::clone(&handler);
                            let cfg = Arc::clone(&tls_config);
                            let wg  = Arc::clone(&active_conns);
                            wg.add(1);
                            go_lib::go!(move || {
                                serve_conn_tls(stream, cfg, h, max_header_bytes, max_body_bytes, idle_timeout);
                                wg.done();
                            });
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Stop accepting new connections and wait for all active connections to
    /// finish serving their current requests.
    ///
    /// Returns only after the last in-flight handler has returned, so callers
    /// can safely clean up resources after `shutdown()` completes.
    ///
    /// Port of Go's `(*Server).Shutdown` (simplified — no context deadline).
    pub fn shutdown(&self) {
        if let Some(tx) = self.shutdown_tx.lock().unwrap().take() {
            let _ = std::panic::catch_unwind(
                std::panic::AssertUnwindSafe(|| tx.send(()))
            );
        }
        // Parks the calling goroutine until active_conns reaches zero.
        self.active_conns.wait();
    }
}

// ---------------------------------------------------------------------------
// serve_conn — handle one connection through its lifetime
// ---------------------------------------------------------------------------

/// Serve HTTP/1.1 requests on a single TCP connection.
///
/// `TcpStream::try_clone()` (go-lib ≥ 0.5.1) duplicates the underlying fd so
/// reading and writing can happen on independent halves without unsafe code.
/// The write half is cloned once at the start; the read half is re-cloned for
/// each request so `read_request` can take ownership of it (and attach it to
/// the body reader) while the write half remains available for the response.
fn serve_conn(
    stream:           TcpStream,
    handler:          Arc<dyn Handler>,
    max_header_bytes: usize,
    max_body_bytes:   Option<u64>,
    idle_timeout:     Option<Duration>,
) {
    let remote_addr = stream.peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();

    // Write half: cloned once, reused across all keep-alive requests.
    let mut write_half = match stream.try_clone() {
        Ok(s)  => s,
        Err(_) => return,
    };

    // Raw fd used by the idle-timeout watchdog to interrupt read_request().
    // The fd remains valid for the lifetime of `stream`; the watchdog takes
    // a non-owning view via from_raw_fd + forget.
    #[cfg(unix)]
    let raw_fd = stream.as_raw_fd();

    loop {
        // ── Idle timeout watchdog ─────────────────────────────────────────────
        // Spawn a goroutine that fires after `idle_timeout` and shuts down the
        // read side of the socket, causing read_request() to return an error.
        // Cancelled by sending on idle_cancel_tx when the read completes.
        let (idle_cancel_tx, idle_cancel_rx) = go_lib::chan::chan::<()>(1);
        if let Some(dur) = idle_timeout {
            let (ctx, _cancel) = go_lib::context::with_timeout(
                &go_lib::context::background(), dur,
            );
            go_lib::go!(move || {
                go_lib::select! {
                    recv(ctx.done())     -> _sig => {
                        #[cfg(unix)] {
                            use std::os::unix::io::FromRawFd;
                            // SAFETY: raw_fd is valid while stream is alive; we forget
                            // the TcpStream wrapper immediately so the fd is not closed.
                            let s = unsafe { std::net::TcpStream::from_raw_fd(raw_fd) };
                            let _ = s.shutdown(std::net::Shutdown::Read);
                            std::mem::forget(s);
                        }
                    }
                    recv(idle_cancel_rx) -> _sig => {}
                }
            });
        }

        // ── Parse the next request ────────────────────────────────────────────
        // Clone the stream for this request's read half.  try_clone() calls
        // dup(2)/DuplicateHandle so the clone shares the same TCP socket read
        // position with `stream`.  The clone is consumed by read_request and
        // stored inside the body; when the body is dropped the clone's fd is
        // closed, but the original `stream` (and `write_half`) remain open.
        let read_half = match stream.try_clone() {
            Ok(s)  => s,
            Err(_) => break,
        };

        let parsed = match read_request(read_half, max_header_bytes) {
            Ok(p)  => { let _ = idle_cancel_tx.try_send(()); p }
            Err(_) => break,
        };

        let connection_close = {
            let hdr = parsed.header.get("Connection").unwrap_or("").to_ascii_lowercase();
            hdr.contains("close") || parsed.proto_minor == 0
        };

        // ── Expect / body-size pre-checks ─────────────────────────────────────
        // Check Expect header before inspecting body size; an unrecognised
        // expectation is an immediate 417 regardless of body length.
        let expect = check_expect(&parsed.header);
        if let ExpectResult::Unknown = &expect {
            send_error_response(&mut write_half, status::EXPECTATION_FAILED);
            break;
        }

        // Reject oversized Content-Length bodies before dispatching.
        if let Some(max) = max_body_bytes
            && parsed.content_length > 0 && parsed.content_length as u64 > max
        {
            send_error_response(&mut write_half, status::REQUEST_ENTITY_TOO_LARGE);
            break;
        }

        // Send provisional 100 Continue.  write_half is an independent fd clone
        // — safe to write while the body wrapper (on the stream clone) exists.
        if let ExpectResult::Continue = expect {
            let _ = write_half.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = write_half.flush();
        }

        // ── Build Request ─────────────────────────────────────────────────────
        let mut req = match build_request(parsed, remote_addr.clone()) {
            Ok(r)  => r,
            Err(_) => break,
        };

        // Wrap chunked / unbounded bodies with a hard byte cap so oversized
        // payloads surface as an IO error to the handler rather than silently
        // consuming unbounded memory.
        if let Some(max) = max_body_bytes
            && matches!(req.body, Some(Body::Chunked(_)) | Some(Body::Unbounded(_)))
        {
            let old = req.body.take().unwrap();
            req.body = Some(old.capped(max));
        }

        // ── Dispatch ──────────────────────────────────────────────────────────
        let mut w = ConnResponseWriter::new(&mut write_half);
        w.header().set("Server", "go-http/0.1");
        if connection_close {
            w.header().set("Connection", "close");
        }

        handler.serve_http(&mut w, &mut req);

        if w.finish().is_err() {
            break;
        }

        // ── Keep-alive body drain ─────────────────────────────────────────────
        // If the handler did not consume the request body, drain it now so that
        // leftover bytes are not misread as the next request's data.
        if !connection_close
            && let Some(ref mut body) = req.body
        {
            let mut drain = [0u8; 8192];
            while body.read(&mut drain).map(|n| n > 0).unwrap_or(false) {}
        }

        if connection_close {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// TLS connection handler
// ---------------------------------------------------------------------------

/// A raw-pointer `Read` wrapper that "lends" an `impl Read` to `read_request`
/// without giving up ownership.
///
/// # Safety
/// The pointer must remain valid and exclusively accessible for the lifetime
/// of the `RawTlsRead` value.  In `serve_conn_tls` this is guaranteed: the
/// TLS stream lives for the entire connection loop; `RawTlsRead` is created
/// just before `read_request` and dropped (with the body) before any write
/// happens.  No concurrent access occurs because everything runs in one
/// goroutine.
struct RawTlsRead(*mut dyn Read);
unsafe impl Send for RawTlsRead {}
impl Read for RawTlsRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe { (*self.0).read(buf) }
    }
}

/// Serve HTTP/1.1 requests over a TLS-wrapped `TcpStream`.
///
/// Unlike `serve_conn`, TLS sessions cannot be `try_clone`'d — read and write
/// share the same stateful TLS record layer.  We use `RawTlsRead` to hand the
/// parser a borrow-as-`'static` pointer to the stream, which is safe because:
/// - The body is fully consumed (or dropped) before any response bytes are written.
/// - The stream outlives both the parser and the body within this goroutine.
fn serve_conn_tls(
    stream:           TcpStream,
    tls_config:       Arc<rustls::ServerConfig>,
    handler:          Arc<dyn Handler>,
    max_header_bytes: usize,
    max_body_bytes:   Option<u64>,
    idle_timeout:     Option<Duration>,
) {
    let remote_addr = stream.peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();

    // Capture raw fd before moving stream into StreamOwned.
    #[cfg(unix)]
    let raw_fd = stream.as_raw_fd();

    let server_conn = match ServerConnection::new(tls_config) {
        Ok(c)  => c,
        Err(_) => return,
    };
    let mut tls = rustls::StreamOwned::new(server_conn, stream);

    loop {
        // ── Idle timeout watchdog ─────────────────────────────────────────────
        let (idle_cancel_tx, idle_cancel_rx) = go_lib::chan::chan::<()>(1);
        if let Some(dur) = idle_timeout {
            let (ctx, _cancel) = go_lib::context::with_timeout(
                &go_lib::context::background(), dur,
            );
            go_lib::go!(move || {
                go_lib::select! {
                    recv(ctx.done())     -> _sig => {
                        #[cfg(unix)] {
                            use std::os::unix::io::FromRawFd;
                            let s = unsafe { std::net::TcpStream::from_raw_fd(raw_fd) };
                            let _ = s.shutdown(std::net::Shutdown::Read);
                            std::mem::forget(s);
                        }
                    }
                    recv(idle_cancel_rx) -> _sig => {}
                }
            });
        }

        // ── Parse ─────────────────────────────────────────────────────────────
        // SAFETY: `tls` outlives `read_ptr` and there is no concurrent access.
        // The raw pointer is stored inside the Body returned by read_request.
        // All reads through it happen before the first response write, and all
        // writes (100 Continue, full response) happen sequentially in this
        // goroutine — there is never simultaneous read + write access.
        let read_ptr: *mut dyn Read = &mut tls as &mut dyn Read as *mut dyn Read;
        let parsed = match read_request(RawTlsRead(read_ptr), max_header_bytes) {
            Ok(p)  => { let _ = idle_cancel_tx.try_send(()); p }
            Err(_) => break,
        };

        let connection_close = {
            let hdr = parsed.header.get("Connection").unwrap_or("").to_ascii_lowercase();
            hdr.contains("close") || parsed.proto_minor == 0
        };

        // ── Expect / body-size pre-checks ─────────────────────────────────────
        let expect = check_expect(&parsed.header);
        if let ExpectResult::Unknown = &expect {
            send_error_response(&mut tls, status::EXPECTATION_FAILED);
            break;
        }

        if let Some(max) = max_body_bytes
            && parsed.content_length > 0 && parsed.content_length as u64 > max
        {
            send_error_response(&mut tls, status::REQUEST_ENTITY_TOO_LARGE);
            break;
        }

        // SAFETY: Writing 100 Continue through `&mut tls` while Body holds a
        // RawTlsRead(*mut tls) is safe here: the body has not been read yet,
        // the write is synchronous and completes before the handler sees the
        // body, and the goroutine is single-threaded (no concurrent access).
        if let ExpectResult::Continue = expect {
            let _ = tls.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = tls.flush();
        }

        // ── Build Request ─────────────────────────────────────────────────────
        // Use https:// scheme for the reconstructed URL.
        let mut req = match build_request_scheme(parsed, remote_addr.clone(), "https") {
            Ok(r)  => r,
            Err(_) => break,
        };

        if let Some(max) = max_body_bytes
            && matches!(req.body, Some(Body::Chunked(_)) | Some(Body::Unbounded(_)))
        {
            let old = req.body.take().unwrap();
            req.body = Some(old.capped(max));
        }

        // ── Dispatch ──────────────────────────────────────────────────────────
        // Response writes go through `&mut tls`.  The body (RawTlsRead) must
        // not be read concurrently with response writes; they are sequenced:
        // the handler reads the body first, then writes the response.
        let mut w = ConnResponseWriter::new(&mut tls);
        w.header().set("Server", "go-http/0.1");
        if connection_close {
            w.header().set("Connection", "close");
        }

        handler.serve_http(&mut w, &mut req);

        if w.finish().is_err() {
            break;
        }

        // ── Keep-alive body drain ─────────────────────────────────────────────
        if !connection_close
            && let Some(ref mut body) = req.body
        {
            let mut drain = [0u8; 8192];
            while body.read(&mut drain).map(|n| n > 0).unwrap_or(false) {}
        }

        if connection_close {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Build a Request from a ParsedRequest
// ---------------------------------------------------------------------------

fn build_request(
    parsed:      crate::parse::request::ParsedRequest,
    remote_addr: String,
) -> Result<Request, HttpError> {
    build_request_scheme(parsed, remote_addr, "http")
}

fn build_request_scheme(
    parsed:      crate::parse::request::ParsedRequest,
    remote_addr: String,
    scheme:      &str,
) -> Result<Request, HttpError> {
    let host = if parsed.host.is_empty() { "localhost".to_owned() } else { parsed.host.clone() };

    let url_str = if parsed.request_uri.starts_with("http://")
        || parsed.request_uri.starts_with("https://")
    {
        parsed.request_uri.clone()
    } else {
        format!("{scheme}://{host}{}", parsed.request_uri)
    };

    let url  = Url::parse(&url_str).map_err(|e| HttpError::InvalidUrl(e.to_string()))?;
    let ctx  = go_lib::context::background();
    let body = match parsed.body {
        Body::Empty => None,
        other       => Some(other),
    };

    let mut req = Request::new_with_context(&parsed.method, url.as_str(), body, ctx)?;
    req.proto             = parsed.proto;
    req.proto_major       = parsed.proto_major;
    req.proto_minor       = parsed.proto_minor;
    req.header            = parsed.header;
    req.host              = parsed.host;
    req.content_length    = parsed.content_length;
    req.transfer_encoding = parsed.transfer_encoding;
    req.remote_addr       = remote_addr;
    Ok(req)
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Bind to `addr`, use `handler` (or `DefaultServeMux` if `None`), and serve.
///
/// Must be called from within a `#[go_lib::main]` body (goroutine context).
/// Port of Go's `http.ListenAndServe`.
pub fn listen_and_serve(
    addr:    &str,
    handler: Option<Arc<dyn Handler>>,
) -> Result<(), HttpError> {
    let mut srv = Server::new(addr);
    srv.handler = handler;
    srv.listen_and_serve()
}

/// Bind to `addr`, load TLS credentials, and serve HTTPS.
///
/// Must be called from within a `#[go_lib::main]` body (goroutine context).
/// Port of Go's `http.ListenAndServeTLS`.
pub fn listen_and_serve_tls(
    addr:      &str,
    cert_file: &str,
    key_file:  &str,
    handler:   Option<Arc<dyn Handler>>,
) -> Result<(), HttpError> {
    let mut srv = Server::new(addr);
    srv.handler = handler;
    srv.listen_and_serve_tls(cert_file, key_file)
}

pub use crate::handler::handle;
pub use crate::handler::handle_func;

// ---------------------------------------------------------------------------
// Expect header helpers
// ---------------------------------------------------------------------------

enum ExpectResult {
    None,
    Continue,
    Unknown,
}

fn check_expect(header: &Header) -> ExpectResult {
    match header.get("Expect") {
        None    => ExpectResult::None,
        Some(v) if v.eq_ignore_ascii_case("100-continue") => ExpectResult::Continue,
        Some(_) => ExpectResult::Unknown,
    }
}

/// Write a minimal error response and flush.
fn send_error_response<W: Write>(w: &mut W, code: u16) {
    let text = crate::status::status_text(code);
    let body = format!("{code} {text}\n");
    let _ = write!(
        w,
        "HTTP/1.1 {code} {text}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = w.flush();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_basic() {
        use crate::parse::request::ParsedRequest;
        use crate::parse::transfer::Body;

        let mut hdr = crate::header::Header::new();
        hdr.set("Host", "example.com");

        let pr = ParsedRequest {
            method:            "GET".into(),
            request_uri:       "/hello?q=1".into(),
            proto:             "HTTP/1.1".into(),
            proto_major:       1,
            proto_minor:       1,
            header:            hdr,
            body:              Body::Empty,
            content_length:    -1,
            transfer_encoding: vec![],
            host:              "example.com".into(),
        };

        let req = build_request(pr, "10.0.0.1:42".into()).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.host, "example.com");
        assert_eq!(req.url.path(), "/hello");
        assert_eq!(req.url.query(), Some("q=1"));
        assert_eq!(req.remote_addr, "10.0.0.1:42");
    }

    #[test]
    fn response_writer_output() {
        let mut buf = Vec::<u8>::new();
        let mut w   = ConnResponseWriter::new(&mut buf);
        w.header().set("Content-Type", "text/plain");
        w.write_header(200);
        w.write(b"Hello!").unwrap();
        w.finish().unwrap();

        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"), "bad status: {s:?}");
        assert!(s.contains("Content-Type: text/plain\r\n"));
        assert!(s.contains("Hello!"));
        assert!(s.contains("0\r\n\r\n"), "missing chunked terminal: {s:?}");
    }

}

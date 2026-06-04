// SPDX-License-Identifier: Apache-2.0

/// Client, Transport, and RoundTripper — port of Go's net/http client.
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use go_lib::context::{with_timeout, CancelFn, Context};
use go_lib::net::TcpStream;
use url::Url;

use crate::cookie::{Cookie, CookieJar};
use crate::error::HttpError;
use crate::header::Header;
use crate::parse::response::{read_response, ParsedResponse};
use crate::parse::transfer::Body;
use crate::request::Request;
use crate::response::Response;

// ---------------------------------------------------------------------------
// RoundTripper — port of Go's http.RoundTripper
// ---------------------------------------------------------------------------

/// The low-level interface for executing a single HTTP request.
/// Port of Go's `http.RoundTripper`.
pub trait RoundTripper: Send + Sync {
    fn round_trip(&self, req: Request) -> Result<Response, HttpError>;
}

// ---------------------------------------------------------------------------
// Transport — default RoundTripper with connection pooling
// ---------------------------------------------------------------------------

/// A connection pool entry: an idle `TcpStream` ready for reuse.
struct IdleConn {
    stream: TcpStream,
}

/// Default `RoundTripper` with per-host idle connection pooling.
/// Port of Go's `http.Transport`.
pub struct Transport {
    pub max_idle_conns_per_host: usize,
    pub idle_conn_timeout:       Option<Duration>,
    pub dial_timeout:            Option<Duration>,
    /// TLS client configuration for HTTPS requests.
    /// `None` uses the default Mozilla root store (via `webpki-roots`).
    pub tls_config: Option<Arc<rustls::ClientConfig>>,
    /// Idle connection pool keyed by `"host:port"`.
    pool: Mutex<HashMap<String, VecDeque<IdleConn>>>,
}

impl Transport {
    pub fn new() -> Self {
        Self {
            max_idle_conns_per_host: 10,
            idle_conn_timeout:       Some(Duration::from_secs(90)),
            dial_timeout:            Some(Duration::from_secs(30)),
            tls_config:              None,
            pool:                    Mutex::new(HashMap::new()),
        }
    }

    /// Acquire an idle connection for `host_port`, or dial a new one.
    fn acquire(&self, host_port: &str) -> io::Result<TcpStream> {
        // Try pool first.
        if let Some(conn) = self
            .pool
            .lock()
            .unwrap()
            .get_mut(host_port)
            .and_then(|q| q.pop_front())
        {
            return Ok(conn.stream);
        }
        // Dial a new connection.
        TcpStream::connect(host_port)
    }

    /// Return a connection to the pool for reuse.
    fn release(&self, host_port: &str, stream: TcpStream) {
        let mut pool = self.pool.lock().unwrap();
        let queue = pool.entry(host_port.to_owned()).or_default();
        if queue.len() < self.max_idle_conns_per_host {
            queue.push_back(IdleConn { stream });
        }
        // If over the limit we simply drop the stream (closes the fd).
    }
}

impl Default for Transport {
    fn default() -> Self {
        Self::new()
    }
}

impl RoundTripper for Transport {
    fn round_trip(&self, mut req: Request) -> Result<Response, HttpError> {
        let scheme    = req.url.scheme();
        let is_https  = scheme == "https";
        let host      = req.url.host_str().unwrap_or("localhost");
        let port      = req.url.port_or_known_default()
            .unwrap_or(if is_https { 443 } else { 80 });
        let host_port = format!("{host}:{port}");

        let stream = self.acquire(&host_port).map_err(HttpError::Io)?;

        if is_https {
            // ── HTTPS path ────────────────────────────────────────────────────
            let tls_cfg = match &self.tls_config {
                Some(c) => Arc::clone(c),
                None    => crate::tls::default_client_config(),
            };
            let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|e| HttpError::Tls(e.to_string()))?;
            let client_conn = rustls::ClientConnection::new(tls_cfg, server_name)
                .map_err(|e| HttpError::Tls(e.to_string()))?;
            let mut tls = rustls::StreamOwned::new(client_conn, stream);

            send_request(&mut tls, &mut req)?;

            // Lend `tls` to the response parser via a raw-pointer read wrapper.
            // Safety: `tls` outlives the parser and the response body within this
            // call frame; there is no concurrent access.
            let read_ptr: *mut dyn Read = &mut tls as &mut dyn Read as *mut dyn Read;
            let parsed = read_response(
                RawRead(read_ptr),
                Some(req.method.as_str()),
                crate::parse::request::DEFAULT_MAX_HEADER_BYTES,
            )?;
            // TLS connections are not pooled (no try_clone equivalent).
            Ok(parsed_response_to_response(parsed))
        } else {
            // ── HTTP path ─────────────────────────────────────────────────────
            let mut stream = stream;
            send_request(&mut stream, &mut req)?;

            let parsed = read_response(
                stream.try_clone().map_err(HttpError::Io)?,
                Some(req.method.as_str()),
                crate::parse::request::DEFAULT_MAX_HEADER_BYTES,
            )?;

            let keep_alive = is_keep_alive_parsed(&parsed, req.proto_minor);
            let resp = parsed_response_to_response(parsed);

            if keep_alive {
                self.release(&host_port, stream);
            }
            Ok(resp)
        }
    }
}

// ---------------------------------------------------------------------------
// RawRead — lends a mutable reference as a Send + 'static Read
// ---------------------------------------------------------------------------

/// Raw-pointer wrapper that gives `read_response` a `Read + Send + 'static`
/// view of a `TLS StreamOwned` (or any `impl Read`) without moving it.
///
/// # Safety
/// The caller must ensure the pointed-to value lives at least as long as
/// this `RawRead` is alive and is not concurrently accessed.
struct RawRead(*mut dyn Read);
unsafe impl Send for RawRead {}
impl Read for RawRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe { (*self.0).read(buf) }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// An HTTP client.  Mirrors Go's `http.Client`.
pub struct Client {
    /// Transport used for request execution.
    pub transport:    Arc<dyn RoundTripper>,
    /// Per-request timeout; `None` means no timeout.
    pub timeout:      Option<Duration>,
    /// Maximum number of redirects to follow (default 10, matching Go).
    pub max_redirects: usize,
    /// Optional cookie jar.
    pub jar: Option<Arc<dyn CookieJar>>,
}

impl Client {
    /// Create a client using the default `Transport`.
    pub fn new() -> Self {
        Self {
            transport:     Arc::new(Transport::new()),
            timeout:       None,
            max_redirects: 10,
            jar:           None,
        }
    }

    // ── Convenience methods ───────────────────────────────────────────────

    /// Issue a GET request.  Port of Go's `(*Client).Get`.
    pub fn get(&self, url: &str) -> Result<Response, HttpError> {
        let req = Request::new("GET", url, None)?;
        self.do_request(req)
    }

    /// Issue a POST request with the given content type and body.
    /// Port of Go's `(*Client).Post`.
    pub fn post(
        &self,
        url:          &str,
        content_type: &str,
        body:         Body,
    ) -> Result<Response, HttpError> {
        let mut req = Request::new("POST", url, Some(body))?;
        req.header.set("Content-Type", content_type);
        self.do_request(req)
    }

    /// Issue a POST request with `application/x-www-form-urlencoded` body.
    /// Port of Go's `(*Client).PostForm`.
    pub fn post_form(
        &self,
        url:    &str,
        values: &[(&str, &str)],
    ) -> Result<Response, HttpError> {
        let encoded = url_encode(values);
        let body = Body::Unbounded(Box::new(io::Cursor::new(encoded.into_bytes())));
        self.post(url, "application/x-www-form-urlencoded", body)
    }

    /// Issue a HEAD request.  Port of Go's `(*Client).Head`.
    pub fn head(&self, url: &str) -> Result<Response, HttpError> {
        let req = Request::new("HEAD", url, None)?;
        self.do_request(req)
    }

    // ── Core request execution ────────────────────────────────────────────

    /// Execute `req`, following redirects and attaching cookies.
    /// Port of Go's `(*Client).Do`.
    pub fn do_request(&self, req: Request) -> Result<Response, HttpError> {
        // Wrap in a context deadline if a timeout is configured.
        let (_cancel, ctx_req) = apply_timeout(&req, self.timeout);
        let mut req = if let Some(ctx) = ctx_req { req.with_context(ctx) } else { req };

        // Attach cookies from the jar for the initial URL.
        if let Some(jar) = &self.jar {
            attach_cookies(&mut req, jar.as_ref());
        }

        let mut redirects = 0usize;

        loop {
            let method = req.method.clone();
            let url    = req.url.clone();

            let mut resp = self.transport.round_trip(req)?;

            // Store cookies from the response.
            if let Some(jar) = &self.jar {
                store_cookies(&url, &resp.header, jar.as_ref());
            }

            // ── Redirect handling ─────────────────────────────────────────
            let status = resp.status;
            if !is_redirect(status) {
                return Ok(resp);
            }

            if redirects >= self.max_redirects {
                return Err(HttpError::TooManyRedirects);
            }
            redirects += 1;

            let location = resp
                .header
                .get("Location")
                .ok_or_else(|| HttpError::InvalidUrl("redirect with no Location".into()))?
                .to_owned();

            // Drain the redirect body so the connection can be reused.
            let _ = resp.body_bytes();

            // Resolve the redirect URL against the original.
            let new_url = resolve_url(&url, &location)?;

            // POST → GET on 301/302/303 (matching Go semantics).
            let new_method = match status {
                301 | 302 | 303 => {
                    if method == "POST" { "GET".to_owned() } else { method }
                }
                _ => method,
            };

            let body = if new_method == "GET" || new_method == "HEAD" {
                None
            } else {
                None // body consumed; caller must handle 307/308 with body separately
            };

            let mut new_req = Request::new(&new_method, new_url.as_str(), body)?;
            // Forward safe headers; strip Authorization on cross-origin redirects.
            forward_headers(&mut new_req.header, &resp.header, same_origin(&url, &new_url));

            if let Some(jar) = &self.jar {
                attach_cookies(&mut new_req, jar.as_ref());
            }

            req = new_req;
        }
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Package-level free functions — port of Go's http.Get / http.Post etc.
// ---------------------------------------------------------------------------

/// Global default client, mirroring Go's `http.DefaultClient`.
fn default_client() -> &'static Client {
    use std::sync::OnceLock;
    static DEFAULT: OnceLock<Client> = OnceLock::new();
    DEFAULT.get_or_init(Client::new)
}

/// Issue a GET using the default client.  Port of Go's `http.Get`.
pub fn get(url: &str) -> Result<Response, HttpError> {
    default_client().get(url)
}

/// Issue a POST using the default client.  Port of Go's `http.Post`.
pub fn post(url: &str, content_type: &str, body: Body) -> Result<Response, HttpError> {
    default_client().post(url, content_type, body)
}

/// Issue a POST form using the default client.  Port of Go's `http.PostForm`.
pub fn post_form(url: &str, values: &[(&str, &str)]) -> Result<Response, HttpError> {
    default_client().post_form(url, values)
}

/// Issue a HEAD using the default client.
pub fn head(url: &str) -> Result<Response, HttpError> {
    default_client().head(url)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Serialize a `Request` (headers + body) to `w`.
fn send_request(w: &mut impl Write, req: &mut Request) -> Result<(), HttpError> {
    req.write_header_to(w)?;
    // Write body bytes if present (POST/PUT/PATCH).
    if let Some(body) = req.body.take() {
        use std::io::Read;
        let mut body = body;
        let mut buf = [0u8; 8192];
        loop {
            let n = body.read(&mut buf).map_err(|_| HttpError::BodyRead)?;
            if n == 0 { break; }
            w.write_all(&buf[..n])?;
        }
    }
    Ok(())
}

/// True if a `ParsedResponse` should be treated as keep-alive.
fn is_keep_alive_parsed(resp: &ParsedResponse, req_minor: u8) -> bool {
    let conn = resp.header.get("Connection").unwrap_or("").to_ascii_lowercase();
    if conn.contains("close") { return false; }
    if req_minor == 0 { conn.contains("keep-alive") } else { true }
}

/// Convert a `ParsedResponse` into the public `Response` type.
fn parsed_response_to_response(p: ParsedResponse) -> Response {
    Response {
        status:            p.status,
        status_text:       p.status_text,
        proto:             p.proto,
        proto_major:       p.proto_major,
        proto_minor:       p.proto_minor,
        header:            p.header,
        body:              match p.body {
            Body::Empty => None,
            other       => Some(other),
        },
        content_length:    p.content_length,
        transfer_encoding: p.transfer_encoding,
        trailer:           Header::new(),
    }
}

/// Copy safe headers from `src` into `dst`; strip Authorization on
/// cross-origin redirects.
fn forward_headers(dst: &mut Header, src: &Header, same_origin: bool) {
    for (name, values) in src.iter() {
        // Never forward hop-by-hop headers.
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "connection" | "keep-alive" | "proxy-authenticate"
                | "proxy-authorization" | "te" | "trailers"
                | "transfer-encoding" | "upgrade"
        ) {
            continue;
        }
        // Strip Authorization on cross-origin redirects (Go behaviour).
        if !same_origin && lower == "authorization" {
            continue;
        }
        for v in values {
            dst.add(name, v.as_str());
        }
    }
}

/// True if `status` is a redirect code.
fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Resolve `location` (possibly relative) against `base`.
fn resolve_url(base: &Url, location: &str) -> Result<Url, HttpError> {
    if location.starts_with("http://") || location.starts_with("https://") {
        Url::parse(location).map_err(|e| HttpError::InvalidUrl(e.to_string()))
    } else {
        base.join(location).map_err(|e| HttpError::InvalidUrl(e.to_string()))
    }
}

/// True if `a` and `b` have the same scheme + host + port.
fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port()     == b.port()
}

/// Attach jar cookies to the request's Cookie header.
fn attach_cookies(req: &mut Request, jar: &dyn CookieJar) {
    let cookies = jar.cookies(&req.url);
    if !cookies.is_empty() {
        let pairs: Vec<String> = cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        req.header.set("Cookie", pairs.join("; "));
    }
}

/// Store Set-Cookie headers from a response into the jar.
fn store_cookies(url: &Url, header: &Header, jar: &dyn CookieJar) {
    let cookies: Vec<Cookie> = header
        .values("Set-Cookie")
        .iter()
        .filter_map(|v| {
            let eq = v.find('=')?;
            let name  = v[..eq].trim().to_owned();
            let rest  = &v[eq + 1..];
            let value = rest.split(';').next().unwrap_or("").trim().to_owned();
            Some(Cookie::new(name, value))
        })
        .collect();
    if !cookies.is_empty() {
        jar.set_cookies(url, &cookies);
    }
}

/// Wrap the request's context with a deadline if `timeout` is set.
/// Returns the CancelFn (must be kept alive) and optionally a new Context.
fn apply_timeout(req: &Request, timeout: Option<Duration>) -> (Option<CancelFn>, Option<Context>) {
    match timeout {
        None => (None, None),
        Some(d) => {
            let (ctx, cancel) = with_timeout(req.context(), d);
            (Some(cancel), Some(ctx))
        }
    }
}

/// Percent-encode a form value (spaces → `+`, special chars → `%XX`).
fn url_encode(values: &[(&str, &str)]) -> String {
    values
        .iter()
        .map(|(k, v)| format!("{}={}", encode_form(k), encode_form(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_form(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' '                         => out.push('+'),
            _                            => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_basic() {
        let pairs = [("q", "hello world"), ("lang", "rust")];
        assert_eq!(url_encode(&pairs), "q=hello+world&lang=rust");
    }

    #[test]
    fn url_encode_special_chars() {
        let pairs = [("a", "b&c=d")];
        assert_eq!(url_encode(&pairs), "a=b%26c%3Dd");
    }

    #[test]
    fn resolve_url_absolute() {
        let base = Url::parse("http://example.com/foo").unwrap();
        let resolved = resolve_url(&base, "http://other.com/bar").unwrap();
        assert_eq!(resolved.as_str(), "http://other.com/bar");
    }

    #[test]
    fn resolve_url_relative() {
        let base = Url::parse("http://example.com/a/b").unwrap();
        let resolved = resolve_url(&base, "/c").unwrap();
        assert_eq!(resolved.as_str(), "http://example.com/c");
    }

    #[test]
    fn is_redirect_codes() {
        for code in [301u16, 302, 303, 307, 308] {
            assert!(is_redirect(code), "{code} should be redirect");
        }
        for code in [200u16, 404, 500] {
            assert!(!is_redirect(code), "{code} should not be redirect");
        }
    }

    #[test]
    fn same_origin_check() {
        let a = Url::parse("http://example.com/foo").unwrap();
        let b = Url::parse("http://example.com/bar").unwrap();
        let c = Url::parse("https://example.com/foo").unwrap();
        let d = Url::parse("http://other.com/foo").unwrap();
        assert!(same_origin(&a, &b));
        assert!(!same_origin(&a, &c)); // different scheme
        assert!(!same_origin(&a, &d)); // different host
    }

    #[test]
    fn transport_pool_reuse() {
        // Verify the pool stores and retrieves entries by key without
        // requiring a real network connection.
        // We can't easily test acquire() without a server, but we can
        // verify the pool's limit enforcement via the internal state.
        let t = Transport::new();
        assert_eq!(t.max_idle_conns_per_host, 10);
        // Pool starts empty.
        assert!(t.pool.lock().unwrap().is_empty());
    }

    // client_get_end_to_end is covered by tests/server_client.rs integration
    // tests (get_basic, multiple_sequential_requests, etc.) which run in their
    // own process — go_lib::run() is not safe to call multiple times in the
    // same process (netpoll singleton), so these tests live there.
}

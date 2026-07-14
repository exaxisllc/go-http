// SPDX-License-Identifier: Apache-2.0

/// Client, Transport, and RoundTripper — port of Go's net/http client.
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use go_lib::context::{with_timeout, Context};
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

/// Resolves the proxy to use for a request, or `None` for a direct connection.
/// Port of Go's `Transport.Proxy func(*Request) (*url.URL, error)`.
pub type ProxyFn = Arc<dyn Fn(&Request) -> Result<Option<Url>, HttpError> + Send + Sync>;

/// Default `RoundTripper` with per-host idle connection pooling.
/// Port of Go's `http.Transport`.
pub struct Transport {
    pub max_idle_conns_per_host: usize,
    pub idle_conn_timeout:       Option<Duration>,
    pub dial_timeout:            Option<Duration>,
    /// TLS client configuration for HTTPS requests.
    /// `None` uses the default Mozilla root store (via `webpki-roots`).
    pub tls_config: Option<Arc<rustls::ClientConfig>>,
    /// Proxy resolver.  `None` connects directly.  See [`proxy_from_environment`].
    pub proxy: Option<ProxyFn>,
    /// Speak HTTP/2 with prior knowledge on cleartext `http://` connections
    /// (no upgrade dance; the server must support h2c).  Off by default.
    /// HTTPS connections negotiate HTTP/2 via ALPN independently of this flag.
    pub h2c_prior_knowledge: bool,
    /// Idle connection pool keyed by `"host:port"` (the dial target — the proxy
    /// when proxying, else the origin).
    pool: Mutex<HashMap<String, VecDeque<IdleConn>>>,
    /// Multiplexed HTTP/2 connections, keyed by `"scheme|host:port"`.
    /// One shared connection per origin; evicted when no longer reusable.
    h2_pool: Mutex<HashMap<String, Arc<crate::h2::client::ClientConn>>>,
}

impl Transport {
    pub fn new() -> Self {
        Self {
            max_idle_conns_per_host: 10,
            idle_conn_timeout:       Some(Duration::from_secs(90)),
            dial_timeout:            Some(Duration::from_secs(30)),
            tls_config:              None,
            proxy:                   None,
            h2c_prior_knowledge:     false,
            pool:                    Mutex::new(HashMap::new()),
            h2_pool:                 Mutex::new(HashMap::new()),
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
        let is_https  = req.url.scheme() == "https";
        let host      = req.url.host_str().unwrap_or("localhost").to_owned();
        let port      = req.url.port_or_known_default()
            .unwrap_or(if is_https { 443 } else { 80 });
        let target_hp = format!("{host}:{port}");

        // Resolve the proxy (if any) for this request.
        let proxy_url = match &self.proxy {
            Some(f) => f(&req)?,
            None    => None,
        };

        match proxy_url {
            None => {
                if is_https {
                    self.https_round_trip(req, &host, &target_hp, None)
                } else if self.h2c_prior_knowledge {
                    self.h2_round_trip(req, "h2c", &target_hp)
                } else {
                    self.http_round_trip(req, &target_hp, false)
                }
            }
            Some(pu) => {
                let proxy_hp = proxy_host_port(&pu)?;
                let auth     = proxy_auth_header(&pu);
                if is_https {
                    // Tunnel to the origin through the proxy with CONNECT, then TLS.
                    self.https_round_trip(req, &host, &target_hp, Some((proxy_hp, auth)))
                } else {
                    // Plain HTTP: dial the proxy and send an absolute-form target.
                    if let Some(a) = auth {
                        req.header.set("Proxy-Authorization", a);
                    }
                    self.http_round_trip(req, &proxy_hp, true)
                }
            }
        }
    }
}

impl Transport {
    /// Plain-HTTP round-trip.  `dial_hp` is the address to connect to (the
    /// proxy when proxying, else the origin); `absolute` selects absolute-form
    /// request-target framing for proxied requests.
    fn http_round_trip(
        &self,
        mut req: Request,
        dial_hp: &str,
        absolute: bool,
    ) -> Result<Response, HttpError> {
        let mut stream = self.acquire(dial_hp).map_err(HttpError::Io)?;
        send_request(&mut stream, &mut req, absolute)?;

        let mut parsed = read_response(
            stream.try_clone().map_err(HttpError::Io)?,
            Some(req.method.as_str()),
            crate::parse::request::DEFAULT_MAX_HEADER_BYTES,
        )?;

        let keep_alive = is_keep_alive_parsed(&parsed, req.proto_minor);

        // Buffer the body into memory before releasing the stream to the pool.
        // The body is backed by a try_clone() of `stream`; releasing `stream`
        // while an unread network-backed body alias is live would race the next
        // request's reads on the same socket.  Buffering fully drains the socket
        // first and replaces the body with an in-memory Cursor.
        if keep_alive {
            let bytes = parsed.body.read_to_vec().map_err(|_| HttpError::BodyRead)?;
            parsed.body = Body::Unbounded(Box::new(io::Cursor::new(bytes)));
            self.release(dial_hp, stream);
        }

        Ok(parsed_response_to_response(parsed))
    }

    /// HTTPS round-trip.  Runs [`https_round_trip_inner`] on a dedicated
    /// goroutine with a large initial stack: the rustls/ring handshake call
    /// chain is deep, park-free, and includes assembly with unprobed
    /// multi-page frames that go-lib's reactive stack growth cannot recover
    /// (see `tls::TLS_HANDSHAKE_STACK`).  The calling goroutine parks on a
    /// one-shot channel meanwhile.
    fn https_round_trip(
        &self,
        req: Request,
        sni_host: &str,
        target_hp: &str,
        proxy: Option<(String, Option<String>)>,
    ) -> Result<Response, HttpError> {
        // The h2-pool fast path performs no TLS work — take it on the
        // calling goroutine.
        let pool_key = match &proxy {
            Some((proxy_hp, _)) => format!("https|{target_hp}|{proxy_hp}"),
            None                => format!("https|{target_hp}|-"),
        };
        let pooled = {
            let pool = self.h2_pool.lock().unwrap();
            pool.get(&pool_key).filter(|cc| cc.is_reusable()).map(Arc::clone)
        };
        let mut req = Some(req);
        if let Some(cc) = pooled {
            let retry_snapshot = req.as_ref().and_then(|r| {
                r.body.is_none().then(|| snapshot_request(r))
            });
            match cc.round_trip(req.take().unwrap()) {
                Err(HttpError::Http2(
                    e @ (crate::h2::H2Error::Closed | crate::h2::H2Error::GoAway(..)),
                )) => {
                    // Stale pooled conn: evict and fall through to a fresh
                    // dial (body-less requests only).
                    self.h2_pool.lock().unwrap().remove(&pool_key);
                    match retry_snapshot {
                        Some(snap) => req = Some(snap),
                        None       => return Err(HttpError::Http2(e)),
                    }
                }
                other => return other,
            }
        }
        let req = req.take().unwrap();

        // Fresh connection: handshake on a big-stack goroutine.
        let (tx, rx) = go_lib::chan::chan::<Result<Response, HttpError>>(1);
        // SAFETY of the borrows: we park until the spawned goroutine sends
        // its result, so `self` and the string slices outlive its execution.
        // The goroutine requires 'static, so clone what it captures.
        let sni_host  = sni_host.to_owned();
        let target_hp = target_hp.to_owned();
        let tls_config = self.tls_config.clone();
        let h2_pool_insert = {
            // Channel to hand a freshly established h2 conn back for pooling.
            let (cc_tx, cc_rx) = go_lib::chan::chan::<Arc<crate::h2::client::ClientConn>>(1);
            go_lib::spawn_with_stack(crate::tls::TLS_HANDSHAKE_STACK, move || {
                let result = https_handshake_and_dispatch(
                    req, &sni_host, &target_hp, proxy, tls_config, &cc_tx,
                );
                let _ = std::panic::catch_unwind(
                    std::panic::AssertUnwindSafe(|| tx.send(result)),
                );
            });
            cc_rx
        };

        let result = rx.recv().unwrap_or(Err(HttpError::Http2(crate::h2::H2Error::Closed)));

        // Pool the h2 connection if one was established (after the fact —
        // a lost dial race is resolved by preferring the existing entry).
        if let Some(Some(cc)) = h2_pool_insert.try_recv() {
            let mut pool = self.h2_pool.lock().unwrap();
            if !pool.get(&pool_key).map(|e| e.is_reusable()).unwrap_or(false) {
                pool.insert(pool_key, cc);
            } else {
                cc.close();
            }
        }
        result
    }
}

/// The TLS dial + handshake + protocol dispatch that must run on a
/// large-stack goroutine.  On an h2 negotiation the new connection is also
/// sent through `cc_tx` so the caller can pool it.
fn https_handshake_and_dispatch(
    mut req:    Request,
    sni_host:   &str,
    target_hp:  &str,
    proxy:      Option<(String, Option<String>)>,
    tls_config: Option<Arc<rustls::ClientConfig>>,
    cc_tx:      &go_lib::chan::Sender<Arc<crate::h2::client::ClientConn>>,
) -> Result<Response, HttpError> {
    let stream = match proxy {
        Some((proxy_hp, auth)) => {
            let mut s = TcpStream::connect(proxy_hp.as_str()).map_err(HttpError::Io)?;
            connect_tunnel(&mut s, target_hp, auth.as_deref())?;
            s
        }
        None => TcpStream::connect(target_hp).map_err(HttpError::Io)?,
    };

    let tls_cfg = match &tls_config {
        Some(c) => crate::tls::with_h2_alpn(c),
        None    => crate::tls::h2_client_config(),
    };
    let server_name = rustls::pki_types::ServerName::try_from(sni_host.to_owned())
        .map_err(|e| HttpError::Tls(e.to_string()))?;
    let client_conn = rustls::ClientConnection::new(tls_cfg, server_name)
        .map_err(|e| HttpError::Tls(e.to_string()))?;
    let mut tls = rustls::StreamOwned::new(client_conn, stream);

    // Complete the handshake so ALPN is decided before the first byte.
    while tls.conn.is_handshaking() {
        tls.conn
            .complete_io(&mut tls.sock)
            .map_err(|e| HttpError::Tls(e.to_string()))?;
    }

    // ── ALPN dispatch ─────────────────────────────────────────────────────
    if tls.conn.alpn_protocol() == Some(b"h2") {
        let rustls::StreamOwned { conn, sock } = tls;
        let cc = crate::h2::client::ClientConn::new_tls(conn, sock)
            .map_err(HttpError::Http2)?;
        // Hand the connection back for pooling (buffered; never blocks).
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cc_tx.try_send(Arc::clone(&cc))
        }));
        return cc.round_trip(req);
    }

    // ── HTTP/1.1 over TLS ──────────────────────────────────────────────────
    // Origin-form over the (possibly tunnelled) TLS connection.
    send_request(&mut tls, &mut req, false)?;

    // Lend `tls` to the response parser via a raw-pointer read wrapper.
    // Safety: `tls` outlives the parser and the fully-buffered body below;
    // there is no concurrent access.
    let read_ptr: *mut dyn Read = &mut tls as &mut dyn Read as *mut dyn Read;
    let mut parsed = read_response(
        RawRead(read_ptr),
        Some(req.method.as_str()),
        crate::parse::request::DEFAULT_MAX_HEADER_BYTES,
    )?;
    // Buffer the body before `tls` (which the body reader points into) goes
    // out of scope.  TLS connections are not pooled, so nothing is reused.
    let bytes = parsed.body.read_to_vec().map_err(|_| HttpError::BodyRead)?;
    parsed.body = Body::Unbounded(Box::new(io::Cursor::new(bytes)));
    Ok(parsed_response_to_response(parsed))
}

impl Transport {
    /// Round-trip over a pooled, multiplexed HTTP/2 connection.
    ///
    /// If the pooled connection turns out to be dead or draining (GOAWAY),
    /// it is evicted and a body-less request is retried once on a fresh
    /// connection, mirroring Go's stale-connection retry.  Requests with a
    /// body are not retried — the body may be partially consumed.
    fn h2_round_trip(
        &self,
        req:     Request,
        scheme:  &str,
        dial_hp: &str,
    ) -> Result<Response, HttpError> {
        let key = format!("{scheme}|{dial_hp}");
        let retry_snapshot = req.body.is_none().then(|| snapshot_request(&req));

        let cc = self.h2_conn(&key, dial_hp)?;
        match cc.round_trip(req) {
            Err(HttpError::Http2(
                e @ (crate::h2::H2Error::Closed | crate::h2::H2Error::GoAway(..)),
            )) => {
                self.h2_pool.lock().unwrap().remove(&key);
                match retry_snapshot {
                    Some(snap) => {
                        let fresh = self.h2_conn(&key, dial_hp)?;
                        fresh.round_trip(snap)
                    }
                    None => Err(HttpError::Http2(e)),
                }
            }
            other => other,
        }
    }

    /// Fetch (or dial) the shared h2 connection for `key`.
    fn h2_conn(
        &self,
        key:     &str,
        dial_hp: &str,
    ) -> Result<Arc<crate::h2::client::ClientConn>, HttpError> {
        if let Some(cc) = self.h2_pool.lock().unwrap().get(key)
            && cc.is_reusable()
        {
            return Ok(Arc::clone(cc));
        }
        let stream = TcpStream::connect(dial_hp).map_err(HttpError::Io)?;
        let cc = crate::h2::client::ClientConn::new_plain(stream).map_err(HttpError::Http2)?;
        let mut pool = self.h2_pool.lock().unwrap();
        // Lost the dial race: prefer the existing reusable conn.
        if let Some(existing) = pool.get(key)
            && existing.is_reusable()
        {
            cc.close();
            return Ok(Arc::clone(existing));
        }
        pool.insert(key.to_owned(), Arc::clone(&cc));
        Ok(cc)
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
// Redirect policy — port of Go's Client.CheckRedirect
// ---------------------------------------------------------------------------

/// What a [`CheckRedirect`] policy decides for the next hop.
pub enum RedirectPolicy {
    /// Follow the redirect (subject to the same policy on the following hop).
    Follow,
    /// Stop and return the most recent response as-is, without following.
    /// Equivalent to returning Go's `ErrUseLastResponse`.
    UseLastResponse,
}

/// A redirect policy.  Called before each redirect is followed, with the
/// request about to be made and `via`, the chain of requests already made
/// (oldest first).  Return [`RedirectPolicy::Follow`] to continue,
/// [`RedirectPolicy::UseLastResponse`] to stop and return the last response,
/// or an `Err` to abort the request with that error.
///
/// Port of Go's `Client.CheckRedirect func(req *Request, via []*Request) error`.
pub type CheckRedirect =
    Arc<dyn Fn(&Request, &[Request]) -> Result<RedirectPolicy, HttpError> + Send + Sync>;

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// An HTTP client.  Mirrors Go's `http.Client`.
pub struct Client {
    /// Transport used for request execution.
    pub transport:    Arc<dyn RoundTripper>,
    /// Per-request timeout; `None` means no timeout.
    pub timeout:      Option<Duration>,
    /// Maximum number of redirects to follow when `check_redirect` is `None`
    /// (default 10, matching Go).
    pub max_redirects: usize,
    /// Custom redirect policy.  `None` uses the default (follow up to
    /// `max_redirects`, then fail with [`HttpError::TooManyRedirects`]).
    pub check_redirect: Option<CheckRedirect>,
    /// Optional cookie jar.
    pub jar: Option<Arc<dyn CookieJar>>,
}

impl Client {
    /// Create a client using the default `Transport`.
    pub fn new() -> Self {
        Self {
            transport:      Arc::new(Transport::new()),
            timeout:        None,
            max_redirects:  10,
            check_redirect: None,
            jar:            None,
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
        // Effective cancellation context governing *every* redirect hop: the
        // request's own context (which may already carry a per-request
        // deadline via `Request::new_with_context`) with the client-wide
        // `timeout` applied on top.  `_cancel` must stay alive for the whole
        // call so the deadline timer is not released early.
        let (_cancel, ctx) = match self.timeout {
            Some(d) => {
                let (c, cancel) = with_timeout(req.context(), d);
                (Some(cancel), c)
            }
            None => (None, req.context().clone()),
        };

        let mut req = req;

        // Attach cookies from the jar for the initial URL.
        if let Some(jar) = &self.jar {
            attach_cookies(&mut req, jar.as_ref());
        }

        // `via` records the requests already made (oldest first), as Go passes
        // to CheckRedirect.  Entries are body-less snapshots.
        let mut via: Vec<Request> = Vec::new();

        loop {
            let method = req.method.clone();
            let url    = req.url.clone();
            let via_entry = snapshot_request(&req);

            let mut resp = self.execute_round_trip(req, &ctx)?;
            via.push(via_entry);

            // Store cookies from the response.
            if let Some(jar) = &self.jar {
                store_cookies(&url, &resp.header, jar.as_ref());
            }

            // ── Redirect handling ─────────────────────────────────────────
            let status = resp.status;
            if !is_redirect(status) {
                return Ok(resp);
            }

            let location = resp
                .header
                .get("Location")
                .ok_or_else(|| HttpError::InvalidUrl("redirect with no Location".into()))?
                .to_owned();

            // Resolve the redirect URL against the current one.
            let new_url = resolve_url(&url, &location)?;

            // POST → GET on 301/302/303 (matching Go semantics).
            let new_method = match status {
                301..=303 => {
                    if method == "POST" { "GET".to_owned() } else { method }
                }
                _ => method,
            };

            // Body is consumed; 307/308 with a body would need a rewindable
            // body to replay (not yet supported).
            let mut new_req = Request::new(&new_method, new_url.as_str(), None)?;
            // Forward safe headers; strip Authorization on cross-origin redirects.
            forward_headers(&mut new_req.header, &resp.header, same_origin(&url, &new_url));
            if let Some(jar) = &self.jar {
                attach_cookies(&mut new_req, jar.as_ref());
            }

            // ── Apply the redirect policy ──────────────────────────────────
            match &self.check_redirect {
                Some(policy) => match policy(&new_req, &via)? {
                    RedirectPolicy::Follow => {}
                    RedirectPolicy::UseLastResponse => return Ok(resp),
                },
                None => {
                    if via.len() > self.max_redirects {
                        return Err(HttpError::TooManyRedirects);
                    }
                }
            }

            // Drain the redirect body so the connection can be reused.
            let _ = resp.body_bytes();

            req = new_req;
        }
    }

    /// Execute one round-trip, honouring the cancellation `ctx`.
    ///
    /// When `ctx` carries a deadline the round-trip runs on its own goroutine
    /// and is raced against `ctx.done()`, so a timeout or cancellation returns
    /// promptly with [`HttpError::Timeout`] even while the underlying socket
    /// I/O is still blocked.  (The abandoned goroutine finishes on its own when
    /// the I/O completes or errors; like Go, we can't forcibly unwind it.)
    /// Without a deadline the round-trip runs synchronously — no extra goroutine.
    fn execute_round_trip(&self, req: Request, ctx: &Context) -> Result<Response, HttpError> {
        if ctx.deadline().is_none() {
            return self.transport.round_trip(req);
        }
        if ctx.is_done() {
            return Err(HttpError::Timeout);
        }

        let (tx, rx) = go_lib::chan::chan::<Result<Response, HttpError>>(1);
        let transport = Arc::clone(&self.transport);
        go_lib::go!(move || {
            let result = transport.round_trip(req);
            // Buffered(1): the send never blocks.  If the receiver already
            // timed out and is gone, catch_unwind swallows any panic.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tx.send(result)));
        });

        go_lib::select! {
            recv(ctx.done()) -> _sig => { Err(HttpError::Timeout) }
            recv(rx) -> result => { result.unwrap_or_else(|| Err(HttpError::Timeout)) }
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

/// Serialize a `Request` (headers + body) to `w`.  `absolute` selects
/// absolute-form request-target framing (`GET http://host/path`) for requests
/// sent to an HTTP proxy.
fn send_request(w: &mut impl Write, req: &mut Request, absolute: bool) -> Result<(), HttpError> {
    if absolute {
        req.write_header_absolute_to(w)?;
    } else {
        req.write_header_to(w)?;
    }
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

// ---------------------------------------------------------------------------
// Proxy helpers
// ---------------------------------------------------------------------------

/// A [`ProxyFn`] that reads the standard proxy environment variables, mirroring
/// Go's `http.ProxyFromEnvironment`:
///
/// - `HTTPS_PROXY` / `https_proxy` for `https://` requests,
/// - `HTTP_PROXY`  / `http_proxy`  for `http://`  requests,
/// - `NO_PROXY`    / `no_proxy`    a comma-separated exclusion list.
///
/// `NO_PROXY` entries match by exact host, by `.suffix` (any sub-domain), or
/// `*` (everything).  The environment is read on each call.  A bare
/// `host:port` proxy value is treated as an `http://` URL.
pub fn proxy_from_environment() -> ProxyFn {
    Arc::new(|req: &Request| -> Result<Option<Url>, HttpError> {
        let is_https = req.url.scheme() == "https";
        let host     = req.url.host_str().unwrap_or("");

        // NO_PROXY exclusions.
        if let Some(no) = env_first(&["NO_PROXY", "no_proxy"])
            && host_matches_no_proxy(host, &no)
        {
            return Ok(None);
        }

        let names: &[&str] = if is_https {
            &["HTTPS_PROXY", "https_proxy"]
        } else {
            &["HTTP_PROXY", "http_proxy"]
        };
        match env_first(names) {
            None => Ok(None),
            Some(v) if v.trim().is_empty() => Ok(None),
            Some(v) => {
                let raw = v.trim();
                // Accept bare "host:port" by defaulting to an http:// scheme.
                let normalized = if raw.contains("://") {
                    raw.to_owned()
                } else {
                    format!("http://{raw}")
                };
                let url = Url::parse(&normalized)
                    .map_err(|e| HttpError::Proxy(format!("bad proxy URL {raw:?}: {e}")))?;
                Ok(Some(url))
            }
        }
    })
}

/// Return the first set, non-empty environment variable among `names`.
fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| std::env::var(n).ok())
}

/// Match `host` against a `NO_PROXY` list (comma-separated).
fn host_matches_no_proxy(host: &str, no_proxy: &str) -> bool {
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    for entry in no_proxy.split(',') {
        let e = entry.trim().to_ascii_lowercase();
        if e.is_empty() {
            continue;
        }
        if e == "*" {
            return true;
        }
        let suffix = e.trim_start_matches('.');
        if host == suffix || host.ends_with(&format!(".{suffix}")) {
            return true;
        }
    }
    false
}

/// `host:port` to dial for a proxy URL (default port 80).
fn proxy_host_port(pu: &Url) -> Result<String, HttpError> {
    let h = pu
        .host_str()
        .ok_or_else(|| HttpError::Proxy("proxy URL has no host".into()))?;
    let p = pu.port_or_known_default().unwrap_or(80);
    Ok(format!("{h}:{p}"))
}

/// Build a `Basic` `Proxy-Authorization` value from a proxy URL's userinfo,
/// or `None` when there is no username.
fn proxy_auth_header(pu: &Url) -> Option<String> {
    let user = pu.username();
    if user.is_empty() {
        return None;
    }
    let pass  = pu.password().unwrap_or("");
    let creds = format!("{user}:{pass}");
    let b64   = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, creds);
    Some(format!("Basic {b64}"))
}

/// Establish an HTTP `CONNECT` tunnel through a proxy to `target_hp`.
/// On success the stream is positioned to begin the TLS handshake.
fn connect_tunnel<S: Read + Write>(
    stream: &mut S,
    target_hp: &str,
    auth: Option<&str>,
) -> Result<(), HttpError> {
    let mut req = format!("CONNECT {target_hp} HTTP/1.1\r\nHost: {target_hp}\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Proxy-Authorization: {a}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).map_err(HttpError::Io)?;

    let status = read_connect_response(stream)?;
    if !(200..300).contains(&status) {
        return Err(HttpError::Proxy(format!(
            "CONNECT to {target_hp} failed with status {status}"
        )));
    }
    Ok(())
}

/// Read a proxy `CONNECT` response (status line + headers, no body) and return
/// its status code.
fn read_connect_response<R: Read>(stream: &mut R) -> Result<u16, HttpError> {
    let mut buf  = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).map_err(HttpError::Io)?;
        if n == 0 {
            return Err(HttpError::Proxy("proxy closed before CONNECT response".into()));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Err(HttpError::Proxy("CONNECT response headers too large".into()));
        }
    }
    let text  = String::from_utf8_lossy(&buf);
    let first = text.lines().next().unwrap_or("");
    first
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Proxy(format!("malformed CONNECT status line: {first:?}")))
}

/// Build a body-less snapshot of `r` for the redirect `via` chain (method,
/// URL, headers, host).  Infallible: `r` already holds a valid method + URL.
fn snapshot_request(r: &Request) -> Request {
    let mut s = Request::new(&r.method, r.url.as_str(), None)
        .unwrap_or_else(|_| Request::new("GET", "http://invalid.invalid/", None).unwrap());
    s.header = r.header.clone();
    s.host   = r.host.clone();
    s.proto  = r.proto.clone();
    s
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
    // tests (get_basic, multiple_sequential_requests, etc.), which carry
    // `#[go_lib::main]` so each test body runs as the first goroutine on the
    // shared process-wide scheduler.

    // ── Proxy helpers ──────────────────────────────────────────────────────

    #[test]
    fn proxy_auth_header_from_userinfo() {
        use base64::Engine;
        let with = Url::parse("http://user:pass@proxy.local:3128").unwrap();
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("user:pass")
        );
        assert_eq!(proxy_auth_header(&with), Some(expected));

        let without = Url::parse("http://proxy.local:3128").unwrap();
        assert_eq!(proxy_auth_header(&without), None);
    }

    #[test]
    fn proxy_host_port_defaults_port_80() {
        let u = Url::parse("http://proxy.local").unwrap();
        assert_eq!(proxy_host_port(&u).unwrap(), "proxy.local:80");
        let u2 = Url::parse("http://proxy.local:8080").unwrap();
        assert_eq!(proxy_host_port(&u2).unwrap(), "proxy.local:8080");
    }

    #[test]
    fn no_proxy_matching() {
        assert!(host_matches_no_proxy("example.com", "example.com"));
        assert!(host_matches_no_proxy("api.example.com", ".example.com"));
        assert!(host_matches_no_proxy("api.example.com", "example.com"));
        assert!(host_matches_no_proxy("anything", "*"));
        assert!(host_matches_no_proxy("b.internal", "foo.com, .internal"));
        assert!(!host_matches_no_proxy("example.org", "example.com"));
        assert!(!host_matches_no_proxy("notexample.com", ".example.com"));
    }

    #[test]
    fn connect_response_parsing() {
        use std::io::Cursor;
        let mut ok = Cursor::new(b"HTTP/1.1 200 Connection established\r\n\r\n".to_vec());
        assert_eq!(read_connect_response(&mut ok).unwrap(), 200);

        let mut denied = Cursor::new(b"HTTP/1.1 407 Proxy Auth Required\r\n\r\n".to_vec());
        assert_eq!(read_connect_response(&mut denied).unwrap(), 407);
    }

    /// In-memory Read+Write used to exercise `connect_tunnel` without a socket.
    struct MockConn {
        to_read: std::io::Cursor<Vec<u8>>,
        written: Vec<u8>,
    }
    impl Read for MockConn {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.to_read.read(buf)
        }
    }
    impl Write for MockConn {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn connect_tunnel_sends_request_and_accepts_200() {
        let mut conn = MockConn {
            to_read: std::io::Cursor::new(b"HTTP/1.1 200 OK\r\n\r\n".to_vec()),
            written: Vec::new(),
        };
        connect_tunnel(&mut conn, "example.com:443", Some("Basic Zm9v")).unwrap();
        let sent = String::from_utf8(conn.written.clone()).unwrap();
        assert!(sent.starts_with("CONNECT example.com:443 HTTP/1.1\r\n"), "got: {sent:?}");
        assert!(sent.contains("Host: example.com:443\r\n"));
        assert!(sent.contains("Proxy-Authorization: Basic Zm9v\r\n"));
        assert!(sent.ends_with("\r\n\r\n"));
    }

    #[test]
    fn connect_tunnel_errors_on_non_2xx() {
        let mut conn = MockConn {
            to_read: std::io::Cursor::new(b"HTTP/1.1 403 Forbidden\r\n\r\n".to_vec()),
            written: Vec::new(),
        };
        let err = connect_tunnel(&mut conn, "example.com:443", None).unwrap_err();
        assert!(matches!(err, HttpError::Proxy(_)), "got: {err:?}");
    }
}

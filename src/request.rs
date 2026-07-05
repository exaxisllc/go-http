// SPDX-License-Identifier: Apache-2.0

/// Request — port of Go's `net/http.Request`.
use std::collections::HashMap;
use std::io::Read;

use url::Url;

use go_lib::context::Context;
use crate::error::HttpError;
use crate::header::Header;
use crate::parse::transfer::Body;

/// An HTTP request (incoming server-side or outgoing client-side).
/// Mirrors Go's `http.Request`.
pub struct Request {
    /// HTTP method (GET, POST, …).
    pub method: String,
    /// Parsed request URL.
    pub url: Url,
    /// Protocol version string, e.g. "HTTP/1.1".
    pub proto: String,
    pub proto_major: u8,
    pub proto_minor: u8,
    /// Request headers.
    pub header: Header,
    /// Request body; `None` after the body has been consumed or for bodyless methods.
    pub body: Option<Body>,
    /// -1 means unknown; ≥ 0 means exact byte count from Content-Length.
    pub content_length: i64,
    /// Transfer-Encoding values in order (e.g. ["chunked"]).
    pub transfer_encoding: Vec<String>,
    /// Value of the Host header (or from the URL for outgoing requests).
    pub host: String,
    /// Trailing headers populated after a chunked body is fully read.
    pub trailer: Header,
    /// Remote address of the client (set by the server, empty on client requests).
    pub remote_addr: String,
    /// Named path parameters captured by ServeMux wildcard patterns, e.g. `{id}`.
    /// Populated by `ServeMux::serve_http`; empty for client-side requests.
    pub path_params: HashMap<String, String>,
    /// Parsed form values (populated by `parse_form`).
    form: Option<HashMap<String, Vec<String>>>,
    /// Cancellation context.
    ctx: Context,
}

impl Request {
    // ── Constructors ─────────────────────────────────────────────────────────

    /// Create a new outgoing request.
    /// Port of Go's `http.NewRequest`.
    pub fn new(method: &str, url: &str, body: Option<Body>) -> Result<Self, HttpError> {
        let ctx = go_lib::context::background();
        Self::new_with_context(method, url, body, ctx)
    }

    /// Create a new outgoing request tied to a context.
    /// Port of Go's `http.NewRequestWithContext`.
    pub fn new_with_context(
        method: &str,
        url: &str,
        body: Option<Body>,
        ctx: Context,
    ) -> Result<Self, HttpError> {
        if !crate::method::is_valid(method) {
            return Err(HttpError::InvalidUrl(format!("invalid method: {method}")));
        }
        let parsed = Url::parse(url).map_err(|e| HttpError::InvalidUrl(e.to_string()))?;
        let host = parsed.host_str().unwrap_or("").to_owned();
        let content_length = match &body {
            None => 0,
            Some(_) => -1,
        };
        Ok(Self {
            method: method.to_owned(),
            url: parsed,
            proto: "HTTP/1.1".into(),
            proto_major: 1,
            proto_minor: 1,
            header: Header::new(),
            body,
            content_length,
            transfer_encoding: Vec::new(),
            host,
            trailer: Header::new(),
            remote_addr: String::new(),
            path_params: HashMap::new(),
            form: None,
            ctx,
        })
    }

    // ── Context ───────────────────────────────────────────────────────────────

    pub fn context(&self) -> &Context {
        &self.ctx
    }

    /// Return a shallow clone of this request with the context replaced.
    /// Port of Go's `(*Request).WithContext`.
    pub fn with_context(mut self, ctx: Context) -> Self {
        self.ctx = ctx;
        self
    }

    // ── Path parameter helpers ────────────────────────────────────────────────

    /// Return the value captured for the named wildcard in the matched ServeMux
    /// pattern (e.g. `{id}` or `{path...}`).  Returns `""` if the parameter
    /// was not captured.  Port of Go's `(*Request).PathValue`.
    pub fn path_value(&self, name: &str) -> &str {
        self.path_params.get(name).map(String::as_str).unwrap_or("")
    }

    // ── Header helpers ────────────────────────────────────────────────────────

    pub fn user_agent(&self) -> &str {
        self.header.get("User-Agent").unwrap_or("")
    }

    pub fn referer(&self) -> &str {
        self.header.get("Referer").unwrap_or("")
    }

    /// Parse and return Basic Auth credentials.
    /// Port of Go's `(*Request).BasicAuth`.
    pub fn basic_auth(&self) -> Option<(String, String)> {
        let val = self.header.get("Authorization")?;
        let rest = val.strip_prefix("Basic ")?;
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            rest.trim(),
        )
        .ok()?;
        let s = String::from_utf8(decoded).ok()?;
        let colon = s.find(':')?;
        Some((s[..colon].to_owned(), s[colon + 1..].to_owned()))
    }

    // ── Cookie helpers ────────────────────────────────────────────────────────

    /// Return all cookies sent with the request.
    pub fn cookies(&self) -> Vec<crate::cookie::Cookie> {
        crate::cookie::parse_request_cookies(&self.header)
    }

    /// Return the named cookie, or `None`.
    pub fn cookie(&self, name: &str) -> Option<crate::cookie::Cookie> {
        self.cookies().into_iter().find(|c| c.name == name)
    }

    // ── Body helpers ─────────────────────────────────────────────────────────

    /// Read the entire request body, populate `self.trailer` from any chunked
    /// trailer headers, and return the raw bytes.
    pub fn body_bytes(&mut self) -> Result<Vec<u8>, HttpError> {
        let mut out = Vec::new();
        if let Some(ref mut body) = self.body {
            body.read_to_end(&mut out).map_err(|_| HttpError::BodyRead)?;
            self.trailer = body.trailers().clone();
        }
        Ok(out)
    }

    /// Trailer headers declared by the client.  Populated after the body has
    /// been fully read via `body_bytes` or a manual `read_to_end`.
    pub fn trailers(&self) -> &Header {
        &self.trailer
    }

    // ── Form parsing ──────────────────────────────────────────────────────────

    /// Parse application/x-www-form-urlencoded body or query string.
    /// Port of Go's `(*Request).ParseForm`.
    pub fn parse_form(&mut self) -> Result<(), HttpError> {
        if self.form.is_some() {
            return Ok(());
        }
        let mut values: HashMap<String, Vec<String>> = HashMap::new();

        // Query string.
        for (k, v) in self.url.query_pairs() {
            values.entry(k.into_owned()).or_default().push(v.into_owned());
        }

        // Body (only for POST/PUT/PATCH with the right content-type).
        let ct = self
            .header
            .get("Content-Type")
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(self.method.as_str(), "POST" | "PUT" | "PATCH")
            && ct.starts_with("application/x-www-form-urlencoded")
            && let Some(body) = self.body.take()
        {
            let mut raw = Vec::new();
            BodyReader(body)
                .read_to_end(&mut raw)
                .map_err(|_| HttpError::BodyRead)?;
            let s = String::from_utf8_lossy(&raw);
            for pair in s.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    let k = url_decode(k);
                    let v = url_decode(v);
                    values.entry(k).or_default().push(v);
                }
            }
        }

        self.form = Some(values);
        Ok(())
    }

    /// Return a form value by key (after calling `parse_form`).
    pub fn form_value(&self, key: &str) -> Option<&str> {
        self.form
            .as_ref()?
            .get(key)?
            .first()
            .map(String::as_str)
    }

    // ── Wire serialization ────────────────────────────────────────────────────

    /// Serialize the request line and headers to `w` (body not included).
    /// Port of Go's `(*Request).write`.
    pub fn write_header_to(&self, w: &mut impl std::io::Write) -> Result<(), HttpError> {
        let path = if self.url.path().is_empty() { "/" } else { self.url.path() };
        let query = self
            .url
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();

        write!(w, "{} {}{} {}\r\n", self.method, path, query, self.proto)?;
        write!(w, "Host: {}\r\n", self.host)?;
        self.header.write_to(w)?;
        w.write_all(b"\r\n")?;
        Ok(())
    }
}

// Helper: allow Body to be read via std::io::Read without exposing internals.
struct BodyReader(Body);
impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

fn url_decode(s: &str) -> String {
    // Simple + → space, then percent-decode.
    let s = s.replace('+', " ");
    url::form_urlencoded::parse(s.as_bytes())
        .map(|(k, _)| k.into_owned())
        .next()
        .unwrap_or(s.clone())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_get_request() {
        let req = Request::new("GET", "http://example.com/path?q=1", None).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.host, "example.com");
        assert_eq!(req.url.path(), "/path");
    }

    #[test]
    fn invalid_method_rejected() {
        assert!(Request::new("GÉT", "http://example.com/", None).is_err());
    }

    #[test]
    fn write_header() {
        let mut req = Request::new("GET", "http://example.com/", None).unwrap();
        req.header.set("Accept", "text/html");
        let mut out = Vec::new();
        req.write_header_to(&mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("GET / HTTP/1.1\r\n"));
        assert!(s.contains("Host: example.com\r\n"));
        assert!(s.contains("Accept: text/html\r\n"));
    }

    #[test]
    fn write_header_includes_query() {
        let req = Request::new("GET", "http://example.com/search?q=rust&n=1", None).unwrap();
        let mut out = Vec::new();
        req.write_header_to(&mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("GET /search?q=rust&n=1 HTTP/1.1\r\n"), "got: {s:?}");
    }

    #[test]
    fn body_sets_content_length_unknown() {
        let body = Body::Unbounded(Box::new(std::io::Cursor::new(b"abc".to_vec())));
        let req = Request::new("POST", "http://example.com/", Some(body)).unwrap();
        assert_eq!(req.content_length, -1);
        let none = Request::new("GET", "http://example.com/", None).unwrap();
        assert_eq!(none.content_length, 0);
    }

    #[test]
    fn parse_form_query_only() {
        let mut req = Request::new("GET", "http://x/p?a=1&a=2&b=hello", None).unwrap();
        req.parse_form().unwrap();
        assert_eq!(req.form_value("a"), Some("1"));
        assert_eq!(req.form_value("b"), Some("hello"));
        assert_eq!(req.form_value("missing"), None);
        // Idempotent: a second call is a no-op.
        req.parse_form().unwrap();
        assert_eq!(req.form_value("a"), Some("1"));
    }

    #[test]
    fn parse_form_urlencoded_body() {
        let body = Body::Unbounded(Box::new(std::io::Cursor::new(
            b"name=Alice&city=New+York".to_vec(),
        )));
        let mut req = Request::new("POST", "http://x/submit?q=top", Some(body)).unwrap();
        req.header.set("Content-Type", "application/x-www-form-urlencoded");
        req.parse_form().unwrap();
        // Query and body values are merged.
        assert_eq!(req.form_value("q"), Some("top"));
        assert_eq!(req.form_value("name"), Some("Alice"));
        assert_eq!(req.form_value("city"), Some("New York"));
    }

    #[test]
    fn basic_auth_valid() {
        use base64::Engine;
        let creds = base64::engine::general_purpose::STANDARD.encode("alice:s3cret");
        let mut req = Request::new("GET", "http://x/", None).unwrap();
        req.header.set("Authorization", format!("Basic {creds}"));
        let (user, pass) = req.basic_auth().unwrap();
        assert_eq!(user, "alice");
        assert_eq!(pass, "s3cret");
    }

    #[test]
    fn basic_auth_absent_or_wrong_scheme() {
        let req = Request::new("GET", "http://x/", None).unwrap();
        assert!(req.basic_auth().is_none());

        let mut bearer = Request::new("GET", "http://x/", None).unwrap();
        bearer.header.set("Authorization", "Bearer token123");
        assert!(bearer.basic_auth().is_none());
    }

    #[test]
    fn user_agent_and_referer() {
        let mut req = Request::new("GET", "http://x/", None).unwrap();
        assert_eq!(req.user_agent(), "");
        assert_eq!(req.referer(), "");
        req.header.set("User-Agent", "go-http/0.1");
        req.header.set("Referer", "http://ref/");
        assert_eq!(req.user_agent(), "go-http/0.1");
        assert_eq!(req.referer(), "http://ref/");
    }

    #[test]
    fn cookies_parsed_from_header() {
        let mut req = Request::new("GET", "http://x/", None).unwrap();
        req.header.set("Cookie", "session=abc; theme=dark");
        let all = req.cookies();
        assert_eq!(all.len(), 2);
        assert_eq!(req.cookie("session").unwrap().value, "abc");
        assert_eq!(req.cookie("theme").unwrap().value, "dark");
        assert!(req.cookie("nope").is_none());
    }

    #[test]
    fn with_context_replaces_context() {
        let req = Request::new("GET", "http://x/", None).unwrap();
        let ctx = go_lib::context::background();
        let req = req.with_context(ctx);
        // The context accessor returns the replacement without panicking.
        let _ = req.context();
    }
}

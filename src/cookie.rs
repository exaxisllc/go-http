// SPDX-License-Identifier: Apache-2.0

/// Cookie and CookieJar — port of Go net/http cookie handling.
use std::time::SystemTime;

use crate::header::Header;

// ---------------------------------------------------------------------------
// SameSite
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SameSite {
    #[default]
    Default,
    Lax,
    Strict,
    None,
}

// ---------------------------------------------------------------------------
// Cookie
// ---------------------------------------------------------------------------

/// An HTTP cookie.  Mirrors Go's `http.Cookie`.
#[derive(Debug, Clone, Default)]
pub struct Cookie {
    pub name:      String,
    pub value:     String,
    pub path:      String,
    pub domain:    String,
    pub expires:   Option<SystemTime>,
    pub max_age:   i32,
    pub secure:    bool,
    pub http_only: bool,
    pub same_site: SameSite,
}

impl Cookie {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            ..Default::default()
        }
    }

    /// Serialize to `Set-Cookie` header value.
    pub fn to_set_cookie_header(&self) -> String {
        let mut s = format!("{}={}", self.name, self.value);
        if !self.path.is_empty()   { s.push_str(&format!("; Path={}", self.path)); }
        if !self.domain.is_empty() { s.push_str(&format!("; Domain={}", self.domain)); }
        if self.max_age > 0        { s.push_str(&format!("; Max-Age={}", self.max_age)); }
        if self.secure             { s.push_str("; Secure"); }
        if self.http_only          { s.push_str("; HttpOnly"); }
        match self.same_site {
            SameSite::Lax    => s.push_str("; SameSite=Lax"),
            SameSite::Strict => s.push_str("; SameSite=Strict"),
            SameSite::None   => s.push_str("; SameSite=None"),
            SameSite::Default => {}
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Parse request cookies from Cookie header
// ---------------------------------------------------------------------------

/// Parse all cookies from the `Cookie` header of an incoming request.
pub fn parse_request_cookies(h: &Header) -> Vec<Cookie> {
    let mut cookies = Vec::new();
    for val in h.values("Cookie") {
        for pair in val.split(';') {
            let pair = pair.trim();
            if let Some(eq) = pair.find('=') {
                let name  = pair[..eq].trim().to_owned();
                let value = pair[eq + 1..].trim().to_owned();
                if !name.is_empty() {
                    cookies.push(Cookie::new(name, value));
                }
            }
        }
    }
    cookies
}

// ---------------------------------------------------------------------------
// CookieJar trait
// ---------------------------------------------------------------------------

pub trait CookieJar: Send + Sync {
    fn cookies(&self, url: &url::Url) -> Vec<Cookie>;
    fn set_cookies(&self, url: &url::Url, cookies: &[Cookie]);
}

// ---------------------------------------------------------------------------
// MemoryCookieJar
// ---------------------------------------------------------------------------

use std::sync::Mutex;
use std::collections::HashMap;

/// In-memory cookie jar.
pub struct MemoryCookieJar {
    store: Mutex<HashMap<String, Vec<Cookie>>>,
}

impl MemoryCookieJar {
    pub fn new() -> Self {
        Self { store: Mutex::new(HashMap::new()) }
    }
}

impl Default for MemoryCookieJar {
    fn default() -> Self { Self::new() }
}

impl CookieJar for MemoryCookieJar {
    fn cookies(&self, url: &url::Url) -> Vec<Cookie> {
        let key = url.host_str().unwrap_or("").to_owned();
        self.store.lock().unwrap().get(&key).cloned().unwrap_or_default()
    }

    fn set_cookies(&self, url: &url::Url, cookies: &[Cookie]) {
        let key = url.host_str().unwrap_or("").to_owned();
        self.store.lock().unwrap()
            .entry(key)
            .or_default()
            .extend_from_slice(cookies);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_set_cookie_header_basic() {
        let c = Cookie::new("session", "abc123");
        let s = c.to_set_cookie_header();
        assert_eq!(s, "session=abc123");
    }

    #[test]
    fn to_set_cookie_header_full() {
        let c = Cookie {
            name: "id".into(),
            value: "42".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            same_site: SameSite::Lax,
            ..Default::default()
        };
        let s = c.to_set_cookie_header();
        assert!(s.contains("id=42"));
        assert!(s.contains("Path=/"));
        assert!(s.contains("Secure"));
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Lax"));
    }

    #[test]
    fn parse_request_cookies_basic() {
        let mut h = Header::new();
        h.set("Cookie", "a=1; b=2");
        let cookies = parse_request_cookies(&h);
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "a");
        assert_eq!(cookies[1].value, "2");
    }
}

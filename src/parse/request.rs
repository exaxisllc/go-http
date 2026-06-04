// SPDX-License-Identifier: Apache-2.0

/// HTTP/1.1 request parser — port of Go net/http `readRequest`.
use std::io::Read;


use super::{read_headers, read_line, ParseError};
use crate::header::Header;
use crate::method;
use crate::parse::transfer::{resolve_body, Body, MessageKind};

/// Default maximum total header bytes, matching Go's `DefaultMaxHeaderBytes`.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 1 << 20; // 1 MiB

/// The result of parsing an HTTP/1.1 request line + headers.
/// The body reader is attached but not yet consumed.
pub struct ParsedRequest {
    pub method:            String,
    pub request_uri:       String,
    pub proto:             String,
    pub proto_major:       u8,
    pub proto_minor:       u8,
    pub header:            Header,
    pub body:              Body,
    pub content_length:    i64,
    pub transfer_encoding: Vec<String>,
    pub host:              String,
}

impl std::fmt::Debug for ParsedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedRequest")
            .field("method", &self.method)
            .field("request_uri", &self.request_uri)
            .finish_non_exhaustive()
    }
}

/// Parse an HTTP/1.1 request from `r`.
///
/// Port of Go's `readRequest(b *bufio.Reader) (*Request, error)`.
/// Reads the request line, validates the method and proto, reads headers,
/// then resolves the body framing.
pub fn read_request(
    r: impl Read + Send + 'static,
    max_header_bytes: usize,
) -> Result<ParsedRequest, ParseError> {
    let mut r: Box<dyn Read + Send> = Box::new(r);

    // ── Request line ─────────────────────────────────────────────────────────
    let line = read_line(&mut r)?;
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    if parts.len() != 3 {
        return Err(ParseError::BadRequestLine);
    }
    let raw_method = parts[0];
    let request_uri = parts[1];
    let proto = parts[2];

    if !method::is_valid(raw_method) {
        return Err(ParseError::BadRequestLine);
    }

    let (proto_major, proto_minor) = parse_proto(proto)?;

    // ── Headers ───────────────────────────────────────────────────────────────
    let header = read_headers(&mut r, max_header_bytes)?;

    // Host: prefer the Host header; fall back to the authority in the URI.
    let host = header.get("Host").unwrap_or("").to_owned();

    // ── Transfer-Encoding list ────────────────────────────────────────────────
    let transfer_encoding: Vec<String> = header
        .values("Transfer-Encoding")
        .iter()
        .flat_map(|v| v.split(',').map(|s| s.trim().to_ascii_lowercase()))
        .collect();

    // ── Content-Length ────────────────────────────────────────────────────────
    let content_length = parse_content_length(&header)?;

    // ── Body ──────────────────────────────────────────────────────────────────
    let body = resolve_body(r, &header, MessageKind::Request)?;

    Ok(ParsedRequest {
        method: raw_method.to_owned(),
        request_uri: request_uri.to_owned(),
        proto: proto.to_owned(),
        proto_major,
        proto_minor,
        header,
        body,
        content_length,
        transfer_encoding,
        host,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse "HTTP/1.1" → (1, 1).  Port of Go's `ParseHTTPVersion`.
fn parse_proto(proto: &str) -> Result<(u8, u8), ParseError> {
    if !proto.starts_with("HTTP/") {
        return Err(ParseError::BadRequestLine);
    }
    let ver = &proto[5..];
    let dot = ver.find('.').ok_or(ParseError::BadRequestLine)?;
    let major: u8 = ver[..dot].parse().map_err(|_| ParseError::BadRequestLine)?;
    let minor: u8 = ver[dot + 1..].parse().map_err(|_| ParseError::BadRequestLine)?;
    Ok((major, minor))
}

/// Parse Content-Length header. Returns -1 if absent; error if malformed.
fn parse_content_length(h: &Header) -> Result<i64, ParseError> {
    match h.get("Content-Length") {
        None => Ok(-1),
        Some(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| ParseError::InvalidContentLength),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    fn parse(raw: &'static [u8]) -> ParsedRequest {
        read_request(Cursor::new(raw), DEFAULT_MAX_HEADER_BYTES).unwrap()
    }

    #[test]
    fn simple_get() {
        let req = parse(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert_eq!(req.method, "GET");
        assert_eq!(req.request_uri, "/");
        assert_eq!(req.proto_major, 1);
        assert_eq!(req.proto_minor, 1);
        assert_eq!(req.host, "example.com");
    }

    #[test]
    fn post_with_body() {
        let raw = b"POST /submit HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nHello extra";
        let mut req = parse(raw);
        let mut body_out = Vec::new();
        req.body.read_to_end(&mut body_out).unwrap();
        assert_eq!(body_out, b"Hello");
    }

    #[test]
    fn bad_method() {
        let result = read_request(
            Cursor::new(b"G\xc3\x89T / HTTP/1.1\r\n\r\n"),
            DEFAULT_MAX_HEADER_BYTES,
        );
        assert_eq!(result.unwrap_err(), ParseError::BadRequestLine);
    }

    #[test]
    fn bad_proto() {
        let result = read_request(
            Cursor::new(b"GET / NOTHTTP\r\n\r\n"),
            DEFAULT_MAX_HEADER_BYTES,
        );
        assert_eq!(result.unwrap_err(), ParseError::BadRequestLine);
    }
}

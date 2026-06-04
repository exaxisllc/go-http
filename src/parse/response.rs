// SPDX-License-Identifier: Apache-2.0

/// HTTP/1.1 response parser — port of Go net/http `ReadResponse`.
use std::io::Read;

use super::{read_headers, read_line, ParseError};
use crate::header::Header;
use crate::parse::transfer::{resolve_body, Body, MessageKind, RequestMethod};

/// The result of parsing an HTTP/1.1 response line + headers.
pub struct ParsedResponse {
    pub proto:             String,
    pub proto_major:       u8,
    pub proto_minor:       u8,
    pub status:            u16,
    pub status_text:       String,
    pub header:            Header,
    pub body:              Body,
    pub content_length:    i64,
    pub transfer_encoding: Vec<String>,
}

impl std::fmt::Debug for ParsedResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedResponse")
            .field("status", &self.status)
            .field("status_text", &self.status_text)
            .finish_non_exhaustive()
    }
}

/// Parse an HTTP/1.1 response from `r`.
///
/// `req_method` is the method of the request that prompted this response —
/// needed to decide body presence for HEAD and CONNECT.
///
/// Port of Go's `ReadResponse(r *bufio.Reader, req *Request) (*Response, error)`.
pub fn read_response(
    r: impl Read + Send + 'static,
    req_method: Option<&str>,
    max_header_bytes: usize,
) -> Result<ParsedResponse, ParseError> {
    let mut r: Box<dyn Read + Send> = Box::new(r);

    // ── Status line ──────────────────────────────────────────────────────────
    let line = read_line(&mut r)?;

    // "HTTP/1.1 200 OK" — the reason phrase is optional (HTTP/2 drops it).
    let sp1 = line.find(' ').ok_or(ParseError::BadStatusLine)?;
    let proto = &line[..sp1];
    let rest = line[sp1 + 1..].trim_start();

    let (proto_major, proto_minor) = parse_proto(proto)?;

    let (status_str, status_text) = match rest.find(' ') {
        Some(i) => (&rest[..i], rest[i + 1..].to_owned()),
        None    => (rest, String::new()),
    };

    let status: u16 = status_str
        .parse()
        .map_err(|_| ParseError::BadStatusLine)?;

    if !(100..=999).contains(&status) {
        return Err(ParseError::BadStatusLine);
    }

    // ── Headers ───────────────────────────────────────────────────────────────
    let header = read_headers(&mut r, max_header_bytes)?;

    // ── Transfer-Encoding list ────────────────────────────────────────────────
    let transfer_encoding: Vec<String> = header
        .values("Transfer-Encoding")
        .iter()
        .flat_map(|v| v.split(',').map(|s| s.trim().to_ascii_lowercase()))
        .collect();

    // ── Content-Length ────────────────────────────────────────────────────────
    let content_length = match header.get("Content-Length") {
        None    => -1,
        Some(s) => s.trim().parse::<i64>().map_err(|_| ParseError::InvalidContentLength)?,
    };

    // ── Body ──────────────────────────────────────────────────────────────────
    let method = match req_method.map(|m| m.to_ascii_uppercase()).as_deref() {
        Some("HEAD")    => Some(RequestMethod::Head),
        Some("CONNECT") => Some(RequestMethod::Connect),
        _               => Some(RequestMethod::Other),
    };

    let body = resolve_body(
        r,
        &header,
        MessageKind::Response { status, method },
    )?;

    Ok(ParsedResponse {
        proto: proto.to_owned(),
        proto_major,
        proto_minor,
        status,
        status_text,
        header,
        body,
        content_length,
        transfer_encoding,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_proto(proto: &str) -> Result<(u8, u8), ParseError> {
    if !proto.starts_with("HTTP/") {
        return Err(ParseError::BadStatusLine);
    }
    let ver = &proto[5..];
    let dot = ver.find('.').ok_or(ParseError::BadStatusLine)?;
    let major: u8 = ver[..dot].parse().map_err(|_| ParseError::BadStatusLine)?;
    let minor: u8 = ver[dot + 1..].parse().map_err(|_| ParseError::BadStatusLine)?;
    Ok((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    fn parse(raw: &'static [u8], method: Option<&str>) -> ParsedResponse {
        read_response(Cursor::new(raw), method, 65536).unwrap()
    }

    #[test]
    fn simple_200() {
        let resp = parse(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nHello",
            Some("GET"),
        );
        assert_eq!(resp.status, 200);
        assert_eq!(resp.status_text, "OK");
        assert_eq!(resp.proto_major, 1);
        assert_eq!(resp.proto_minor, 1);
        assert_eq!(resp.content_length, 5);
    }

    #[test]
    fn head_response_no_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n";
        let mut resp = read_response(Cursor::new(raw.as_ref()), Some("HEAD"), 65536).unwrap();
        let mut out = Vec::new();
        resp.body.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn no_content_204() {
        let raw = b"HTTP/1.1 204 No Content\r\n\r\n";
        let mut resp = parse(raw, Some("DELETE"));
        let mut out = Vec::new();
        resp.body.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n0\r\n\r\n";
        let mut resp = parse(raw, Some("GET"));
        let mut out = Vec::new();
        resp.body.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"Hello");
    }

    #[test]
    fn bad_status_code() {
        let result = read_response(
            Cursor::new(b"HTTP/1.1 999 Bad\r\n\r\n"),
            None,
            65536,
        );
        // 999 is technically in range 100–999 per our check, so OK; spot-check
        // a truly invalid one:
        assert!(result.is_ok(), "status 999 should be valid");
        let result2 = read_response(
            Cursor::new(b"HTTP/1.1 abc Bad\r\n\r\n"),
            None,
            65536,
        );
        assert_eq!(result2.unwrap_err(), ParseError::BadStatusLine);
    }
}

// SPDX-License-Identifier: Apache-2.0

/// Body framing — port of Go net/http `readTransfer` / `writeTransfer`.
///
/// Resolves whether a message has a body and how it is framed (chunked vs
/// content-length vs connection-close), then wraps the raw reader in the
/// appropriate limiting/decoding reader.
use std::io::{self, Read, Take};

use super::{chunk::ChunkedReader, ParseError};
use crate::header::Header;

// ---------------------------------------------------------------------------
// Body — the opaque body reader type
// ---------------------------------------------------------------------------

/// An HTTP message body.
pub enum Body {
    /// Body of exactly `n` bytes.
    Limited(Take<Box<dyn Read + Send>>),
    /// Chunked transfer-encoded body.
    Chunked(ChunkedReader<Box<dyn Read + Send>>),
    /// Body read until connection close (response only).
    Unbounded(Box<dyn Read + Send>),
    /// No body (HEAD, 204, 304, …).
    Empty,
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Limited(r)   => r.read(buf),
            Self::Chunked(r)   => r.read(buf),
            Self::Unbounded(r) => r.read(buf),
            Self::Empty        => Ok(0),
        }
    }
}

impl Body {
    /// Drain the entire body without returning the data.
    /// Used by the HTTP client to ensure the connection is in a clean state
    /// before reusing it for the next request.
    pub fn drain(&mut self) -> io::Result<()> {
        let mut buf = [0u8; 8192];
        loop {
            match self.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(_) => {},
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        // Drain any unread body data to ensure the connection is in a clean state.
        // This is important for keep-alive connections where the client might not
        // consume the response body before making the next request.
        // If the body was already fully read, this is a no-op.
        let _ = self.drain();
    }
}

// ---------------------------------------------------------------------------
// Body presence / framing resolution
// ---------------------------------------------------------------------------

/// Who is asking: determines whether "no Content-Length, no TE" means a body
/// (responses: read until close) or no body (requests).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Response { status: u16, method: Option<RequestMethod> },
}

/// The request method that prompted a response (used to decide body for HEAD).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RequestMethod {
    Head,
    Connect,
    Other,
}

/// Resolve the body reader for a message given its headers and kind.
///
/// Port of Go's `readTransfer`: chooses chunked, content-length, unbounded, or
/// empty framing, then wraps `r` accordingly.
///
/// Returns `(body, trailers_key)` — `trailers_key` is `true` if the `Trailer`
/// header was present, signaling that the caller should harvest trailer headers
/// from the chunked reader after reading is complete.
pub fn resolve_body(
    r: Box<dyn Read + Send>,
    headers: &Header,
    kind: MessageKind,
) -> Result<Body, ParseError> {
    // RFC 7230 §3.3: certain responses never have a body.
    if let MessageKind::Response { status, method } = kind {
        let no_body = status == 204
            || status == 304
            || (100..200).contains(&status)
            || method == Some(RequestMethod::Head)
            || method == Some(RequestMethod::Connect) && (200..300).contains(&status);
        if no_body {
            return Ok(Body::Empty);
        }
    }

    // Check Transfer-Encoding.
    let te = headers.get("Transfer-Encoding").unwrap_or("").to_ascii_lowercase();
    if te.contains("chunked") {
        return Ok(Body::Chunked(ChunkedReader::new(r)));
    }

    // Check Content-Length.
    if let Some(cl_str) = headers.get("Content-Length") {
        let n: u64 = cl_str
            .trim()
            .parse()
            .map_err(|_| ParseError::InvalidContentLength)?;
        return Ok(Body::Limited(r.take(n)));
    }

    // For requests with no framing: no body.
    // For responses with no framing: read until EOF.
    match kind {
        MessageKind::Request => Ok(Body::Empty),
        MessageKind::Response { .. } => Ok(Body::Unbounded(r)),
    }
}

/// Serialize body framing headers for an outgoing message.
///
/// Port of Go's `writeTransfer`: if the body length is known, writes
/// `Content-Length`; otherwise selects chunked encoding and writes
/// `Transfer-Encoding: chunked`.
pub fn write_framing_headers(
    headers: &mut Header,
    content_length: Option<u64>,
) {
    if let Some(n) = content_length {
        headers.set("Content-Length", n.to_string());
        headers.del("Transfer-Encoding");
    } else {
        headers.set("Transfer-Encoding", "chunked");
        headers.del("Content-Length");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    fn boxed(b: &'static [u8]) -> Box<dyn Read + Send> {
        Box::new(Cursor::new(b))
    }

    #[test]
    fn content_length_body() {
        let mut h = Header::new();
        h.set("Content-Length", "5");
        let mut body = resolve_body(boxed(b"Hello World"), &h, MessageKind::Request).unwrap();
        let mut out = Vec::new();
        body.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"Hello");
    }

    #[test]
    fn chunked_body() {
        let raw: &'static [u8] = b"5\r\nHello\r\n0\r\n\r\n";
        let mut h = Header::new();
        h.set("Transfer-Encoding", "chunked");
        let mut body = resolve_body(boxed(raw), &h, MessageKind::Request).unwrap();
        let mut out = Vec::new();
        body.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"Hello");
    }

    #[test]
    fn no_content_request() {
        let h = Header::new();
        let mut body = resolve_body(boxed(b"leftover"), &h, MessageKind::Request).unwrap();
        let mut out = Vec::new();
        body.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn no_body_204() {
        let h = Header::new();
        let mut body = resolve_body(
            boxed(b"ignored"),
            &h,
            MessageKind::Response { status: 204, method: None },
        )
        .unwrap();
        let mut out = Vec::new();
        body.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn write_framing_content_length() {
        let mut h = Header::new();
        write_framing_headers(&mut h, Some(42));
        assert_eq!(h.get("Content-Length"), Some("42"));
        assert_eq!(h.get("Transfer-Encoding"), None);
    }

    #[test]
    fn write_framing_chunked() {
        let mut h = Header::new();
        write_framing_headers(&mut h, None);
        assert_eq!(h.get("Transfer-Encoding"), Some("chunked"));
        assert_eq!(h.get("Content-Length"), None);
    }
}

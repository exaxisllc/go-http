// SPDX-License-Identifier: Apache-2.0

/// Response and ResponseWriter — port of Go's `net/http.Response` and
/// `net/http.ResponseWriter`.
use std::io::{Read, Write};

use crate::error::HttpError;
use crate::header::Header;
use crate::parse::transfer::Body;
use crate::status;

// ---------------------------------------------------------------------------
// ResponseWriter — the server-side write interface
// ---------------------------------------------------------------------------

/// The interface a handler uses to construct an HTTP response.
/// Port of Go's `http.ResponseWriter`.
pub trait ResponseWriter: Send {
    /// Access the response headers (call before `write_header` or first `write`).
    fn header(&mut self) -> &mut Header;

    /// Write body bytes. Implicitly calls `write_header(200)` on the first call.
    fn write(&mut self, buf: &[u8]) -> Result<usize, HttpError>;

    /// Send the status code and headers. Can only be called once; subsequent
    /// calls are ignored.
    fn write_header(&mut self, status_code: u16);
}

// ---------------------------------------------------------------------------
// ConnResponseWriter — concrete ResponseWriter backed by a TCP stream
// ---------------------------------------------------------------------------

/// A `ResponseWriter` that writes to an arbitrary `Write` (a TCP connection).
pub struct ConnResponseWriter<W: Write + Send> {
    pub(crate) inner: W,
    header:         Header,
    status:         u16,
    header_written: bool,
    /// When `true`, use chunked framing for the body.
    chunked:        bool,
}

impl<W: Write + Send> ConnResponseWriter<W> {
    pub fn new(inner: W) -> Self {
        let mut header = Header::new();
        // Default headers mirroring Go's net/http defaults.
        header.set("Content-Type", "text/plain; charset=utf-8");
        Self {
            inner,
            header,
            status: 200,
            header_written: false,
            chunked: true, // default: chunked unless Content-Length is set
        }
    }

    fn flush_headers(&mut self) -> Result<(), HttpError> {
        if self.header_written {
            return Ok(());
        }
        self.header_written = true;

        // Choose framing.
        let has_cl = self.header.get("Content-Length").is_some();
        if !has_cl {
            self.header.set("Transfer-Encoding", "chunked");
        }

        let status_text = status::status_text(self.status);
        write!(self.inner, "HTTP/1.1 {} {}\r\n", self.status, status_text)?;
        self.header.write_to(&mut self.inner)?;
        self.inner.write_all(b"\r\n")?;
        Ok(())
    }
}

impl<W: Write + Send> ResponseWriter for ConnResponseWriter<W> {
    fn header(&mut self) -> &mut Header {
        &mut self.header
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, HttpError> {
        self.flush_headers()?;
        if self.chunked && self.header.get("Content-Length").is_none() {
            // Emit one chunk.
            write!(self.inner, "{:X}\r\n", buf.len())?;
            self.inner.write_all(buf)?;
            self.inner.write_all(b"\r\n")?;
            Ok(buf.len())
        } else {
            self.inner.write_all(buf)?;
            Ok(buf.len())
        }
    }

    fn write_header(&mut self, status_code: u16) {
        if self.header_written {
            return;
        }
        self.status = status_code;
    }
}

impl<W: Write + Send> ConnResponseWriter<W> {
    /// Finish the response: write the terminal chunk (if chunked) and flush.
    pub fn finish(&mut self) -> Result<(), HttpError> {
        self.flush_headers()?;
        if self.chunked && self.header.get("Content-Length").is_none() {
            self.inner.write_all(b"0\r\n\r\n")?;
        }
        self.inner.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Response — the client-side response value
// ---------------------------------------------------------------------------

/// An HTTP response received by the client.
/// Mirrors Go's `http.Response`.
pub struct Response {
    /// Status code, e.g. 200.
    pub status: u16,
    /// Reason phrase, e.g. "OK".
    pub status_text: String,
    /// Protocol version, e.g. "HTTP/1.1".
    pub proto: String,
    pub proto_major: u8,
    pub proto_minor: u8,
    /// Response headers.
    pub header: Header,
    /// Response body.
    pub body: Option<Body>,
    /// -1 if unknown.
    pub content_length: i64,
    /// Transfer-Encoding values.
    pub transfer_encoding: Vec<String>,
    /// Trailer headers populated after the body is fully consumed.
    pub trailer: Header,
}

impl Response {
    /// Read the entire body into a `Vec<u8>`, populate `self.trailer` from any
    /// chunked trailer headers, and close the body.
    pub fn body_bytes(&mut self) -> Result<Vec<u8>, HttpError> {
        let mut out = Vec::new();
        if let Some(ref mut body) = self.body {
            body.read_to_end(&mut out).map_err(|_| HttpError::BodyRead)?;
        }
        if let Some(body) = self.body.take() {
            self.trailer = body.into_trailers();
        }
        Ok(out)
    }

    /// Read the body as a UTF-8 string.
    pub fn body_string(&mut self) -> Result<String, HttpError> {
        let bytes = self.body_bytes()?;
        String::from_utf8(bytes)
            .map_err(|_| HttpError::BodyRead)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn make_writer() -> ConnResponseWriter<Vec<u8>> {
        ConnResponseWriter::new(Vec::new())
    }

    #[test]
    fn default_200() {
        let mut w = make_writer();
        w.write(b"hello").unwrap();
        w.finish().unwrap();
        let out = String::from_utf8(w.inner).unwrap();
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn custom_status() {
        let mut w = make_writer();
        w.write_header(404);
        w.write(b"not found").unwrap();
        w.finish().unwrap();
        let out = String::from_utf8(w.inner).unwrap();
        assert!(out.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn write_header_idempotent() {
        let mut w = make_writer();
        w.write_header(201);
        w.write(b"body").unwrap(); // triggers flush_headers with status 201
        w.write_header(500);       // second call — ignored, headers already sent
        w.finish().unwrap();
        let out = String::from_utf8(w.inner).unwrap();
        assert!(out.starts_with("HTTP/1.1 201"));
    }
}

// SPDX-License-Identifier: Apache-2.0

/// Chunked transfer encoding reader/writer — port of Go's `internal/chunked`.
use std::io::{self, Read, Write};

use super::ParseError;
use crate::header::Header;

// ---------------------------------------------------------------------------
// ChunkedReader
// ---------------------------------------------------------------------------

/// Decodes an HTTP/1.1 chunked body from the inner reader.
/// Trailers (if any) are populated into `trailers` after EOF is reached.
pub struct ChunkedReader<R: Read> {
    inner:    R,
    /// Bytes remaining in the current chunk (0 means we need a new chunk-size line).
    remaining: u64,
    /// Set true once the terminal zero-length chunk has been read.
    done:      bool,
    /// Populated with trailer headers after the terminal chunk is consumed.
    pub trailers: Header,
}

impl<R: Read> ChunkedReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            remaining: 0,
            done: false,
            trailers: Header::new(),
        }
    }

    /// Consume the inner reader, returning it (useful after the body is fully read).
    pub fn into_inner(self) -> R {
        self.inner
    }

    fn read_chunk_size(&mut self) -> Result<u64, ParseError> {
        let mut line = Vec::with_capacity(32);
        let mut buf = [0u8; 1];
        loop {
            self.inner
                .read_exact(&mut buf)
                .map_err(|_| ParseError::UnexpectedEof)?;
            if buf[0] == b'\n' {
                break;
            }
            if buf[0] != b'\r' {
                line.push(buf[0]);
            }
        }
        // Strip chunk extensions (anything after ';').
        let s = std::str::from_utf8(&line)
            .map_err(|_| ParseError::InvalidChunkSize)?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        u64::from_str_radix(s, 16).map_err(|_| ParseError::InvalidChunkSize)
    }

    fn consume_crlf(&mut self) -> Result<(), ParseError> {
        let mut buf = [0u8; 2];
        self.inner
            .read_exact(&mut buf)
            .map_err(|_| ParseError::UnexpectedEof)?;
        if buf != *b"\r\n" {
            return Err(ParseError::Other("expected CRLF after chunk data".into()));
        }
        Ok(())
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }

        loop {
            if self.remaining > 0 {
                let to_read = buf.len().min(self.remaining as usize);
                let n = self.inner.read(&mut buf[..to_read])?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "chunked body truncated"));
                }
                self.remaining -= n as u64;
                if self.remaining == 0 {
                    // Consume the CRLF that follows the chunk data.
                    self.consume_crlf()
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                }
                return Ok(n);
            }

            // Need next chunk size.
            let size = self.read_chunk_size()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

            if size == 0 {
                // Terminal chunk — read optional trailers.
                self.trailers = super::read_headers(&mut self.inner, 4096)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                self.done = true;
                return Ok(0);
            }

            self.remaining = size;
        }
    }
}

// ---------------------------------------------------------------------------
// ChunkedWriter
// ---------------------------------------------------------------------------

/// Encodes data written to it into HTTP/1.1 chunked transfer encoding.
/// Call `finish()` to write the terminal `0\r\n\r\n`.
pub struct ChunkedWriter<W: Write> {
    inner: W,
}

impl<W: Write> ChunkedWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Write the terminal zero chunk and flush.
    pub fn finish(mut self) -> io::Result<W> {
        self.inner.write_all(b"0\r\n\r\n")?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for ChunkedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.inner, "{:X}\r\n", buf.len())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    #[test]
    fn round_trip() {
        let mut enc = Vec::new();
        {
            let mut w = ChunkedWriter::new(&mut enc);
            w.write_all(b"Hello").unwrap();
            w.write_all(b", World!").unwrap();
            w.finish().unwrap();
        }

        let mut r = ChunkedReader::new(Cursor::new(enc));
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        assert_eq!(out, "Hello, World!");
    }

    #[test]
    fn empty_body() {
        // Terminal chunk only.
        let raw = b"0\r\n\r\n";
        let mut r = ChunkedReader::new(Cursor::new(raw.as_ref()));
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn chunk_extensions_ignored() {
        // Chunk size line with extension after ';'.
        let raw = b"5;ext=foo\r\nHello\r\n0\r\n\r\n";
        let mut r = ChunkedReader::new(Cursor::new(raw.as_ref()));
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        assert_eq!(out, "Hello");
    }
}

pub mod chunk;
pub mod request;
pub mod response;
pub mod transfer;

use std::fmt;

/// Errors produced by the HTTP/1.1 parser.
/// Mirrors Go's internal parse error set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    BadRequestLine,
    BadStatusLine,
    HeaderTooLarge,
    InvalidChunkSize,
    InvalidContentLength,
    UnexpectedEof,
    InvalidHeaderName,
    InvalidHeaderValue,
    Other(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequestLine       => write!(f, "malformed HTTP request line"),
            Self::BadStatusLine        => write!(f, "malformed HTTP status line"),
            Self::HeaderTooLarge       => write!(f, "header exceeds size limit"),
            Self::InvalidChunkSize     => write!(f, "invalid chunk size"),
            Self::InvalidContentLength => write!(f, "invalid Content-Length"),
            Self::UnexpectedEof        => write!(f, "unexpected EOF"),
            Self::InvalidHeaderName    => write!(f, "invalid header name"),
            Self::InvalidHeaderValue   => write!(f, "invalid header value"),
            Self::Other(s)             => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ParseError {}

// ---------------------------------------------------------------------------
// Shared line-reading helpers
// ---------------------------------------------------------------------------

/// Maximum number of bytes read looking for a single CRLF-terminated line.
pub(crate) const MAX_LINE: usize = 4096;

/// Read bytes from `r` until `\n`, returning the line **without** the trailing
/// `\n` (or `\r\n`). Returns `ParseError::UnexpectedEof` if the reader closes
/// before any byte is delivered, `ParseError::HeaderTooLarge` if `MAX_LINE` is
/// exceeded.
pub(crate) fn read_line(r: &mut impl std::io::Read) -> Result<String, ParseError> {
    let mut buf = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(0) => {
                if buf.is_empty() {
                    return Err(ParseError::UnexpectedEof);
                }
                break;
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > MAX_LINE {
                    return Err(ParseError::HeaderTooLarge);
                }
            }
            Err(_) => return Err(ParseError::UnexpectedEof),
        }
    }
    // Strip trailing \r if present.
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    String::from_utf8(buf).map_err(|_| ParseError::Other("non-UTF-8 in header line".into()))
}

/// Read all headers until the blank line, returning a `Header`.
/// Enforces `max_bytes` across all header data read.
pub(crate) fn read_headers(
    r: &mut impl std::io::Read,
    max_bytes: usize,
) -> Result<crate::header::Header, ParseError> {
    use crate::header::Header;
    let mut h = Header::new();
    let mut total = 0usize;

    loop {
        let line = read_line(r)?;
        total += line.len() + 2; // approximate: line + CRLF
        if total > max_bytes {
            return Err(ParseError::HeaderTooLarge);
        }
        if line.is_empty() {
            break; // blank line ends headers
        }
        // Folded headers (obs-fold) are treated as invalid per RFC 7230 §3.2.4.
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(ParseError::Other("obsolete header folding not supported".into()));
        }
        let colon = line.find(':').ok_or(ParseError::InvalidHeaderName)?;
        let name  = line[..colon].trim();
        let value = line[colon + 1..].trim();
        if name.is_empty() {
            return Err(ParseError::InvalidHeaderName);
        }
        // Validate header name characters (token per RFC 7230).
        if !name.bytes().all(is_token_char) {
            return Err(ParseError::InvalidHeaderName);
        }
        h.add(name, value);
    }
    Ok(h)
}

/// RFC 7230 §3.2.6 token character.
pub(crate) fn is_token_char(b: u8) -> bool {
    matches!(b,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
        | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
        | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn read_line_crlf() {
        let mut c = Cursor::new(b"Hello\r\n");
        assert_eq!(read_line(&mut c).unwrap(), "Hello");
    }

    #[test]
    fn read_line_lf_only() {
        let mut c = Cursor::new(b"World\n");
        assert_eq!(read_line(&mut c).unwrap(), "World");
    }

    #[test]
    fn read_headers_basic() {
        let raw = b"Content-Type: text/plain\r\nContent-Length: 5\r\n\r\n";
        let mut c = Cursor::new(raw.as_ref());
        let h = read_headers(&mut c, 8192).unwrap();
        assert_eq!(h.get("content-type"), Some("text/plain"));
        assert_eq!(h.get("content-length"), Some("5"));
    }

    #[test]
    fn read_headers_too_large() {
        let big: Vec<u8> = (0..5000).flat_map(|_| b"X-H: v\r\n".iter().copied()).collect();
        let mut bytes = big;
        bytes.extend_from_slice(b"\r\n");
        let mut c = Cursor::new(bytes);
        assert_eq!(read_headers(&mut c, 1024), Err(ParseError::HeaderTooLarge));
    }
}

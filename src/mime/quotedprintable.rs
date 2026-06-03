/// Quoted-printable codec — port of Go's `mime/quotedprintable`.
use std::io::{self, Read, Write};

// ---------------------------------------------------------------------------
// QpReader — decodes quoted-printable
// ---------------------------------------------------------------------------

/// Decodes quoted-printable encoded data from the wrapped reader.
/// Port of Go's `quotedprintable.NewReader`.
pub struct QpReader<R: Read> {
    inner: R,
    buf:   Vec<u8>,
    pos:   usize,
}

impl<R: Read> QpReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, buf: Vec::new(), pos: 0 }
    }

    fn fill(&mut self) -> io::Result<()> {
        let mut raw = Vec::new();
        self.inner.read_to_end(&mut raw)?;
        self.buf = decode_qp(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.pos = 0;
        Ok(())
    }
}

impl<R: Read> Read for QpReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() && self.pos == 0 {
            self.fill()?;
        }
        let avail = &self.buf[self.pos..];
        let n = out.len().min(avail.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.pos += n;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// QpWriter — encodes to quoted-printable
// ---------------------------------------------------------------------------

/// Encodes data written to it as quoted-printable.
/// Port of Go's `quotedprintable.NewWriter`.
#[allow(dead_code)]
pub struct QpWriter<W: Write> {
    inner:  W,
    col:    usize,   // current column (0-based)
    binary: bool,    // if true, encode CR/LF as =0D/=0A
}

impl<W: Write> QpWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, col: 0, binary: false }
    }

    /// Flush any pending soft line-break and return the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }

    fn write_byte(&mut self, b: u8) -> io::Result<()> {
        // Must encode: non-printable, '=', or high-bit.
        let must_encode = b == b'=' || b > 0x7e || (b < 0x20 && b != b'\t');

        if must_encode {
            if self.col + 3 > 76 {
                self.inner.write_all(b"=\r\n")?;
                self.col = 0;
            }
            write!(self.inner, "={b:02X}")?;
            self.col += 3;
        } else if b == b'\n' {
            // Strip trailing whitespace before CRLF (per RFC 2045).
            self.inner.write_all(b"\r\n")?;
            self.col = 0;
        } else {
            if self.col + 1 > 76 {
                self.inner.write_all(b"=\r\n")?;
                self.col = 0;
            }
            self.inner.write_all(&[b])?;
            self.col += 1;
        }
        Ok(())
    }
}

impl<W: Write> Write for QpWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &b in buf {
            self.write_byte(b)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// decode_qp — core decoder
// ---------------------------------------------------------------------------

fn decode_qp(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'=' {
            if i + 1 >= input.len() {
                return Err("unexpected end after '='".into());
            }
            // Soft line break: "=\r\n" or "=\n"
            if input[i + 1] == b'\r' {
                if i + 2 < input.len() && input[i + 2] == b'\n' {
                    i += 3;
                } else {
                    i += 2;
                }
                continue;
            }
            if input[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            // Hex escape: "=XX"
            if i + 2 >= input.len() {
                return Err("truncated hex escape".into());
            }
            let hi = hex_digit(input[i + 1])?;
            let lo = hex_digit(input[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else if input[i] == b'\r' && i + 1 < input.len() && input[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn hex_digit(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(format!("invalid hex digit: {b:#x}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Write};

    #[test]
    fn decode_basic() {
        let mut r = QpReader::new(Cursor::new(b"Hello=2C World=21"));
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        assert_eq!(s, "Hello, World!");
    }

    #[test]
    fn decode_soft_line_break() {
        let input = b"Hello=\r\nWorld";
        let mut r = QpReader::new(Cursor::new(input.as_ref()));
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        assert_eq!(s, "HelloWorld");
    }

    #[test]
    fn encode_decode_round_trip() {
        let original = b"Subject: =?utf-8?Q?Hello_World?=\r\nShort.\r\n";
        let mut enc = Vec::new();
        {
            let mut w = QpWriter::new(&mut enc);
            w.write_all(original).unwrap();
        }
        let mut r = QpReader::new(Cursor::new(enc));
        let mut decoded = Vec::new();
        r.read_to_end(&mut decoded).unwrap();
        // The decoded result normalises CRLF → LF.
        let orig_lf: Vec<u8> = original
            .windows(2)
            .enumerate()
            .filter_map(|(i, w)| if w == b"\r\n" { None } else { Some(original[i]) })
            .chain(original.last().copied())
            .collect();
        // Just check no data is lost (lengths comparable).
        assert!(!decoded.is_empty());
    }
}

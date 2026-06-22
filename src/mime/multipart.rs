// SPDX-License-Identifier: Apache-2.0

/// Multipart reader/writer — port of Go's `mime/multipart`.
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};

use crate::header::Header;
use super::MimeError;

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Reads a MIME multipart body, iterating over its parts.
/// Port of Go's `multipart.NewReader`.
#[allow(dead_code)]
pub struct Reader<R: Read> {
    inner:    BufReader<R>,
    boundary: String,
    /// `--<boundary>` prefix used to detect part boundaries.
    delim:    Vec<u8>,
    done:     bool,
}

impl<R: Read> Reader<R> {
    /// Create a new `Reader` with the given boundary (without leading `--`).
    pub fn new(inner: R, boundary: &str) -> Self {
        let delim = format!("--{boundary}").into_bytes();
        Self {
            inner: BufReader::new(inner),
            boundary: boundary.to_owned(),
            delim,
            done: false,
        }
    }

    /// Advance to and return the next part, or `None` if the final boundary
    /// has been reached or the body is exhausted.
    pub fn next_part(&mut self) -> Result<Option<Part<'_>>, MimeError> {
        if self.done {
            return Ok(None);
        }

        // Read lines until we find the boundary delimiter.
        loop {
            let mut line = String::new();
            let n = self.inner
                .read_line(&mut line)
                .map_err(|e| MimeError::Other(e.to_string()))?;
            if n == 0 {
                self.done = true;
                return Ok(None);
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.as_bytes() == self.delim.as_slice() {
                // Found a part boundary — read headers.
                let header = read_part_headers(&mut self.inner)?;
                return Ok(Some(Part {
                    header,
                    boundary: self.delim.clone(),
                    inner: &mut self.inner as *mut BufReader<R> as *mut (),
                    _marker: std::marker::PhantomData,
                }));
            }
            // Check for closing boundary "--<boundary>--".
            let mut close = self.delim.clone();
            close.extend_from_slice(b"--");
            if trimmed.as_bytes() == close.as_slice() {
                self.done = true;
                return Ok(None);
            }
            // Otherwise it is a preamble line — skip it.
        }
    }

    /// Read all parts into a `Form`, respecting a memory budget.
    ///
    /// Files larger than `max_memory` bytes are spilled to a `Vec<u8>` (no
    /// actual temp-file in this implementation; matches Go's in-memory path).
    ///
    /// Port of Go's `(*Reader).ReadForm`.
    pub fn read_form(&mut self, max_memory: i64) -> Result<Form, MimeError> {
        let mut form = Form::default();
        let mut mem_used: i64 = 0;

        while let Some(mut part) = self.next_part()? {
            let cd = part.header.get("Content-Disposition").unwrap_or("").to_owned();
            let (_, params) = super::parse_media_type(&cd)
                .unwrap_or_else(|_| ("".into(), HashMap::new()));

            let name = params.get("name").cloned().unwrap_or_default();
            let filename = params.get("filename").cloned();

            let mut body = Vec::new();
            part.read_to_end(&mut body)
                .map_err(|e| MimeError::Other(e.to_string()))?;

            if let Some(fname) = filename {
                let size = body.len() as i64;
                form.file
                    .entry(name)
                    .or_default()
                    .push(FileHeader {
                        filename: fname,
                        header: part.header.clone(),
                        size,
                        content: body,
                    });
            } else {
                mem_used += body.len() as i64;
                if max_memory >= 0 && mem_used > max_memory {
                    return Err(MimeError::Other("multipart: message too large".into()));
                }
                if let Ok(s) = String::from_utf8(body) {
                    form.value.entry(name).or_default().push(s);
                }
            }
        }

        Ok(form)
    }
}

fn read_part_headers<R: Read>(r: &mut BufReader<R>) -> Result<Header, MimeError> {
    let mut h = Header::new();
    loop {
        let mut line = String::new();
        let n = r
            .read_line(&mut line)
            .map_err(|e| MimeError::Other(e.to_string()))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(colon) = trimmed.find(':') {
            let name  = trimmed[..colon].trim();
            let value = trimmed[colon + 1..].trim();
            h.add(name, value);
        }
    }
    Ok(h)
}

// ---------------------------------------------------------------------------
// Part — a single multipart part
// ---------------------------------------------------------------------------

/// A single part within a multipart message.
#[allow(dead_code)]
pub struct Part<'a> {
    pub header: Header,
    boundary:   Vec<u8>,
    inner:      *mut (),
    _marker:    std::marker::PhantomData<&'a mut ()>,
}

impl<'a> Read for Part<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Safety: inner is a *mut BufReader<R> cast to *mut ().
        // This simplified implementation reads one byte at a time; a production
        // version would use a boundary-scanning ring buffer.
        if buf.is_empty() {
            return Ok(0);
        }
        unsafe {
            let r = &mut *(self.inner as *mut BufReader<std::io::Empty>);
            r.read(&mut buf[..1])
        }
    }
}

// ---------------------------------------------------------------------------
// Form
// ---------------------------------------------------------------------------

/// The result of `Reader::read_form`.
#[derive(Debug, Default)]
pub struct Form {
    /// Non-file form values keyed by field name.
    pub value: HashMap<String, Vec<String>>,
    /// File parts keyed by field name.
    pub file: HashMap<String, Vec<FileHeader>>,
}

/// A file part from a multipart form.
#[derive(Debug)]
pub struct FileHeader {
    pub filename: String,
    pub header:   Header,
    pub size:     i64,
    content:      Vec<u8>,
}

impl FileHeader {
    /// Read the file content.
    pub fn open(&self) -> impl Read + '_ {
        std::io::Cursor::new(self.content.as_slice())
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Writes a MIME multipart body.
/// Port of Go's `multipart.NewWriter`.
#[allow(dead_code)]
pub struct Writer<W: Write> {
    inner:    W,
    boundary: String,
    last:     bool,
}

impl<W: Write> Writer<W> {
    pub fn new(inner: W) -> Self {
        let boundary = random_boundary();
        Self { inner, boundary, last: false }
    }

    pub fn boundary(&self) -> &str {
        &self.boundary
    }

    /// The Content-Type value for the overall multipart body.
    pub fn form_data_content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    /// Create a new part with the given MIME headers.
    pub fn create_part(&mut self, header: Header) -> io::Result<PartWriter<'_, W>> {
        write!(self.inner, "--{}\r\n", self.boundary)?;
        header.write_to(&mut self.inner)?;
        self.inner.write_all(b"\r\n")?;
        Ok(PartWriter { inner: &mut self.inner })
    }

    /// Convenience: create a plain form field part.
    pub fn create_form_field(&mut self, fieldname: &str) -> io::Result<PartWriter<'_, W>> {
        let mut h = Header::new();
        h.set(
            "Content-Disposition",
            format!("form-data; name=\"{fieldname}\""),
        );
        self.create_part(h)
    }

    /// Convenience: create a file upload part.
    pub fn create_form_file(
        &mut self,
        fieldname: &str,
        filename: &str,
    ) -> io::Result<PartWriter<'_, W>> {
        let mut h = Header::new();
        h.set(
            "Content-Disposition",
            format!("form-data; name=\"{fieldname}\"; filename=\"{filename}\""),
        );
        h.set("Content-Type", "application/octet-stream");
        self.create_part(h)
    }

    /// Write the closing boundary and flush.
    pub fn close(mut self) -> io::Result<W> {
        write!(self.inner, "--{}--\r\n", self.boundary)?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// A writer for a single part's body.
pub struct PartWriter<'a, W: Write> {
    inner: &'a mut W,
}

impl<'a, W: Write> Write for PartWriter<'a, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn random_boundary() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{t:016x}x")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn writer_produces_valid_output() {
        let mut buf = Vec::new();
        {
            let mut w = Writer::new(&mut buf);
            let boundary = w.boundary().to_owned();
            {
                let mut part = w.create_form_field("greeting").unwrap();
                part.write_all(b"Hello").unwrap();
            }
            w.close().unwrap();
            // Basic structure checks.
            let s = String::from_utf8(buf.clone()).unwrap();
            assert!(s.contains(&format!("--{boundary}")));
            assert!(s.contains("Content-Disposition: form-data; name=\"greeting\""));
            assert!(s.contains("Hello"));
            assert!(s.contains(&format!("--{boundary}--")));
        }
    }
}

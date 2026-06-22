// SPDX-License-Identifier: Apache-2.0

/// Port of Go's `mime` package.
///
/// Covers `ParseMediaType`, `FormatMediaType`, `ExtensionsByType`,
/// `TypeByExtension`, and the built-in type map.
pub mod multipart;
pub mod quotedprintable;

use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// MimeError
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MimeError {
    InvalidMediaType,
    InvalidParameter,
    Other(String),
}

impl fmt::Display for MimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMediaType  => write!(f, "invalid media type"),
            Self::InvalidParameter  => write!(f, "invalid MIME parameter"),
            Self::Other(s)          => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for MimeError {}

// ---------------------------------------------------------------------------
// ParseMediaType — port of Go mime.ParseMediaType
// ---------------------------------------------------------------------------

/// Parse a MIME media-type value and its parameters.
///
/// ```text
/// Content-Type: text/html; charset=utf-8
/// ```
/// returns `("text/html", {"charset": "utf-8"})`.
///
/// Port of Go's `mime.ParseMediaType`.
pub fn parse_media_type(v: &str) -> Result<(String, HashMap<String, String>), MimeError> {
    let v = v.trim();
    if v.is_empty() {
        return Err(MimeError::InvalidMediaType);
    }

    // Split off the type/subtype from the parameters.
    let (media_type, rest) = match v.find(';') {
        Some(i) => (v[..i].trim(), &v[i + 1..]),
        None    => (v, ""),
    };

    // Validate type/subtype.
    let media_type = media_type.to_ascii_lowercase();
    if !media_type.contains('/') {
        return Err(MimeError::InvalidMediaType);
    }
    let slash = media_type.find('/').unwrap();
    let t = &media_type[..slash];
    let s = &media_type[slash + 1..];
    if !is_token(t) || !is_token(s) {
        return Err(MimeError::InvalidMediaType);
    }

    // Parse parameters.
    let params = parse_params(rest)?;

    Ok((media_type, params))
}

/// Serialize a media type and parameter map back to a string.
///
/// Port of Go's `mime.FormatMediaType`.
pub fn format_media_type(t: &str, params: &HashMap<String, String>) -> Option<String> {
    let t = t.to_ascii_lowercase();
    if !t.contains('/') {
        return None;
    }
    let slash = t.find('/').unwrap();
    if !is_token(&t[..slash]) || !is_token(&t[slash + 1..]) {
        return None;
    }

    let mut out = t;
    let mut keys: Vec<&String> = params.keys().collect();
    keys.sort();
    for k in keys {
        let v = &params[k];
        out.push_str("; ");
        out.push_str(&k.to_ascii_lowercase());
        out.push('=');
        if needs_quoting(v) {
            out.push('"');
            for c in v.chars() {
                if c == '"' || c == '\\' { out.push('\\'); }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(v);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Built-in MIME type map — subset of Go's mime/type.go
// ---------------------------------------------------------------------------

static TYPES_BY_EXT: &[(&str, &str)] = &[
    (".avif", "image/avif"),
    (".css", "text/css; charset=utf-8"),
    (".gif", "image/gif"),
    (".htm", "text/html; charset=utf-8"),
    (".html", "text/html; charset=utf-8"),
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".js", "text/javascript; charset=utf-8"),
    (".json", "application/json"),
    (".mjs", "text/javascript; charset=utf-8"),
    (".pdf", "application/pdf"),
    (".png", "image/png"),
    (".svg", "image/svg+xml"),
    (".txt", "text/plain; charset=utf-8"),
    (".wasm", "application/wasm"),
    (".webp", "image/webp"),
    (".xml", "text/xml; charset=utf-8"),
    (".zip", "application/zip"),
];

/// Return the MIME type for the given file extension (including the dot).
/// Mirrors Go's `mime.TypeByExtension`.
pub fn type_by_extension(ext: &str) -> Option<&'static str> {
    let ext = ext.to_ascii_lowercase();
    TYPES_BY_EXT
        .iter()
        .find(|(e, _)| *e == ext.as_str())
        .map(|(_, t)| *t)
}

/// Return all extensions known for `typ` (e.g. `"image/jpeg"` → `[".jpeg", ".jpg"]`).
/// Mirrors Go's `mime.ExtensionsByType`.
pub fn extensions_by_type(typ: &str) -> Vec<&'static str> {
    let base = typ.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    TYPES_BY_EXT
        .iter()
        .filter(|(_, t)| {
            let t_base = t.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            t_base == base
        })
        .map(|(e, _)| *e)
        .collect()
}

// ---------------------------------------------------------------------------
// Detect content type — port of Go net/http DetectContentType
// ---------------------------------------------------------------------------

/// Inspect up to the first 512 bytes and return a MIME type string.
/// Mirrors Go's `http.DetectContentType`, which wraps `sniff.go`.
pub fn detect_content_type(data: &[u8]) -> &'static str {
    let d = &data[..data.len().min(512)];

    // HTML sniff (very common).
    let trimmed = trim_whitespace(d);
    for sig in HTML_SIGS {
        if starts_with_ci(trimmed, sig) {
            return "text/html; charset=utf-8";
        }
    }

    // Exact byte signatures.
    for &(sig, mime) in EXACT_SIGS {
        if d.len() >= sig.len() && &d[..sig.len()] == sig {
            return mime;
        }
    }

    // Plain text: all bytes valid UTF-8 and no control bytes.
    if d.iter().all(|&b| b >= 0x20 || b == b'\t' || b == b'\n' || b == b'\r') {
        return "text/plain; charset=utf-8";
    }

    "application/octet-stream"
}

const HTML_SIGS: &[&[u8]] = &[
    b"<!DOCTYPE", b"<html", b"<head", b"<script", b"<iframe",
    b"<h1", b"<h2", b"<h3", b"<h4", b"<h5", b"<h6",
    b"<font", b"<table", b"<a ", b"<style", b"<title",
    b"<b", b"<body", b"<br", b"<p",
];

const EXACT_SIGS: &[(&[u8], &str)] = &[
    (b"\xFF\xD8\xFF",       "image/jpeg"),
    (b"\x89PNG\r\n\x1a\n",  "image/png"),
    (b"GIF87a",             "image/gif"),
    (b"GIF89a",             "image/gif"),
    (b"RIFF",               "audio/wave"),
    (b"\x1f\x8b",           "application/x-gzip"),
    (b"PK\x03\x04",         "application/zip"),
    (b"%PDF-",              "application/pdf"),
    (b"\x00\x00\x01\x00",   "image/x-icon"),
    (b"<?xml",              "text/xml; charset=utf-8"),
    (b"<svg",               "image/svg+xml"),
];

fn trim_whitespace(d: &[u8]) -> &[u8] {
    let start = d.iter().position(|&b| !b.is_ascii_whitespace()).unwrap_or(d.len());
    &d[start..]
}

fn starts_with_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack[..needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(super::mime::is_token_byte)
}

fn is_token_byte(b: u8) -> bool {
    matches!(b,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
        | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
        | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
    )
}

fn needs_quoting(s: &str) -> bool {
    !s.bytes().all(is_token_byte)
}

/// Parse `; key=value` parameter list.
fn parse_params(s: &str) -> Result<HashMap<String, String>, MimeError> {
    let mut params = HashMap::new();
    let mut rest = s;
    while !rest.trim().is_empty() {
        rest = rest.trim_start_matches(|c: char| c == ';' || c.is_ascii_whitespace());
        if rest.is_empty() {
            break;
        }
        let eq = rest.find('=').ok_or(MimeError::InvalidParameter)?;
        let key = rest[..eq].trim().to_ascii_lowercase();
        if !is_token(&key) {
            return Err(MimeError::InvalidParameter);
        }
        rest = rest[eq + 1..].trim_start();

        let (value, remaining) = if let Some(s) = rest.strip_prefix('"') {
            consume_quoted_string(s)?
        } else {
            // Token value — read until ';' or end.
            let end = rest.find(';').unwrap_or(rest.len());
            (rest[..end].trim().to_owned(), &rest[end..])
        };

        params.insert(key, value);
        rest = remaining;
    }
    Ok(params)
}

/// Consume a quoted-string body (after the opening `"`), returning
/// (unescaped_value, remaining_input).
fn consume_quoted_string(s: &str) -> Result<(String, &str), MimeError> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    loop {
        match chars.next() {
            None           => return Err(MimeError::InvalidParameter),
            Some((_, '"')) => {
                // Closing quote — next chars.next() gives the remaining slice.
                let remaining = match chars.next() {
                    None         => "",
                    Some((i, _)) => &s[i..],
                };
                // Rewind one char — we consumed the char after the closing quote.
                // Instead, split at the closing quote position.
                let close_pos = s[..s.len() - remaining.len() - 1].len();
                return Ok((out, &s[close_pos + 1..]));
            }
            Some((_, '\\')) => {
                match chars.next() {
                    None         => return Err(MimeError::InvalidParameter),
                    Some((_, c)) => out.push(c),
                }
            }
            Some((_, c)) => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let (t, p) = parse_media_type("text/html; charset=utf-8").unwrap();
        assert_eq!(t, "text/html");
        assert_eq!(p["charset"], "utf-8");
    }

    #[test]
    fn parse_no_params() {
        let (t, p) = parse_media_type("application/json").unwrap();
        assert_eq!(t, "application/json");
        assert!(p.is_empty());
    }

    #[test]
    fn parse_quoted_param() {
        let (t, p) = parse_media_type(r#"multipart/form-data; boundary="----abc""#).unwrap();
        assert_eq!(t, "multipart/form-data");
        assert_eq!(p["boundary"], "----abc");
    }

    #[test]
    fn format_round_trip() {
        let mut params = HashMap::new();
        params.insert("charset".into(), "utf-8".into());
        let s = format_media_type("text/html", &params).unwrap();
        assert_eq!(s, "text/html; charset=utf-8");
    }

    #[test]
    fn type_by_ext() {
        assert!(type_by_extension(".html").unwrap().contains("text/html"));
        assert!(type_by_extension(".png").unwrap().contains("image/png"));
        assert!(type_by_extension(".unknown").is_none());
    }

    #[test]
    fn detect_png() {
        let png = b"\x89PNG\r\n\x1a\nsome data";
        assert_eq!(detect_content_type(png), "image/png");
    }

    #[test]
    fn detect_html() {
        let html = b"<!DOCTYPE html><html><body></body></html>";
        assert_eq!(detect_content_type(html), "text/html; charset=utf-8");
    }

    #[test]
    fn detect_text() {
        let text = b"Hello, plain text here.";
        assert_eq!(detect_content_type(text), "text/plain; charset=utf-8");
    }

    #[test]
    fn detect_binary() {
        let bin: &[u8] = &[0x00, 0x01, 0x02, 0x03];
        assert_eq!(detect_content_type(bin), "application/octet-stream");
    }
}

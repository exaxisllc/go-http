// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::io::{self, Write};

/// HTTP headers — a multi-valued map with case-insensitive canonical keys.
///
/// Keys are stored in canonical form ("Content-Type", not "content-type").
/// Mirrors Go's `http.Header`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Header(HashMap<String, Vec<String>>);

impl Header {
    pub fn new() -> Self {
        Self::default()
    }

    /// Canonical form of a header key: "content-type" → "Content-Type".
    pub fn canonical_key(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut upper = true;
        for c in s.chars() {
            if c == '-' {
                out.push('-');
                upper = true;
            } else if upper {
                out.extend(c.to_uppercase());
                upper = false;
            } else {
                out.extend(c.to_lowercase());
            }
        }
        out
    }

    /// Return the first value for the given key, if any.
    pub fn get(&self, key: &str) -> Option<&str> {
        let k = Self::canonical_key(key);
        self.0.get(&k).and_then(|v| v.first()).map(String::as_str)
    }

    /// Set the key to a single value, replacing any existing values.
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let k = Self::canonical_key(key);
        self.0.insert(k, vec![value.into()]);
    }

    /// Add a value to the key without replacing existing values.
    pub fn add(&mut self, key: &str, value: impl Into<String>) {
        let k = Self::canonical_key(key);
        self.0.entry(k).or_default().push(value.into());
    }

    /// Remove all values for the given key.
    pub fn del(&mut self, key: &str) {
        let k = Self::canonical_key(key);
        self.0.remove(&k);
    }

    /// All values for the given key.
    pub fn values(&self, key: &str) -> &[String] {
        let k = Self::canonical_key(key);
        self.0.get(&k).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Iterate over all (key, values) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// `true` if the header map contains no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Serialize the headers to wire format (each "Key: value\r\n" line).
    /// Does not write the terminal blank line.
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let mut keys: Vec<&String> = self.0.keys().collect();
        keys.sort();
        for key in keys {
            for val in &self.0[key] {
                write!(w, "{key}: {val}\r\n")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_key() {
        assert_eq!(Header::canonical_key("content-type"), "Content-Type");
        assert_eq!(Header::canonical_key("x-request-id"), "X-Request-Id");
        assert_eq!(Header::canonical_key("ACCEPT"), "Accept");
    }

    #[test]
    fn set_get_del() {
        let mut h = Header::new();
        h.set("content-type", "text/plain");
        assert_eq!(h.get("Content-Type"), Some("text/plain"));
        h.del("content-type");
        assert_eq!(h.get("content-type"), None);
    }

    #[test]
    fn add_multi_value() {
        let mut h = Header::new();
        h.add("Accept", "text/html");
        h.add("accept", "application/json");
        assert_eq!(h.values("Accept"), &["text/html", "application/json"]);
    }

    #[test]
    fn write_wire_format() {
        let mut h = Header::new();
        h.set("Content-Type", "text/plain");
        h.set("Content-Length", "5");
        let mut buf = Vec::new();
        h.write_to(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Content-Type: text/plain\r\n"));
        assert!(s.contains("Content-Length: 5\r\n"));
    }
}

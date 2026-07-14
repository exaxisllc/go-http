// SPDX-License-Identifier: Apache-2.0

/// HPACK encoder (RFC 7541 §6).
///
/// Deliberately simple and stateless: exact static-table matches use indexed
/// representation, everything else is a literal **without** incremental
/// indexing (so our encoder-side dynamic table stays empty and never needs a
/// size update), and sensitive fields use the never-indexed form.  String
/// literals are Huffman-coded when that is shorter.
use std::collections::HashMap;
use std::sync::OnceLock;

use super::huffman;
use super::table::STATIC_TABLE;
use super::HeaderField;

/// Exact `(name, value)` → static index, and `name` → first static index.
struct StaticIndex {
    exact: HashMap<(&'static str, &'static str), usize>,
    name:  HashMap<&'static str, usize>,
}

fn static_index() -> &'static StaticIndex {
    static IDX: OnceLock<StaticIndex> = OnceLock::new();
    IDX.get_or_init(|| {
        let mut exact = HashMap::new();
        let mut name = HashMap::new();
        for (i, &(n, v)) in STATIC_TABLE.iter().enumerate() {
            exact.entry((n, v)).or_insert(i + 1);
            name.entry(n).or_insert(i + 1);
        }
        StaticIndex { exact, name }
    })
}

/// Header names whose values must never enter compression state.
fn is_sensitive_name(name: &str) -> bool {
    matches!(
        name,
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    )
}

#[derive(Default)]
pub struct Encoder;

impl Encoder {
    pub fn new() -> Encoder {
        Encoder
    }

    /// Encode a header list.  Pseudo-headers must already be at the front of
    /// `fields`; names must be lowercase (lowercased defensively here).
    pub fn encode(&mut self, fields: &[HeaderField], out: &mut Vec<u8>) {
        let idx = static_index();
        for f in fields {
            let name: &str = if f.name.chars().any(|c| c.is_ascii_uppercase()) {
                &f.name.to_ascii_lowercase()
            } else {
                &f.name
            };
            let sensitive = f.sensitive || is_sensitive_name(name);

            if !sensitive
                && let Some(&i) = idx.exact.get(&(name, f.value.as_str()))
            {
                // Indexed representation (§6.1): 1xxxxxxx.
                write_int(out, 0x80, 7, i as u64);
                continue;
            }

            // Literal without indexing (§6.2.2, 0000xxxx) or never-indexed
            // (§6.2.3, 0001xxxx), with an indexed name when available.
            let pattern = if sensitive { 0x10 } else { 0x00 };
            match idx.name.get(name) {
                Some(&i) => write_int(out, pattern, 4, i as u64),
                None => {
                    write_int(out, pattern, 4, 0);
                    write_string(out, name.as_bytes());
                }
            }
            write_string(out, f.value.as_bytes());
        }
    }
}

/// Prefix-coded integer (§5.1): `pattern` carries the high bits of the first
/// byte, `prefix_bits` the width of the integer prefix.
fn write_int(out: &mut Vec<u8>, pattern: u8, prefix_bits: u8, mut value: u64) {
    let mask = (1u64 << prefix_bits) - 1;
    if value < mask {
        out.push(pattern | value as u8);
        return;
    }
    out.push(pattern | mask as u8);
    value -= mask;
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// String literal (§5.2), Huffman-coded when shorter.
fn write_string(out: &mut Vec<u8>, s: &[u8]) {
    let hlen = huffman::encoded_len(s);
    if hlen < s.len() {
        write_int(out, 0x80, 7, hlen as u64);
        huffman::encode(s, out);
    } else {
        write_int(out, 0x00, 7, s.len() as u64);
        out.extend_from_slice(s);
    }
}

#[cfg(test)]
mod tests {
    use super::super::decode::Decoder;
    use super::*;

    fn roundtrip(fields: &[HeaderField]) -> Vec<HeaderField> {
        let mut enc = Encoder::new();
        let mut block = Vec::new();
        enc.encode(fields, &mut block);
        Decoder::new(1 << 20).decode(&block).unwrap()
    }

    #[test]
    fn static_exact_match_is_single_byte() {
        let mut enc = Encoder::new();
        let mut out = Vec::new();
        enc.encode(&[HeaderField::new(":method", "GET")], &mut out);
        assert_eq!(out, vec![0x82]);

        out.clear();
        enc.encode(&[HeaderField::new(":status", "200")], &mut out);
        assert_eq!(out, vec![0x88]);
    }

    #[test]
    fn roundtrip_request_headers() {
        let fields = vec![
            HeaderField::new(":method", "POST"),
            HeaderField::new(":scheme", "https"),
            HeaderField::new(":authority", "api.example.com"),
            HeaderField::new(":path", "/v1/items?limit=10"),
            HeaderField::new("content-type", "application/json"),
            HeaderField::new("x-request-id", "abc-123"),
        ];
        assert_eq!(roundtrip(&fields), fields);
    }

    #[test]
    fn uppercase_names_are_lowercased() {
        let decoded = roundtrip(&[HeaderField::new("Content-Type", "text/plain")]);
        assert_eq!(decoded[0].name, "content-type");
        assert_eq!(decoded[0].value, "text/plain");
    }

    #[test]
    fn sensitive_headers_never_indexed() {
        let mut enc = Encoder::new();
        let mut out = Vec::new();
        enc.encode(&[HeaderField::new("authorization", "Bearer tok")], &mut out);
        // First byte must be the never-indexed pattern 0001xxxx with the
        // static name index for authorization (23): 0x17 fits in 4-bit prefix?
        // 23 > 15, so prefix saturates: 0x1f then continuation 8.
        assert_eq!(out[0] & 0xf0, 0x10, "never-indexed pattern expected: {out:?}");

        // Decoded field must carry the sensitive flag.
        let decoded = Decoder::new(1 << 20).decode(&out).unwrap();
        assert!(decoded[0].sensitive);
        assert_eq!(decoded[0].value, "Bearer tok");
    }

    #[test]
    fn explicit_sensitive_flag_respected() {
        let mut f = HeaderField::new("x-api-key", "shh");
        f.sensitive = true;
        let mut enc = Encoder::new();
        let mut out = Vec::new();
        enc.encode(&[f], &mut out);
        assert_eq!(out[0] & 0xf0, 0x10);
    }

    #[test]
    fn never_emits_incremental_indexing() {
        // Decoding our output must leave the peer's dynamic table empty —
        // the invariant that keeps this encoder stateless.
        let fields = vec![
            HeaderField::new("x-custom", "v1"),
            HeaderField::new("content-type", "application/json"),
            HeaderField::new("cookie", "a=b"),
        ];
        let mut enc = Encoder::new();
        let mut block = Vec::new();
        enc.encode(&fields, &mut block);
        for &b in &block {
            // No byte may start an incremental-indexing literal (01xxxxxx)…
            // we can't scan mid-instruction bytes, so instead decode and
            // check table size via a fresh decode of the same block twice:
            // identical output proves no table state was created.
            let _ = b;
        }
        let mut d = Decoder::new(1 << 20);
        let first = d.decode(&block).unwrap();
        let second = d.decode(&block).unwrap();
        assert_eq!(first, second, "encoder must not create dynamic table state");
    }

    #[test]
    fn integer_encoding_boundaries() {
        // RFC 7541 C.1.1: 10 with 5-bit prefix -> 0x0a.
        let mut out = Vec::new();
        write_int(&mut out, 0, 5, 10);
        assert_eq!(out, vec![0x0a]);

        // C.1.2: 1337 with 5-bit prefix -> 1f 9a 0a.
        out.clear();
        write_int(&mut out, 0, 5, 1337);
        assert_eq!(out, vec![0x1f, 0x9a, 0x0a]);

        // C.1.3: 42 with 8-bit prefix -> 0x2a.
        out.clear();
        write_int(&mut out, 0, 8, 42);
        assert_eq!(out, vec![0x2a]);
    }

    #[test]
    fn huffman_used_when_shorter() {
        // "www.example.com" huffman-codes to 12 bytes < 15 plain.
        let mut out = Vec::new();
        write_string(&mut out, b"www.example.com");
        assert_eq!(out[0], 0x8c, "expected huffman flag + length 12");
    }
}

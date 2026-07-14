// SPDX-License-Identifier: Apache-2.0

/// HPACK decoder (RFC 7541 §3, §6).
use crate::h2::error::{ErrCode, H2Error};

use super::huffman;
use super::table::DynamicTable;
use super::HeaderField;

/// Decoder failure modes.
#[derive(Debug)]
pub enum DecodeErr {
    /// A malformed header block — always a connection error of type
    /// COMPRESSION_ERROR (RFC 7541 §5).
    Compression(String),
    /// The decoded header list exceeds SETTINGS_MAX_HEADER_LIST_SIZE.  The
    /// dynamic table state is still consistent (the whole block was
    /// processed), so the connection survives; the caller rejects the
    /// stream/request.
    ListTooLarge,
}

impl From<DecodeErr> for H2Error {
    fn from(e: DecodeErr) -> Self {
        match e {
            DecodeErr::Compression(msg) => H2Error::Connection(ErrCode::Compression, msg),
            DecodeErr::ListTooLarge => H2Error::Connection(
                ErrCode::EnhanceYourCalm,
                "header list too large".into(),
            ),
        }
    }
}

pub struct Decoder {
    table: DynamicTable,
    /// SETTINGS_MAX_HEADER_LIST_SIZE we advertised; enforced per block.
    max_header_list: u64,
}

impl Decoder {
    pub fn new(max_header_list: u64) -> Decoder {
        Decoder {
            table: DynamicTable::new(4096),
            max_header_list,
        }
    }

    /// Adjust the dynamic table capacity when our SETTINGS_HEADER_TABLE_SIZE
    /// changes (unused in v1 — we keep the 4096 default).
    pub fn set_max_table_capacity(&mut self, cap: u64) {
        self.table.set_capacity_from_settings(cap);
    }

    /// Decode one complete header block (after CONTINUATION reassembly).
    ///
    /// On `ListTooLarge` the entire block has still been processed so the
    /// dynamic table stays synchronized with the peer's encoder — only the
    /// decoded fields are discarded.
    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<HeaderField>, DecodeErr> {
        let mut fields = Vec::new();
        let mut pos = 0usize;
        let mut list_size = 0u64;
        let mut too_large = false;
        let mut seen_field = false;

        while pos < block.len() {
            let b = block[pos];
            if b & 0x80 != 0 {
                // Indexed header field (§6.1).
                let index = self.read_int(block, &mut pos, 7)? as usize;
                if index == 0 {
                    return Err(DecodeErr::Compression("indexed field with index 0".into()));
                }
                let (name, value) = self
                    .table
                    .get(index)
                    .map(|(n, v)| (n.to_owned(), v.to_owned()))
                    .ok_or_else(|| DecodeErr::Compression(format!("index {index} out of range")))?;
                seen_field = true;
                push_field(&mut fields, name, value, false, &mut list_size,
                           self.max_header_list, &mut too_large);
            } else if b & 0x40 != 0 {
                // Literal with incremental indexing (§6.2.1).
                let (name, value) = self.read_literal(block, &mut pos, 6)?;
                self.table.insert(name.clone(), value.clone());
                seen_field = true;
                push_field(&mut fields, name, value, false, &mut list_size,
                           self.max_header_list, &mut too_large);
            } else if b & 0x20 != 0 {
                // Dynamic table size update (§6.3) — only legal before the
                // first header field of the block.
                if seen_field {
                    return Err(DecodeErr::Compression(
                        "table size update after header field".into(),
                    ));
                }
                let max = self.read_int(block, &mut pos, 5)?;
                self.table
                    .resize(max)
                    .map_err(|e| DecodeErr::Compression(e.to_string()))?;
            } else {
                // Literal without indexing (§6.2.2, prefix 0000) or
                // never-indexed (§6.2.3, prefix 0001).  Identical wire shape
                // apart from the sensitive bit.
                let sensitive = b & 0x10 != 0;
                let (name, value) = self.read_literal(block, &mut pos, 4)?;
                seen_field = true;
                push_field(&mut fields, name, value, sensitive, &mut list_size,
                           self.max_header_list, &mut too_large);
            }
        }

        if too_large {
            return Err(DecodeErr::ListTooLarge);
        }
        Ok(fields)
    }

    /// Read a name (indexed or literal) + literal value pair.
    fn read_literal(
        &mut self,
        block: &[u8],
        pos: &mut usize,
        name_prefix: u8,
    ) -> Result<(String, String), DecodeErr> {
        let name_index = self.read_int(block, pos, name_prefix)? as usize;
        let name = if name_index == 0 {
            self.read_string(block, pos)?
        } else {
            self.table
                .get(name_index)
                .map(|(n, _)| n.to_owned())
                .ok_or_else(|| {
                    DecodeErr::Compression(format!("name index {name_index} out of range"))
                })?
        };
        let value = self.read_string(block, pos)?;
        Ok((name, value))
    }

    /// Prefix-coded integer (§5.1).
    fn read_int(&self, block: &[u8], pos: &mut usize, prefix_bits: u8) -> Result<u64, DecodeErr> {
        if *pos >= block.len() {
            return Err(DecodeErr::Compression("truncated integer".into()));
        }
        let mask = (1u64 << prefix_bits) - 1;
        let mut value = (block[*pos] as u64) & mask;
        *pos += 1;
        if value < mask {
            return Ok(value);
        }
        let mut shift = 0u32;
        loop {
            if *pos >= block.len() {
                return Err(DecodeErr::Compression("truncated integer continuation".into()));
            }
            let b = block[*pos];
            *pos += 1;
            value = value
                .checked_add(((b & 0x7f) as u64) << shift)
                .ok_or_else(|| DecodeErr::Compression("integer overflow".into()))?;
            if b & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 56 {
                return Err(DecodeErr::Compression("integer too long".into()));
            }
        }
    }

    /// String literal (§5.2): H bit + length + data, Huffman-decoded if H.
    fn read_string(&self, block: &[u8], pos: &mut usize) -> Result<String, DecodeErr> {
        if *pos >= block.len() {
            return Err(DecodeErr::Compression("truncated string".into()));
        }
        let huffman_coded = block[*pos] & 0x80 != 0;
        let len = self.read_int(block, pos, 7)? as usize;
        if *pos + len > block.len() {
            return Err(DecodeErr::Compression("string length exceeds block".into()));
        }
        let raw = &block[*pos..*pos + len];
        *pos += len;

        let bytes = if huffman_coded {
            let mut out = Vec::with_capacity(len * 2);
            huffman::decode(raw, &mut out).map_err(|e| DecodeErr::Compression(e.to_string()))?;
            out
        } else {
            raw.to_vec()
        };
        // Header names/values are field-value octets; treat as opaque bytes
        // carried in a String.  Reject non-UTF-8 (header values are ASCII in
        // practice; the rest of go-http stores headers as String).
        String::from_utf8(bytes)
            .map_err(|_| DecodeErr::Compression("non-UTF-8 header string".into()))
    }
}

/// Append a decoded field, tracking the RFC 7541 §4.1 list size cap.
fn push_field(
    fields: &mut Vec<HeaderField>,
    name: String,
    value: String,
    sensitive: bool,
    list_size: &mut u64,
    max: u64,
    too_large: &mut bool,
) {
    *list_size += name.len() as u64 + value.len() as u64 + 32;
    if *list_size > max {
        *too_large = true;
        return; // discard, but keep decoding for table consistency
    }
    fields.push(HeaderField { name, value, sensitive });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(n: &str, v: &str) -> HeaderField {
        HeaderField::new(n, v)
    }

    // ── RFC 7541 Appendix C vectors ──────────────────────────────────────────

    /// C.2.1: literal with incremental indexing.
    #[test]
    fn c2_1_literal_with_indexing() {
        let block: &[u8] = &[
            0x40, 0x0a, b'c', b'u', b's', b't', b'o', b'm', b'-', b'k', b'e', b'y',
            0x0d, b'c', b'u', b's', b't', b'o', b'm', b'-', b'h', b'e', b'a', b'd', b'e', b'r',
        ];
        let mut d = Decoder::new(1 << 20);
        let fields = d.decode(block).unwrap();
        assert_eq!(fields, vec![field("custom-key", "custom-header")]);
        assert_eq!(d.table.entry_count(), 1);
        assert_eq!(d.table.size(), 55);
    }

    /// C.2.2: literal without indexing, indexed name.
    #[test]
    fn c2_2_literal_without_indexing() {
        let block: &[u8] = &[
            0x04, 0x0c, b'/', b's', b'a', b'm', b'p', b'l', b'e', b'/', b'p', b'a', b't', b'h',
        ];
        let mut d = Decoder::new(1 << 20);
        let fields = d.decode(block).unwrap();
        assert_eq!(fields, vec![field(":path", "/sample/path")]);
        assert_eq!(d.table.entry_count(), 0);
    }

    /// C.2.3: never-indexed literal.
    #[test]
    fn c2_3_never_indexed() {
        let block: &[u8] = &[
            0x10, 0x08, b'p', b'a', b's', b's', b'w', b'o', b'r', b'd',
            0x06, b's', b'e', b'c', b'r', b'e', b't',
        ];
        let mut d = Decoder::new(1 << 20);
        let fields = d.decode(block).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "password");
        assert_eq!(fields[0].value, "secret");
        assert!(fields[0].sensitive);
        assert_eq!(d.table.entry_count(), 0);
    }

    /// C.2.4: indexed header field.
    #[test]
    fn c2_4_indexed() {
        let mut d = Decoder::new(1 << 20);
        let fields = d.decode(&[0x82]).unwrap();
        assert_eq!(fields, vec![field(":method", "GET")]);
    }

    /// C.3: three sequential request header blocks on one connection,
    /// exercising dynamic table accumulation.
    #[test]
    fn c3_requests_without_huffman() {
        let mut d = Decoder::new(1 << 20);

        // C.3.1
        let b1: &[u8] = &[
            0x82, 0x86, 0x84, 0x41, 0x0f, b'w', b'w', b'w', b'.', b'e', b'x', b'a',
            b'm', b'p', b'l', b'e', b'.', b'c', b'o', b'm',
        ];
        let f1 = d.decode(b1).unwrap();
        assert_eq!(
            f1,
            vec![
                field(":method", "GET"),
                field(":scheme", "http"),
                field(":path", "/"),
                field(":authority", "www.example.com"),
            ]
        );
        assert_eq!(d.table.entry_count(), 1);
        assert_eq!(d.table.size(), 57);

        // C.3.2 — :authority now served from the dynamic table (index 62).
        let b2: &[u8] = &[
            0x82, 0x86, 0x84, 0xbe, 0x58, 0x08, b'n', b'o', b'-', b'c', b'a', b'c', b'h', b'e',
        ];
        let f2 = d.decode(b2).unwrap();
        assert_eq!(f2[3], field(":authority", "www.example.com"));
        assert_eq!(f2[4], field("cache-control", "no-cache"));
        assert_eq!(d.table.entry_count(), 2);
        assert_eq!(d.table.size(), 110);

        // C.3.3
        let b3: &[u8] = &[
            0x82, 0x87, 0x85, 0xbf, 0x40, 0x0a, b'c', b'u', b's', b't', b'o', b'm',
            b'-', b'k', b'e', b'y', 0x0c, b'c', b'u', b's', b't', b'o', b'm', b'-',
            b'v', b'a', b'l', b'u', b'e',
        ];
        let f3 = d.decode(b3).unwrap();
        assert_eq!(
            f3,
            vec![
                field(":method", "GET"),
                field(":scheme", "https"),
                field(":path", "/index.html"),
                field(":authority", "www.example.com"),
                field("custom-key", "custom-value"),
            ]
        );
        assert_eq!(d.table.entry_count(), 3);
        assert_eq!(d.table.size(), 164);
    }

    /// C.4: the same three requests with Huffman-coded strings.
    #[test]
    fn c4_requests_with_huffman() {
        let mut d = Decoder::new(1 << 20);

        let b1: &[u8] = &[
            0x82, 0x86, 0x84, 0x41, 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b,
            0xa0, 0xab, 0x90, 0xf4, 0xff,
        ];
        let f1 = d.decode(b1).unwrap();
        assert_eq!(f1[3], field(":authority", "www.example.com"));
        assert_eq!(d.table.size(), 57);

        let b2: &[u8] = &[0x82, 0x86, 0x84, 0xbe, 0x58, 0x86, 0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf];
        let f2 = d.decode(b2).unwrap();
        assert_eq!(f2[4], field("cache-control", "no-cache"));

        let b3: &[u8] = &[
            0x82, 0x87, 0x85, 0xbf, 0x40, 0x88, 0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xa9,
            0x7d, 0x7f, 0x89, 0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xb8, 0xe8, 0xb4, 0xbf,
        ];
        let f3 = d.decode(b3).unwrap();
        assert_eq!(f3[4], field("custom-key", "custom-value"));
        assert_eq!(d.table.entry_count(), 3);
        assert_eq!(d.table.size(), 164);
    }

    /// C.5: response header blocks with a 256-byte table forcing evictions.
    #[test]
    fn c5_responses_with_eviction() {
        let mut d = Decoder::new(1 << 20);
        d.set_max_table_capacity(256);

        // C.5.1
        let b1: &[u8] = &[
            0x48, 0x03, b'3', b'0', b'2', 0x58, 0x07, b'p', b'r', b'i', b'v', b'a',
            b't', b'e', 0x61, 0x1d, b'M', b'o', b'n', b',', b' ', b'2', b'1', b' ',
            b'O', b'c', b't', b' ', b'2', b'0', b'1', b'3', b' ', b'2', b'0', b':',
            b'1', b'3', b':', b'2', b'1', b' ', b'G', b'M', b'T', 0x6e, 0x17, b'h',
            b't', b't', b'p', b's', b':', b'/', b'/', b'w', b'w', b'w', b'.', b'e',
            b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'c', b'o', b'm',
        ];
        let f1 = d.decode(b1).unwrap();
        assert_eq!(
            f1,
            vec![
                field(":status", "302"),
                field("cache-control", "private"),
                field("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
                field("location", "https://www.example.com"),
            ]
        );
        assert_eq!(d.table.entry_count(), 4);
        assert_eq!(d.table.size(), 222);

        // C.5.2 — ":status 307" insertion evicts ":status 302".
        let b2: &[u8] = &[0x48, 0x03, b'3', b'0', b'7', 0xc1, 0xc0, 0xbf];
        let f2 = d.decode(b2).unwrap();
        assert_eq!(f2[0], field(":status", "307"));
        assert_eq!(f2[1], field("cache-control", "private"));
        assert_eq!(d.table.entry_count(), 4);
        assert_eq!(d.table.size(), 222);

        // C.5.3 — two more evictions.
        let b3: &[u8] = &[
            0x88, 0xc1, 0x61, 0x1d, b'M', b'o', b'n', b',', b' ', b'2', b'1', b' ',
            b'O', b'c', b't', b' ', b'2', b'0', b'1', b'3', b' ', b'2', b'0', b':',
            b'1', b'3', b':', b'2', b'2', b' ', b'G', b'M', b'T', 0xc0, 0x5a, 0x04,
            b'g', b'z', b'i', b'p', 0x77, 0x38, b'f', b'o', b'o', b'=', b'A', b'S',
            b'D', b'J', b'K', b'H', b'Q', b'K', b'B', b'Z', b'X', b'O', b'Q', b'W',
            b'E', b'O', b'P', b'I', b'U', b'A', b'X', b'Q', b'W', b'E', b'O', b'I',
            b'U', b';', b' ', b'm', b'a', b'x', b'-', b'a', b'g', b'e', b'=', b'3',
            b'6', b'0', b'0', b';', b' ', b'v', b'e', b'r', b's', b'i', b'o', b'n',
            b'=', b'1',
        ];
        let f3 = d.decode(b3).unwrap();
        assert_eq!(f3[0], field(":status", "200"));
        assert_eq!(f3[2], field("date", "Mon, 21 Oct 2013 20:13:22 GMT"));
        assert_eq!(f3[5], field("set-cookie",
            "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1"));
        assert_eq!(d.table.entry_count(), 3);
        assert_eq!(d.table.size(), 215);
    }

    /// C.6: the same responses Huffman-coded.
    #[test]
    fn c6_responses_with_huffman() {
        let mut d = Decoder::new(1 << 20);
        d.set_max_table_capacity(256);

        let b1: &[u8] = &[
            0x48, 0x82, 0x64, 0x02, 0x58, 0x85, 0xae, 0xc3, 0x77, 0x1a, 0x4b, 0x61,
            0x96, 0xd0, 0x7a, 0xbe, 0x94, 0x10, 0x54, 0xd4, 0x44, 0xa8, 0x20, 0x05,
            0x95, 0x04, 0x0b, 0x81, 0x66, 0xe0, 0x82, 0xa6, 0x2d, 0x1b, 0xff, 0x6e,
            0x91, 0x9d, 0x29, 0xad, 0x17, 0x18, 0x63, 0xc7, 0x8f, 0x0b, 0x97, 0xc8,
            0xe9, 0xae, 0x82, 0xae, 0x43, 0xd3,
        ];
        let f1 = d.decode(b1).unwrap();
        assert_eq!(f1[0], field(":status", "302"));
        assert_eq!(f1[3], field("location", "https://www.example.com"));
        assert_eq!(d.table.size(), 222);

        let b2: &[u8] = &[0x48, 0x83, 0x64, 0x0e, 0xff, 0xc1, 0xc0, 0xbf];
        let f2 = d.decode(b2).unwrap();
        assert_eq!(f2[0], field(":status", "307"));

        let b3: &[u8] = &[
            0x88, 0xc1, 0x61, 0x96, 0xd0, 0x7a, 0xbe, 0x94, 0x10, 0x54, 0xd4, 0x44,
            0xa8, 0x20, 0x05, 0x95, 0x04, 0x0b, 0x81, 0x66, 0xe0, 0x84, 0xa6, 0x2d,
            0x1b, 0xff, 0xc0, 0x5a, 0x83, 0x9b, 0xd9, 0xab, 0x77, 0xad, 0x94, 0xe7,
            0x82, 0x1d, 0xd7, 0xf2, 0xe6, 0xc7, 0xb3, 0x35, 0xdf, 0xdf, 0xcd, 0x5b,
            0x39, 0x60, 0xd5, 0xaf, 0x27, 0x08, 0x7f, 0x36, 0x72, 0xc1, 0xab, 0x27,
            0x0f, 0xb5, 0x29, 0x1f, 0x95, 0x87, 0x31, 0x60, 0x65, 0xc0, 0x03, 0xed,
            0x4e, 0xe5, 0xb1, 0x06, 0x3d, 0x50, 0x07,
        ];
        let f3 = d.decode(b3).unwrap();
        assert_eq!(f3[4], field("content-encoding", "gzip"));
        assert_eq!(f3[5], field("set-cookie",
            "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1"));
        assert_eq!(d.table.entry_count(), 3);
        assert_eq!(d.table.size(), 215);
    }

    // ── Error handling ───────────────────────────────────────────────────────

    #[test]
    fn index_zero_rejected() {
        let mut d = Decoder::new(1 << 20);
        assert!(matches!(d.decode(&[0x80]), Err(DecodeErr::Compression(_))));
    }

    #[test]
    fn out_of_range_index_rejected() {
        let mut d = Decoder::new(1 << 20);
        // Index 100 with empty dynamic table.
        assert!(matches!(d.decode(&[0xff, 0x25]), Err(DecodeErr::Compression(_))));
    }

    #[test]
    fn truncated_string_rejected() {
        let mut d = Decoder::new(1 << 20);
        // Literal, new name, claims 10 bytes but block ends.
        assert!(matches!(
            d.decode(&[0x40, 0x0a, b'a', b'b']),
            Err(DecodeErr::Compression(_))
        ));
    }

    #[test]
    fn integer_overflow_rejected() {
        let mut d = Decoder::new(1 << 20);
        // 0x7f prefix + endless continuation bytes.
        let mut block = vec![0xffu8];
        block.extend(std::iter::repeat_n(0xff, 12));
        assert!(matches!(d.decode(&block), Err(DecodeErr::Compression(_))));
    }

    #[test]
    fn size_update_after_field_rejected() {
        let mut d = Decoder::new(1 << 20);
        // Indexed field then a size update.
        assert!(matches!(d.decode(&[0x82, 0x20]), Err(DecodeErr::Compression(_))));
    }

    #[test]
    fn size_update_above_capacity_rejected() {
        let mut d = Decoder::new(1 << 20);
        // Update to 8192 when SETTINGS capacity is 4096.
        // 0x3f then varint remainder: 8192 - 31 = 8161 = 0xE1 0x3F.
        assert!(matches!(
            d.decode(&[0x3f, 0xe1, 0x3f]),
            Err(DecodeErr::Compression(_))
        ));
    }

    #[test]
    fn list_too_large_keeps_table_state() {
        // Cap small enough that the second field overflows.
        let mut d = Decoder::new(50);
        let block: &[u8] = &[
            // literal w/ indexing: "a: b" (size 34)
            0x40, 0x01, b'a', 0x01, b'b',
            // literal w/ indexing: "cc: dd" (size 36) — overflows the 50 cap
            0x40, 0x02, b'c', b'c', 0x02, b'd', b'd',
        ];
        assert!(matches!(d.decode(block), Err(DecodeErr::ListTooLarge)));
        // Both entries must still have entered the dynamic table.
        assert_eq!(d.table.entry_count(), 2);
    }
}

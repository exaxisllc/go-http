// SPDX-License-Identifier: Apache-2.0

/// HPACK Huffman coding (RFC 7541 §5.2, Appendix B).
use std::sync::OnceLock;

use crate::h2::error::{ErrCode, H2Error};

/// `(code, bits)` for each symbol 0..=255, plus EOS at index 256
/// (RFC 7541 Appendix B).  Codes are right-aligned in the `u32`.
pub static HUFFMAN_CODES: [(u32, u8); 257] = [
    (0x1ff8, 13), (0x7fffd8, 23), (0xfffffe2, 28), (0xfffffe3, 28),
    (0xfffffe4, 28), (0xfffffe5, 28), (0xfffffe6, 28), (0xfffffe7, 28),
    (0xfffffe8, 28), (0xffffea, 24), (0x3ffffffc, 30), (0xfffffe9, 28),
    (0xfffffea, 28), (0x3ffffffd, 30), (0xfffffeb, 28), (0xfffffec, 28),
    (0xfffffed, 28), (0xfffffee, 28), (0xfffffef, 28), (0xffffff0, 28),
    (0xffffff1, 28), (0xffffff2, 28), (0x3ffffffe, 30), (0xffffff3, 28),
    (0xffffff4, 28), (0xffffff5, 28), (0xffffff6, 28), (0xffffff7, 28),
    (0xffffff8, 28), (0xffffff9, 28), (0xffffffa, 28), (0xffffffb, 28),
    (0x14, 6), (0x3f8, 10), (0x3f9, 10), (0xffa, 12),
    (0x1ff9, 13), (0x15, 6), (0xf8, 8), (0x7fa, 11),
    (0x3fa, 10), (0x3fb, 10), (0xf9, 8), (0x7fb, 11),
    (0xfa, 8), (0x16, 6), (0x17, 6), (0x18, 6),
    (0x0, 5), (0x1, 5), (0x2, 5), (0x19, 6),
    (0x1a, 6), (0x1b, 6), (0x1c, 6), (0x1d, 6),
    (0x1e, 6), (0x1f, 6), (0x5c, 7), (0xfb, 8),
    (0x7ffc, 15), (0x20, 6), (0xffb, 12), (0x3fc, 10),
    (0x1ffa, 13), (0x21, 6), (0x5d, 7), (0x5e, 7),
    (0x5f, 7), (0x60, 7), (0x61, 7), (0x62, 7),
    (0x63, 7), (0x64, 7), (0x65, 7), (0x66, 7),
    (0x67, 7), (0x68, 7), (0x69, 7), (0x6a, 7),
    (0x6b, 7), (0x6c, 7), (0x6d, 7), (0x6e, 7),
    (0x6f, 7), (0x70, 7), (0x71, 7), (0x72, 7),
    (0xfc, 8), (0x73, 7), (0xfd, 8), (0x1ffb, 13),
    (0x7fff0, 19), (0x1ffc, 13), (0x3ffc, 14), (0x22, 6),
    (0x7ffd, 15), (0x3, 5), (0x23, 6), (0x4, 5),
    (0x24, 6), (0x5, 5), (0x25, 6), (0x26, 6),
    (0x27, 6), (0x6, 5), (0x74, 7), (0x75, 7),
    (0x28, 6), (0x29, 6), (0x2a, 6), (0x7, 5),
    (0x2b, 6), (0x76, 7), (0x2c, 6), (0x8, 5),
    (0x9, 5), (0x2d, 6), (0x77, 7), (0x78, 7),
    (0x79, 7), (0x7a, 7), (0x7b, 7), (0x7ffe, 15),
    (0x7fc, 11), (0x3ffd, 14), (0x1ffd, 13), (0xffffffc, 28),
    (0xfffe6, 20), (0x3fffd2, 22), (0xfffe7, 20), (0xfffe8, 20),
    (0x3fffd3, 22), (0x3fffd4, 22), (0x3fffd5, 22), (0x7fffd9, 23),
    (0x3fffd6, 22), (0x7fffda, 23), (0x7fffdb, 23), (0x7fffdc, 23),
    (0x7fffdd, 23), (0x7fffde, 23), (0xffffeb, 24), (0x7fffdf, 23),
    (0xffffec, 24), (0xffffed, 24), (0x3fffd7, 22), (0x7fffe0, 23),
    (0xffffee, 24), (0x7fffe1, 23), (0x7fffe2, 23), (0x7fffe3, 23),
    (0x7fffe4, 23), (0x1fffdc, 21), (0x3fffd8, 22), (0x7fffe5, 23),
    (0x3fffd9, 22), (0x7fffe6, 23), (0x7fffe7, 23), (0xffffef, 24),
    (0x3fffda, 22), (0x1fffdd, 21), (0xfffe9, 20), (0x3fffdb, 22),
    (0x3fffdc, 22), (0x7fffe8, 23), (0x7fffe9, 23), (0x1fffde, 21),
    (0x7fffea, 23), (0x3fffdd, 22), (0x3fffde, 22), (0xfffff0, 24),
    (0x1fffdf, 21), (0x3fffdf, 22), (0x7fffeb, 23), (0x7fffec, 23),
    (0x1fffe0, 21), (0x1fffe1, 21), (0x3fffe0, 22), (0x1fffe2, 21),
    (0x7fffed, 23), (0x3fffe1, 22), (0x7fffee, 23), (0x7fffef, 23),
    (0xfffea, 20), (0x3fffe2, 22), (0x3fffe3, 22), (0x3fffe4, 22),
    (0x7ffff0, 23), (0x3fffe5, 22), (0x3fffe6, 22), (0x7ffff1, 23),
    (0x3ffffe0, 26), (0x3ffffe1, 26), (0xfffeb, 20), (0x7fff1, 19),
    (0x3fffe7, 22), (0x7ffff2, 23), (0x3fffe8, 22), (0x1ffffec, 25),
    (0x3ffffe2, 26), (0x3ffffe3, 26), (0x3ffffe4, 26), (0x7ffffde, 27),
    (0x7ffffdf, 27), (0x3ffffe5, 26), (0xfffff1, 24), (0x1ffffed, 25),
    (0x7fff2, 19), (0x1fffe3, 21), (0x3ffffe6, 26), (0x7ffffe0, 27),
    (0x7ffffe1, 27), (0x3ffffe7, 26), (0x7ffffe2, 27), (0xfffff2, 24),
    (0x1fffe4, 21), (0x1fffe5, 21), (0x3ffffe8, 26), (0x3ffffe9, 26),
    (0xffffffd, 28), (0x7ffffe3, 27), (0x7ffffe4, 27), (0x7ffffe5, 27),
    (0xfffec, 20), (0xfffff3, 24), (0xfffed, 20), (0x1fffe6, 21),
    (0x3fffe9, 22), (0x1fffe7, 21), (0x1fffe8, 21), (0x7ffff3, 23),
    (0x3fffea, 22), (0x3fffeb, 22), (0x1ffffee, 25), (0x1ffffef, 25),
    (0xfffff4, 24), (0xfffff5, 24), (0x3ffffea, 26), (0x7ffff4, 23),
    (0x3ffffeb, 26), (0x7ffffe6, 27), (0x3ffffec, 26), (0x3ffffed, 26),
    (0x7ffffe7, 27), (0x7ffffe8, 27), (0x7ffffe9, 27), (0x7ffffea, 27),
    (0x7ffffeb, 27), (0xffffffe, 28), (0x7ffffec, 27), (0x7ffffed, 27),
    (0x7ffffee, 27), (0x7ffffef, 27), (0x7fffff0, 27), (0x3ffffee, 26),
    (0x3fffffff, 30),
];

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

/// A binary-tree node: branch children are node indices, leaves hold a symbol.
#[derive(Clone, Copy)]
enum Node {
    Branch { zero: u16, one: u16 },
    Leaf(u16), // symbol 0..=256
    Empty,
}

fn decode_tree() -> &'static Vec<Node> {
    static TREE: OnceLock<Vec<Node>> = OnceLock::new();
    TREE.get_or_init(|| {
        let mut nodes = vec![Node::Empty];
        for (sym, &(code, bits)) in HUFFMAN_CODES.iter().enumerate() {
            let mut idx = 0usize;
            for i in (0..bits).rev() {
                let bit = (code >> i) & 1;
                let next = match nodes[idx] {
                    Node::Branch { zero, one } => {
                        let child = if bit == 0 { zero } else { one };
                        if child != 0 {
                            child as usize
                        } else {
                            let new = nodes.len();
                            nodes.push(Node::Empty);
                            nodes[idx] = Node::Branch {
                                zero: if bit == 0 { new as u16 } else { zero },
                                one:  if bit == 1 { new as u16 } else { one },
                            };
                            new
                        }
                    }
                    Node::Empty => {
                        let new = nodes.len();
                        nodes.push(Node::Empty);
                        nodes[idx] = Node::Branch {
                            zero: if bit == 0 { new as u16 } else { 0 },
                            one:  if bit == 1 { new as u16 } else { 0 },
                        };
                        new
                    }
                    Node::Leaf(_) => unreachable!("prefix-free code walked through a leaf"),
                };
                idx = next;
            }
            nodes[idx] = Node::Leaf(sym as u16);
        }
        nodes
    })
}

/// Decode a Huffman-coded string literal into `out`.
///
/// Errors with COMPRESSION_ERROR on: an embedded EOS symbol, padding longer
/// than 7 bits, or padding that is not the most-significant bits of EOS
/// (i.e. not all ones) — RFC 7541 §5.2.
pub fn decode(input: &[u8], out: &mut Vec<u8>) -> Result<(), H2Error> {
    let tree = decode_tree();
    let mut idx = 0usize;         // current tree node
    let mut path_bits = 0u32;     // bits consumed since the last emitted symbol
    let mut path_all_ones = true; // whether those bits are all 1s

    for &byte in input {
        for i in (0..8).rev() {
            let bit = (byte >> i) & 1;
            idx = match tree[idx] {
                Node::Branch { zero, one } => (if bit == 0 { zero } else { one }) as usize,
                _ => 0,
            };
            if idx == 0 {
                return Err(compression_err("invalid Huffman code"));
            }
            path_bits += 1;
            path_all_ones &= bit == 1;
            if let Node::Leaf(sym) = tree[idx] {
                if sym == 256 {
                    return Err(compression_err("EOS symbol in Huffman-coded string"));
                }
                out.push(sym as u8);
                idx = 0;
                path_bits = 0;
                path_all_ones = true;
            }
        }
    }

    // Remaining bits are padding: must be < 8 bits and all ones.
    if path_bits > 7 || !path_all_ones {
        return Err(compression_err("invalid Huffman padding"));
    }
    Ok(())
}

fn compression_err(msg: &str) -> H2Error {
    H2Error::Connection(ErrCode::Compression, msg.to_owned())
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

/// Huffman-encode `input`, appending to `out`.  The final partial byte is
/// padded with the most-significant bits of EOS (all ones).
pub fn encode(input: &[u8], out: &mut Vec<u8>) {
    let mut acc: u64 = 0;
    let mut acc_bits: u32 = 0;
    for &b in input {
        let (code, bits) = HUFFMAN_CODES[b as usize];
        acc = (acc << bits) | code as u64;
        acc_bits += bits as u32;
        while acc_bits >= 8 {
            acc_bits -= 8;
            out.push((acc >> acc_bits) as u8);
        }
    }
    if acc_bits > 0 {
        // Pad with 1s to the byte boundary.
        out.push(((acc << (8 - acc_bits)) as u8) | ((1 << (8 - acc_bits)) - 1));
    }
}

/// The encoded length in bytes of `input` (used to decide whether Huffman
/// coding is shorter than the plain literal).
pub fn encoded_len(input: &[u8]) -> usize {
    let bits: u64 = input.iter().map(|&b| HUFFMAN_CODES[b as usize].1 as u64).sum();
    bits.div_ceil(8) as usize
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The code table must be prefix-free — this catches most transcription
    /// errors (duplicate codes, wrong lengths).
    #[test]
    fn table_is_prefix_free() {
        for (i, &(code_a, bits_a)) in HUFFMAN_CODES.iter().enumerate() {
            assert!((5..=30).contains(&bits_a), "symbol {i}: bad length {bits_a}");
            assert!((code_a as u64) < (1u64 << bits_a), "symbol {i}: code wider than length");
            for (j, &(code_b, bits_b)) in HUFFMAN_CODES.iter().enumerate() {
                if i == j {
                    continue;
                }
                let (short, long, sc, lc) = if bits_a <= bits_b {
                    (bits_a, bits_b, code_a, code_b)
                } else {
                    (bits_b, bits_a, code_b, code_a)
                };
                assert!(
                    lc >> (long - short) != sc,
                    "symbol {i} and {j}: prefix collision"
                );
            }
        }
    }

    /// The table is a canonical Huffman code: sorted by (length, symbol),
    /// each code is the previous one incremented and left-shifted into the
    /// new length.  This verifies every single entry against the canonical
    /// construction — a full transcription check.
    #[test]
    fn table_is_canonical() {
        let mut by_len: Vec<(u8, usize, u32)> = HUFFMAN_CODES
            .iter()
            .enumerate()
            .map(|(sym, &(code, bits))| (bits, sym, code))
            .collect();
        by_len.sort();
        let mut expected: u32 = 0;
        let mut prev_bits: u8 = by_len[0].0;
        for &(bits, sym, code) in &by_len {
            expected <<= bits - prev_bits;
            prev_bits = bits;
            assert_eq!(code, expected, "symbol {sym} (len {bits}): expected {expected:#x}");
            expected += 1;
        }
    }

    fn encode_str(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        encode(s.as_bytes(), &mut out);
        out
    }

    fn decode_bytes(b: &[u8]) -> Result<String, H2Error> {
        let mut out = Vec::new();
        decode(b, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    /// RFC 7541 Appendix C.4 / C.6 Huffman-coded string examples.
    #[test]
    fn rfc7541_examples() {
        let cases: &[(&str, &[u8])] = &[
            ("www.example.com", &[0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff]),
            ("no-cache", &[0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf]),
            ("custom-key", &[0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xa9, 0x7d, 0x7f]),
            ("custom-value", &[0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xb8, 0xe8, 0xb4, 0xbf]),
            ("302", &[0x64, 0x02]),
            ("private", &[0xae, 0xc3, 0x77, 0x1a, 0x4b]),
            (
                "Mon, 21 Oct 2013 20:13:21 GMT",
                &[0xd0, 0x7a, 0xbe, 0x94, 0x10, 0x54, 0xd4, 0x44, 0xa8, 0x20, 0x05, 0x95,
                  0x04, 0x0b, 0x81, 0x66, 0xe0, 0x82, 0xa6, 0x2d, 0x1b, 0xff],
            ),
            (
                "https://www.example.com",
                &[0x9d, 0x29, 0xad, 0x17, 0x18, 0x63, 0xc7, 0x8f, 0x0b, 0x97, 0xc8, 0xe9,
                  0xae, 0x82, 0xae, 0x43, 0xd3],
            ),
            ("307", &[0x64, 0x0e, 0xff]),
            ("gzip", &[0x9b, 0xd9, 0xab]),
            (
                "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
                &[0x94, 0xe7, 0x82, 0x1d, 0xd7, 0xf2, 0xe6, 0xc7, 0xb3, 0x35, 0xdf, 0xdf,
                  0xcd, 0x5b, 0x39, 0x60, 0xd5, 0xaf, 0x27, 0x08, 0x7f, 0x36, 0x72, 0xc1,
                  0xab, 0x27, 0x0f, 0xb5, 0x29, 0x1f, 0x95, 0x87, 0x31, 0x60, 0x65, 0xc0,
                  0x03, 0xed, 0x4e, 0xe5, 0xb1, 0x06, 0x3d, 0x50, 0x07],
            ),
        ];
        for (plain, coded) in cases {
            assert_eq!(&encode_str(plain), coded, "encode {plain:?}");
            assert_eq!(&decode_bytes(coded).unwrap(), plain, "decode {plain:?}");
        }
    }

    #[test]
    fn roundtrip_all_bytes() {
        let all: Vec<u8> = (0u8..=255).collect();
        let mut coded = Vec::new();
        encode(&all, &mut coded);
        let mut back = Vec::new();
        decode(&coded, &mut back).unwrap();
        assert_eq!(back, all);
    }

    #[test]
    fn empty_string() {
        assert!(encode_str("").is_empty());
        assert_eq!(decode_bytes(&[]).unwrap(), "");
    }

    #[test]
    fn bad_padding_rejected() {
        // 'a' = 00011 (5 bits); pad with zeros instead of ones.
        let byte = 0b0001_1000u8;
        let mut out = Vec::new();
        let err = decode(&[byte], &mut out).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::Compression, _)), "{err:?}");
    }

    #[test]
    fn overlong_padding_rejected() {
        // A full byte of ones after a symbol: 8 bits of padding is illegal.
        // 'a' (00011) + 3 one-bits fills byte 1, then a full 0xff padding byte.
        let bytes = [0b0001_1111u8, 0xff];
        let mut out = Vec::new();
        let err = decode(&bytes, &mut out).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::Compression, _)), "{err:?}");
    }

    #[test]
    fn eos_in_stream_rejected() {
        // EOS = 30 one-bits; encode it directly: 3 bytes of 0xff + 6 more one
        // bits padded with 1s = 4 bytes of 0xff.
        let bytes = [0xffu8; 4];
        let mut out = Vec::new();
        let err = decode(&bytes, &mut out).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::Compression, _)), "{err:?}");
    }

    #[test]
    fn encoded_len_matches() {
        for s in ["www.example.com", "no-cache", "gzip", ""] {
            assert_eq!(encoded_len(s.as_bytes()), encode_str(s).len());
        }
    }
}

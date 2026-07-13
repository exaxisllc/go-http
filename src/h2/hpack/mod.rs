// SPDX-License-Identifier: Apache-2.0

/// HPACK header compression (RFC 7541).
///
/// The decoder implements the full specification: indexed and literal
/// representations, the static and dynamic tables, dynamic table size
/// updates, and Huffman-coded string literals.
///
/// The encoder favors simplicity over maximum compression: it uses indexed
/// representation for exact static-table matches, literal-without-indexing
/// otherwise (so the encoder-side dynamic table stays empty), plain
/// (non-Huffman) string literals, and never-indexed literals for sensitive
/// fields.
pub mod decode;
pub mod encode;
pub mod huffman;
pub mod table;

pub use decode::{DecodeErr, Decoder};
pub use encode::Encoder;

/// One decoded (or to-be-encoded) header field.
///
/// Names are kept in wire form: lowercase, with pseudo-headers carrying their
/// leading `:`.  Conversion to/from the Title-Case `crate::header::Header`
/// happens at the h2 server/client boundary, never here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderField {
    pub name:  String,
    pub value: String,
    /// Sensitive fields are encoded as never-indexed literals so
    /// intermediaries do not store them in their compression state.
    pub sensitive: bool,
}

impl HeaderField {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self { name: name.into(), value: value.into(), sensitive: false }
    }

    /// The field's contribution to the header list size (RFC 7541 §4.1):
    /// name length + value length + 32.
    pub fn size(&self) -> u64 {
        self.name.len() as u64 + self.value.len() as u64 + 32
    }
}

/// Total header list size per RFC 7541 §4.1 — compared against
/// SETTINGS_MAX_HEADER_LIST_SIZE.
pub fn list_size(fields: &[HeaderField]) -> u64 {
    fields.iter().map(HeaderField::size).sum()
}

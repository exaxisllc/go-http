// SPDX-License-Identifier: Apache-2.0

/// HPACK static and dynamic tables (RFC 7541 §2.3, Appendix A).
use std::collections::VecDeque;

use crate::h2::error::{ErrCode, H2Error};

/// The static table (RFC 7541 Appendix A).  Index 1..=61 on the wire.
pub static STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

/// The dynamic table: newest entry at the front, evicted from the back.
/// Wire indices 62.. address entries front-to-back.
pub struct DynamicTable {
    entries:  VecDeque<(String, String)>,
    /// Current size per RFC 7541 §4.1 (sum of name + value + 32 per entry).
    size:     u64,
    /// Current maximum size, set by dynamic table size updates.
    max_size: u64,
    /// Upper bound for `max_size`, from SETTINGS_HEADER_TABLE_SIZE.
    capacity: u64,
}

impl DynamicTable {
    pub fn new(capacity: u64) -> Self {
        Self { entries: VecDeque::new(), size: 0, max_size: capacity, capacity }
    }

    /// Called when (our) SETTINGS_HEADER_TABLE_SIZE changes: adjusts the cap
    /// a peer's size update may select.
    pub fn set_capacity_from_settings(&mut self, cap: u64) {
        self.capacity = cap;
        if self.max_size > cap {
            self.max_size = cap;
            self.evict();
        }
    }

    /// Handle a dynamic-table-size-update instruction (RFC 7541 §6.3).
    /// Selecting a size above the SETTINGS-derived capacity is a compression
    /// error.
    pub fn resize(&mut self, max: u64) -> Result<(), H2Error> {
        if max > self.capacity {
            return Err(H2Error::Connection(
                ErrCode::Compression,
                format!("dynamic table size update {max} exceeds capacity {}", self.capacity),
            ));
        }
        self.max_size = max;
        self.evict();
        Ok(())
    }

    /// Insert a new entry at the front, evicting from the back as needed
    /// (RFC 7541 §4.4).  An entry larger than the whole table empties it.
    pub fn insert(&mut self, name: String, value: String) {
        let entry_size = name.len() as u64 + value.len() as u64 + 32;
        self.size += entry_size;
        self.entries.push_front((name, value));
        self.evict();
    }

    fn evict(&mut self) {
        while self.size > self.max_size {
            match self.entries.pop_back() {
                Some((n, v)) => self.size -= n.len() as u64 + v.len() as u64 + 32,
                None => {
                    self.size = 0;
                    break;
                }
            }
        }
    }

    /// Look up a wire index in the combined address space:
    /// 1..=61 static, 62.. dynamic.  Index 0 is invalid.
    pub fn get(&self, index: usize) -> Option<(&str, &str)> {
        if index == 0 {
            return None;
        }
        if index <= STATIC_TABLE.len() {
            let (n, v) = STATIC_TABLE[index - 1];
            return Some((n, v));
        }
        self.entries
            .get(index - STATIC_TABLE.len() - 1)
            .map(|(n, v)| (n.as_str(), v.as_str()))
    }

    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn size(&self) -> u64 {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_table_spot_checks() {
        // RFC 7541 Appendix A.
        assert_eq!(STATIC_TABLE[0], (":authority", ""));
        assert_eq!(STATIC_TABLE[1], (":method", "GET"));
        assert_eq!(STATIC_TABLE[7], (":status", "200"));
        assert_eq!(STATIC_TABLE[37], ("host", ""));
        assert_eq!(STATIC_TABLE[60], ("www-authenticate", ""));
    }

    #[test]
    fn combined_index_space() {
        let mut t = DynamicTable::new(4096);
        assert_eq!(t.get(0), None);
        assert_eq!(t.get(2), Some((":method", "GET")));
        assert_eq!(t.get(61), Some(("www-authenticate", "")));
        assert_eq!(t.get(62), None); // dynamic table empty

        t.insert("x-a".into(), "1".into());
        t.insert("x-b".into(), "2".into());
        // Newest first: index 62 is x-b.
        assert_eq!(t.get(62), Some(("x-b", "2")));
        assert_eq!(t.get(63), Some(("x-a", "1")));
        assert_eq!(t.get(64), None);
    }

    #[test]
    fn eviction_on_overflow() {
        // Each entry "abcd"/"efgh" is 4+4+32 = 40 bytes; cap 100 fits two.
        let mut t = DynamicTable::new(100);
        t.insert("abcd".into(), "efgh".into());
        t.insert("ijkl".into(), "mnop".into());
        assert_eq!(t.entry_count(), 2);
        t.insert("qrst".into(), "uvwx".into());
        assert_eq!(t.entry_count(), 2, "oldest entry must be evicted");
        assert_eq!(t.get(62), Some(("qrst", "uvwx")));
        assert_eq!(t.get(63), Some(("ijkl", "mnop")));
    }

    #[test]
    fn oversized_entry_empties_table() {
        let mut t = DynamicTable::new(50);
        t.insert("a".into(), "b".into()); // 34 bytes
        assert_eq!(t.entry_count(), 1);
        t.insert("x".repeat(100), "y".into()); // way over cap
        assert_eq!(t.entry_count(), 0);
        assert_eq!(t.size(), 0);
    }

    #[test]
    fn resize_evicts_and_validates() {
        let mut t = DynamicTable::new(200);
        t.insert("abcd".into(), "efgh".into()); // 40
        t.insert("ijkl".into(), "mnop".into()); // 40
        t.resize(50).unwrap();
        assert_eq!(t.entry_count(), 1);
        assert_eq!(t.get(62), Some(("ijkl", "mnop")));

        assert!(t.resize(201).is_err(), "resize above capacity must fail");
    }

    #[test]
    fn settings_capacity_shrink() {
        let mut t = DynamicTable::new(200);
        t.insert("abcd".into(), "efgh".into());
        t.insert("ijkl".into(), "mnop".into());
        t.set_capacity_from_settings(40);
        assert_eq!(t.entry_count(), 1);
    }
}

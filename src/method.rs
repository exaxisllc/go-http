// SPDX-License-Identifier: Apache-2.0

pub const GET: &str = "GET";
pub const HEAD: &str = "HEAD";
pub const POST: &str = "POST";
pub const PUT: &str = "PUT";
pub const PATCH: &str = "PATCH";
pub const DELETE: &str = "DELETE";
pub const CONNECT: &str = "CONNECT";
pub const OPTIONS: &str = "OPTIONS";
pub const TRACE: &str = "TRACE";

/// Returns true if `m` is a valid HTTP method token (RFC 7230 §3.2.6).
pub fn is_valid(m: &str) -> bool {
    !m.is_empty()
        && m.bytes().all(|b| {
            matches!(b,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
                | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
                | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_methods() {
        for m in &[GET, HEAD, POST, PUT, PATCH, DELETE, CONNECT, OPTIONS, TRACE] {
            assert!(is_valid(m), "{m} should be valid");
        }
    }

    #[test]
    fn invalid_methods() {
        assert!(!is_valid(""));
        assert!(!is_valid("GET EXTRA"));
        assert!(!is_valid("GÉT"));
    }
}

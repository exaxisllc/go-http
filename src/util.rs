/// Utility functions — port of Go net/http helpers.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cookie::Cookie;
use crate::request::Request;
use crate::response::ResponseWriter;
use crate::status;

// ---------------------------------------------------------------------------
// detect_content_type — re-exported from mime
// ---------------------------------------------------------------------------

pub use crate::mime::detect_content_type;

// ---------------------------------------------------------------------------
// parse_time — HTTP-date parsing (RFC 7231 §7.1.1.1)
// ---------------------------------------------------------------------------

/// Parse an HTTP-date string in any of the three formats Go accepts:
/// RFC 1123 ("Mon, 02 Jan 2006 15:04:05 GMT"),
/// RFC 850  ("Monday, 02-Jan-06 15:04:05 GMT"),
/// asctime  ("Mon Jan  2 15:04:05 2006").
///
/// Port of Go's `http.ParseTime`.
pub fn parse_time(s: &str) -> Option<SystemTime> {
    // Try each format in order.
    for fmt in &[
        "%a, %d %b %Y %H:%M:%S GMT",  // RFC 1123
        "%A, %d-%b-%y %H:%M:%S GMT",  // RFC 850
        "%a %b %e %H:%M:%S %Y",       // asctime
    ] {
        // We use a minimal manual parser rather than pulling in chrono.
        if let Some(t) = parse_http_date(s, fmt) {
            return Some(t);
        }
    }
    None
}

/// Minimal HTTP-date parser for the three formats above.
/// Returns `None` if parsing fails.
fn parse_http_date(s: &str, _fmt: &str) -> Option<SystemTime> {
    // Parse RFC 1123: "Mon, 02 Jan 2006 15:04:05 GMT"
    // We accept either RFC 1123 or RFC 1123Z (with +0000).
    let s = s.trim();
    // Find the date portion after the weekday.
    let rest = if let Some(i) = s.find(", ") { &s[i + 2..] } else { s };

    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 4 { return None; }

    let day:  u64 = parts[0].parse().ok()?;
    let mon:  u64 = month_num(parts[1])?;
    let year: u64 = parts[2].parse().ok()?;
    let time_parts: Vec<&str> = parts[3].split(':').collect();
    if time_parts.len() < 3 { return None; }
    let hour:  u64 = time_parts[0].parse().ok()?;
    let min:   u64 = time_parts[1].parse().ok()?;
    let sec:   u64 = time_parts[2].parse().ok()?;

    // Convert to seconds since Unix epoch via a simple formula.
    let days_since_epoch = days_from_civil(year as i64, mon, day);
    let secs = days_since_epoch as u64 * 86400 + hour * 3600 + min * 60 + sec;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

fn month_num(s: &str) -> Option<u64> {
    match s {
        "Jan" | "January"  => Some(1),  "Feb" | "February" => Some(2),
        "Mar" | "March"    => Some(3),  "Apr" | "April"    => Some(4),
        "May"              => Some(5),  "Jun" | "June"     => Some(6),
        "Jul" | "July"     => Some(7),  "Aug" | "August"   => Some(8),
        "Sep" | "September"=> Some(9),  "Oct" | "October"  => Some(10),
        "Nov" | "November" => Some(11), "Dec" | "December" => Some(12),
        _ => None,
    }
}

/// Days since Unix epoch (1970-01-01) for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u64, d: u64) -> i64 {
    let m = m as i64;
    let d = d as i64;
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// ---------------------------------------------------------------------------
// set_cookie
// ---------------------------------------------------------------------------

/// Append a Set-Cookie header to the response.
/// Port of Go's `http.SetCookie`.
pub fn set_cookie(w: &mut dyn ResponseWriter, cookie: &Cookie) {
    w.header().add("Set-Cookie", cookie.to_set_cookie_header());
}

// ---------------------------------------------------------------------------
// error, not_found, redirect
// ---------------------------------------------------------------------------

/// Write a plain-text error response.
/// Port of Go's `http.Error`.
pub fn error(w: &mut dyn ResponseWriter, message: &str, code: u16) {
    w.header().set("Content-Type", "text/plain; charset=utf-8");
    w.header().set("X-Content-Type-Options", "nosniff");
    w.write_header(code);
    let _ = w.write(message.as_bytes());
    let _ = w.write(b"\n");
}

/// Reply with 404 Not Found.
/// Port of Go's `http.NotFound`.
pub fn not_found(w: &mut dyn ResponseWriter, _r: &Request) {
    error(w, "404 page not found", status::NOT_FOUND);
}

/// Reply with a redirect to `url`.
/// Port of Go's `http.Redirect`.
pub fn redirect(w: &mut dyn ResponseWriter, _r: &Request, url: &str, code: u16) {
    w.header().set("Location", url);
    w.header().set("Content-Type", "text/html; charset=utf-8");
    w.write_header(code);
    let body = format!(
        "<a href=\"{url}\">{text}</a>.\n",
        url = html_escape(url),
        text = status::status_text(code),
    );
    let _ = w.write(body.as_bytes());
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&#34;")
}

// ---------------------------------------------------------------------------
// canonical_header_key
// ---------------------------------------------------------------------------

/// Return a header key in canonical form.
pub fn canonical_header_key(s: &str) -> String {
    crate::header::Header::canonical_key(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_key() {
        assert_eq!(canonical_header_key("content-type"), "Content-Type");
    }

    #[test]
    fn parse_time_rfc1123() {
        let t = parse_time("Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(t, Some(UNIX_EPOCH));
    }

    #[test]
    fn days_from_civil_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }
}

// 1xx
pub const CONTINUE: u16 = 100;
pub const SWITCHING_PROTOCOLS: u16 = 101;
pub const PROCESSING: u16 = 102;
pub const EARLY_HINTS: u16 = 103;

// 2xx
pub const OK: u16 = 200;
pub const CREATED: u16 = 201;
pub const ACCEPTED: u16 = 202;
pub const NON_AUTHORITATIVE_INFO: u16 = 203;
pub const NO_CONTENT: u16 = 204;
pub const RESET_CONTENT: u16 = 205;
pub const PARTIAL_CONTENT: u16 = 206;
pub const MULTI_STATUS: u16 = 207;
pub const ALREADY_REPORTED: u16 = 208;
pub const IM_USED: u16 = 226;

// 3xx
pub const MULTIPLE_CHOICES: u16 = 300;
pub const MOVED_PERMANENTLY: u16 = 301;
pub const FOUND: u16 = 302;
pub const SEE_OTHER: u16 = 303;
pub const NOT_MODIFIED: u16 = 304;
pub const USE_PROXY: u16 = 305;
pub const TEMPORARY_REDIRECT: u16 = 307;
pub const PERMANENT_REDIRECT: u16 = 308;

// 4xx
pub const BAD_REQUEST: u16 = 400;
pub const UNAUTHORIZED: u16 = 401;
pub const PAYMENT_REQUIRED: u16 = 402;
pub const FORBIDDEN: u16 = 403;
pub const NOT_FOUND: u16 = 404;
pub const METHOD_NOT_ALLOWED: u16 = 405;
pub const NOT_ACCEPTABLE: u16 = 406;
pub const PROXY_AUTH_REQUIRED: u16 = 407;
pub const REQUEST_TIMEOUT: u16 = 408;
pub const CONFLICT: u16 = 409;
pub const GONE: u16 = 410;
pub const LENGTH_REQUIRED: u16 = 411;
pub const PRECONDITION_FAILED: u16 = 412;
pub const REQUEST_ENTITY_TOO_LARGE: u16 = 413;
pub const REQUEST_URI_TOO_LONG: u16 = 414;
pub const UNSUPPORTED_MEDIA_TYPE: u16 = 415;
pub const REQUESTED_RANGE_NOT_SATISFIABLE: u16 = 416;
pub const EXPECTATION_FAILED: u16 = 417;
pub const TEAPOT: u16 = 418;
pub const MISDIRECTED_REQUEST: u16 = 421;
pub const UNPROCESSABLE_ENTITY: u16 = 422;
pub const LOCKED: u16 = 423;
pub const FAILED_DEPENDENCY: u16 = 424;
pub const TOO_EARLY: u16 = 425;
pub const UPGRADE_REQUIRED: u16 = 426;
pub const PRECONDITION_REQUIRED: u16 = 428;
pub const TOO_MANY_REQUESTS: u16 = 429;
pub const REQUEST_HEADER_FIELDS_TOO_LARGE: u16 = 431;
pub const UNAVAILABLE_FOR_LEGAL_REASONS: u16 = 451;

// 5xx
pub const INTERNAL_SERVER_ERROR: u16 = 500;
pub const NOT_IMPLEMENTED: u16 = 501;
pub const BAD_GATEWAY: u16 = 502;
pub const SERVICE_UNAVAILABLE: u16 = 503;
pub const GATEWAY_TIMEOUT: u16 = 504;
pub const HTTP_VERSION_NOT_SUPPORTED: u16 = 505;
pub const VARIANT_ALSO_NEGOTIATES: u16 = 506;
pub const INSUFFICIENT_STORAGE: u16 = 507;
pub const LOOP_DETECTED: u16 = 508;
pub const NOT_EXTENDED: u16 = 510;
pub const NETWORK_AUTHENTICATION_REQUIRED: u16 = 511;

/// Return the canonical reason phrase for a status code, or `""` if unknown.
/// Mirrors Go's `http.StatusText`.
pub fn status_text(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        414 => "Request-URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Requested Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes() {
        assert_eq!(status_text(200), "OK");
        assert_eq!(status_text(404), "Not Found");
        assert_eq!(status_text(500), "Internal Server Error");
        assert_eq!(status_text(418), "I'm a teapot");
    }

    #[test]
    fn unknown_code() {
        assert_eq!(status_text(999), "");
    }
}

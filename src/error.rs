use std::fmt;
use std::io;

use crate::parse::ParseError;

/// Top-level error type for go-http.
#[derive(Debug)]
pub enum HttpError {
    Io(io::Error),
    Parse(ParseError),
    InvalidUrl(String),
    Timeout,
    TooManyRedirects,
    BodyRead,
    Mime(String),
    Tls(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e)            => write!(f, "io error: {e}"),
            Self::Parse(e)         => write!(f, "parse error: {e}"),
            Self::InvalidUrl(s)    => write!(f, "invalid URL: {s}"),
            Self::Timeout          => write!(f, "request timed out"),
            Self::TooManyRedirects => write!(f, "too many redirects"),
            Self::BodyRead         => write!(f, "error reading body"),
            Self::Mime(s)          => write!(f, "mime error: {s}"),
            Self::Tls(s)           => write!(f, "TLS error: {s}"),
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e)    => Some(e),
            Self::Parse(e) => Some(e),
            _              => None,
        }
    }
}

impl From<io::Error> for HttpError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ParseError> for HttpError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}

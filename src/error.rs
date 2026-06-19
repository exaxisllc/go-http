// SPDX-License-Identifier: Apache-2.0

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn display_for_each_variant() {
        let io_err = HttpError::Io(io::Error::other("boom"));
        assert!(io_err.to_string().contains("io error"));
        assert!(io_err.to_string().contains("boom"));

        let parse = HttpError::Parse(ParseError::BadRequestLine);
        assert!(parse.to_string().contains("parse error"));

        assert!(HttpError::InvalidUrl("bad".into()).to_string().contains("invalid URL: bad"));
        assert_eq!(HttpError::Timeout.to_string(), "request timed out");
        assert_eq!(HttpError::TooManyRedirects.to_string(), "too many redirects");
        assert_eq!(HttpError::BodyRead.to_string(), "error reading body");
        assert!(HttpError::Mime("m".into()).to_string().contains("mime error: m"));
        assert!(HttpError::Tls("t".into()).to_string().contains("TLS error: t"));
    }

    #[test]
    fn source_is_set_for_wrapped_errors() {
        let io_err = HttpError::Io(io::Error::other("x"));
        assert!(io_err.source().is_some());

        let parse = HttpError::Parse(ParseError::UnexpectedEof);
        assert!(parse.source().is_some());

        // Variants without an inner cause return None.
        assert!(HttpError::Timeout.source().is_none());
        assert!(HttpError::BodyRead.source().is_none());
    }

    #[test]
    fn from_conversions() {
        let from_io: HttpError = io::Error::new(io::ErrorKind::NotFound, "nf").into();
        assert!(matches!(from_io, HttpError::Io(_)));

        let from_parse: HttpError = ParseError::BadStatusLine.into();
        assert!(matches!(from_parse, HttpError::Parse(_)));
    }
}

// SPDX-License-Identifier: Apache-2.0

/// HTTP/2 error codes and error taxonomy (RFC 9113 §7).
use std::fmt;
use std::io;

/// An HTTP/2 error code as carried by RST_STREAM and GOAWAY frames.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrCode {
    NoError            = 0x0,
    Protocol           = 0x1,
    Internal           = 0x2,
    FlowControl        = 0x3,
    SettingsTimeout    = 0x4,
    StreamClosed       = 0x5,
    FrameSize          = 0x6,
    RefusedStream      = 0x7,
    Cancel             = 0x8,
    Compression        = 0x9,
    Connect            = 0xa,
    EnhanceYourCalm    = 0xb,
    InadequateSecurity = 0xc,
    Http11Required     = 0xd,
}

impl ErrCode {
    /// Decode a wire error code.  Unknown codes are treated as `Internal`,
    /// as RFC 9113 §7 permits.
    pub fn from_u32(v: u32) -> ErrCode {
        match v {
            0x0 => ErrCode::NoError,
            0x1 => ErrCode::Protocol,
            0x2 => ErrCode::Internal,
            0x3 => ErrCode::FlowControl,
            0x4 => ErrCode::SettingsTimeout,
            0x5 => ErrCode::StreamClosed,
            0x6 => ErrCode::FrameSize,
            0x7 => ErrCode::RefusedStream,
            0x8 => ErrCode::Cancel,
            0x9 => ErrCode::Compression,
            0xa => ErrCode::Connect,
            0xb => ErrCode::EnhanceYourCalm,
            0xc => ErrCode::InadequateSecurity,
            0xd => ErrCode::Http11Required,
            _   => ErrCode::Internal,
        }
    }
}

impl fmt::Display for ErrCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ErrCode::NoError            => "NO_ERROR",
            ErrCode::Protocol           => "PROTOCOL_ERROR",
            ErrCode::Internal           => "INTERNAL_ERROR",
            ErrCode::FlowControl        => "FLOW_CONTROL_ERROR",
            ErrCode::SettingsTimeout    => "SETTINGS_TIMEOUT",
            ErrCode::StreamClosed       => "STREAM_CLOSED",
            ErrCode::FrameSize          => "FRAME_SIZE_ERROR",
            ErrCode::RefusedStream      => "REFUSED_STREAM",
            ErrCode::Cancel             => "CANCEL",
            ErrCode::Compression        => "COMPRESSION_ERROR",
            ErrCode::Connect            => "CONNECT_ERROR",
            ErrCode::EnhanceYourCalm    => "ENHANCE_YOUR_CALM",
            ErrCode::InadequateSecurity => "INADEQUATE_SECURITY",
            ErrCode::Http11Required     => "HTTP_1_1_REQUIRED",
        };
        f.write_str(s)
    }
}

/// An HTTP/2 protocol error, distinguishing connection-fatal errors from
/// per-stream errors (RFC 9113 §5.4).
#[derive(Debug)]
pub enum H2Error {
    /// Whole-connection failure: send GOAWAY(code) and tear the connection down.
    Connection(ErrCode, String),
    /// Single-stream failure: send RST_STREAM(id, code); other streams continue.
    Stream(u32, ErrCode),
    /// The peer sent GOAWAY: last processed stream id, error code, debug text.
    GoAway(u32, ErrCode, String),
    /// The connection is already closed / the writer is gone.
    Closed,
    /// Underlying transport I/O error.
    Io(io::Error),
}

impl fmt::Display for H2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            H2Error::Connection(code, msg) => write!(f, "connection error {code}: {msg}"),
            H2Error::Stream(id, code)      => write!(f, "stream {id} error {code}"),
            H2Error::GoAway(last, code, d) => {
                write!(f, "peer sent GOAWAY (last stream {last}, {code})")?;
                if !d.is_empty() { write!(f, ": {d}")?; }
                Ok(())
            }
            H2Error::Closed => f.write_str("connection closed"),
            H2Error::Io(e)  => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for H2Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            H2Error::Io(e) => Some(e),
            _              => None,
        }
    }
}

impl From<io::Error> for H2Error {
    fn from(e: io::Error) -> Self {
        H2Error::Io(e)
    }
}

impl H2Error {
    /// Convert into an `io::Error` for use inside `Read`/`Write` impls.
    pub fn into_io(self) -> io::Error {
        match self {
            H2Error::Io(e) => e,
            other          => io::Error::other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errcode_roundtrip() {
        for v in 0u32..=0xd {
            let code = ErrCode::from_u32(v);
            assert_eq!(code as u32, v);
        }
    }

    #[test]
    fn errcode_unknown_maps_to_internal() {
        assert_eq!(ErrCode::from_u32(0xff), ErrCode::Internal);
        assert_eq!(ErrCode::from_u32(u32::MAX), ErrCode::Internal);
    }

    #[test]
    fn display_forms() {
        assert_eq!(ErrCode::Protocol.to_string(), "PROTOCOL_ERROR");
        let e = H2Error::Stream(5, ErrCode::Cancel);
        assert_eq!(e.to_string(), "stream 5 error CANCEL");
        let g = H2Error::GoAway(7, ErrCode::NoError, "bye".into());
        assert!(g.to_string().contains("last stream 7"));
        assert!(g.to_string().contains("bye"));
    }
}

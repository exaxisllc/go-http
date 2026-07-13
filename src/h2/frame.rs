// SPDX-License-Identifier: Apache-2.0

/// HTTP/2 frame codec (RFC 9113 §4, §6).
///
/// A frame is a 9-byte header — 24-bit payload length, 8-bit type, 8-bit
/// flags, 1 reserved bit + 31-bit stream identifier — followed by the payload.
use std::io::{self, Read};

use super::error::{ErrCode, H2Error};

pub const FRAME_HEADER_LEN: usize = 9;

// Frame type codes (RFC 9113 §6).
pub const TYPE_DATA:          u8 = 0x0;
pub const TYPE_HEADERS:       u8 = 0x1;
pub const TYPE_PRIORITY:      u8 = 0x2;
pub const TYPE_RST_STREAM:    u8 = 0x3;
pub const TYPE_SETTINGS:      u8 = 0x4;
pub const TYPE_PUSH_PROMISE:  u8 = 0x5;
pub const TYPE_PING:          u8 = 0x6;
pub const TYPE_GOAWAY:        u8 = 0x7;
pub const TYPE_WINDOW_UPDATE: u8 = 0x8;
pub const TYPE_CONTINUATION:  u8 = 0x9;

/// Frame flag bits.  A flag's meaning depends on the frame type.
pub mod flags {
    pub const END_STREAM:  u8 = 0x1; // DATA, HEADERS
    pub const ACK:         u8 = 0x1; // SETTINGS, PING
    pub const END_HEADERS: u8 = 0x4; // HEADERS, PUSH_PROMISE, CONTINUATION
    pub const PADDED:      u8 = 0x8; // DATA, HEADERS, PUSH_PROMISE
    pub const PRIORITY:    u8 = 0x20; // HEADERS
}

// ---------------------------------------------------------------------------
// Frame header
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub length:    u32,
    pub ftype:     u8,
    pub flags:     u8,
    pub stream_id: u32,
}

/// Read exactly one 9-byte frame header.  An `UnexpectedEof` on the first
/// byte means the peer closed cleanly between frames.
pub fn read_frame_header(r: &mut impl Read) -> io::Result<FrameHeader> {
    let mut buf = [0u8; FRAME_HEADER_LEN];
    r.read_exact(&mut buf)?;
    Ok(decode_frame_header(&buf))
}

pub fn decode_frame_header(buf: &[u8; FRAME_HEADER_LEN]) -> FrameHeader {
    FrameHeader {
        length:    ((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | buf[2] as u32,
        ftype:     buf[3],
        flags:     buf[4],
        // High (reserved) bit masked off per RFC 9113 §4.1.
        stream_id: u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) & 0x7fff_ffff,
    }
}

/// Read one full frame (header + payload).  Enforces `max_frame_size` — a
/// larger payload is a connection error of type FRAME_SIZE_ERROR.
pub fn read_frame(
    r: &mut impl Read,
    max_frame_size: u32,
) -> Result<(FrameHeader, Vec<u8>), H2Error> {
    let header = read_frame_header(r).map_err(H2Error::Io)?;
    if header.length > max_frame_size {
        return Err(H2Error::Connection(
            ErrCode::FrameSize,
            format!("frame length {} exceeds max frame size {max_frame_size}", header.length),
        ));
    }
    let mut payload = vec![0u8; header.length as usize];
    r.read_exact(&mut payload).map_err(H2Error::Io)?;
    Ok((header, payload))
}

/// Append one serialized frame (header + payload) to `buf`.
///
/// The caller is responsible for splitting oversized DATA / header blocks at
/// the peer's SETTINGS_MAX_FRAME_SIZE before calling this.
pub fn write_frame(buf: &mut Vec<u8>, ftype: u8, flags: u8, stream_id: u32, payload: &[u8]) {
    debug_assert!(payload.len() < 1 << 24, "frame payload too large to encode");
    let len = payload.len() as u32;
    buf.push((len >> 16) as u8);
    buf.push((len >> 8) as u8);
    buf.push(len as u8);
    buf.push(ftype);
    buf.push(flags);
    buf.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    buf.extend_from_slice(payload);
}

// ---------------------------------------------------------------------------
// Parsed frames
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Frame {
    Data {
        stream_id:  u32,
        data:       Vec<u8>,
        end_stream: bool,
        /// Flow-controlled length: the full payload including any padding.
        flow_len:   u32,
    },
    Headers {
        stream_id:   u32,
        fragment:    Vec<u8>,
        end_stream:  bool,
        end_headers: bool,
    },
    /// Parsed for length validation only; priority signals are ignored.
    Priority { stream_id: u32 },
    RstStream { stream_id: u32, code: ErrCode },
    Settings { ack: bool, params: Vec<(u16, u32)> },
    /// Push is disabled in both directions; receipt is handled by the caller.
    PushPromise { stream_id: u32 },
    Ping { ack: bool, payload: [u8; 8] },
    GoAway { last_stream_id: u32, code: ErrCode, debug: Vec<u8> },
    WindowUpdate { stream_id: u32, increment: u32 },
    Continuation { stream_id: u32, fragment: Vec<u8>, end_headers: bool },
    /// Unknown frame types must be ignored (RFC 9113 §4.1).
    Unknown { ftype: u8 },
}

/// Interpret a raw payload according to the frame type: strip padding,
/// validate fixed lengths and stream-id-zero rules.
pub fn parse_frame(h: FrameHeader, payload: Vec<u8>) -> Result<Frame, H2Error> {
    match h.ftype {
        TYPE_DATA => {
            require_stream(h, "DATA")?;
            let flow_len = h.length;
            let data = strip_padding(&h, payload, 0)?;
            Ok(Frame::Data {
                stream_id:  h.stream_id,
                data,
                end_stream: h.flags & flags::END_STREAM != 0,
                flow_len,
            })
        }
        TYPE_HEADERS => {
            require_stream(h, "HEADERS")?;
            // After padding removal, a PRIORITY flag means a 5-byte
            // exclusive-bit/dependency/weight prefix we skip.
            let prio_len = if h.flags & flags::PRIORITY != 0 { 5 } else { 0 };
            let body = strip_padding(&h, payload, prio_len)?;
            Ok(Frame::Headers {
                stream_id:   h.stream_id,
                fragment:    body,
                end_stream:  h.flags & flags::END_STREAM != 0,
                end_headers: h.flags & flags::END_HEADERS != 0,
            })
        }
        TYPE_PRIORITY => {
            require_stream(h, "PRIORITY")?;
            if payload.len() != 5 {
                // Stream error, not connection error (RFC 9113 §6.3).
                return Err(H2Error::Stream(h.stream_id, ErrCode::FrameSize));
            }
            Ok(Frame::Priority { stream_id: h.stream_id })
        }
        TYPE_RST_STREAM => {
            require_stream(h, "RST_STREAM")?;
            if payload.len() != 4 {
                return Err(frame_size_conn_error("RST_STREAM", 4, payload.len()));
            }
            let code = ErrCode::from_u32(u32::from_be_bytes(payload[..4].try_into().unwrap()));
            Ok(Frame::RstStream { stream_id: h.stream_id, code })
        }
        TYPE_SETTINGS => {
            require_conn(h, "SETTINGS")?;
            let ack = h.flags & flags::ACK != 0;
            if ack && !payload.is_empty() {
                return Err(frame_size_conn_error("SETTINGS ACK", 0, payload.len()));
            }
            if !payload.len().is_multiple_of(6) {
                return Err(H2Error::Connection(
                    ErrCode::FrameSize,
                    format!("SETTINGS payload length {} not a multiple of 6", payload.len()),
                ));
            }
            let params = payload
                .chunks_exact(6)
                .map(|c| {
                    (
                        u16::from_be_bytes([c[0], c[1]]),
                        u32::from_be_bytes([c[2], c[3], c[4], c[5]]),
                    )
                })
                .collect();
            Ok(Frame::Settings { ack, params })
        }
        TYPE_PUSH_PROMISE => {
            require_stream(h, "PUSH_PROMISE")?;
            Ok(Frame::PushPromise { stream_id: h.stream_id })
        }
        TYPE_PING => {
            require_conn(h, "PING")?;
            if payload.len() != 8 {
                return Err(frame_size_conn_error("PING", 8, payload.len()));
            }
            Ok(Frame::Ping {
                ack:     h.flags & flags::ACK != 0,
                payload: payload[..8].try_into().unwrap(),
            })
        }
        TYPE_GOAWAY => {
            require_conn(h, "GOAWAY")?;
            if payload.len() < 8 {
                return Err(frame_size_conn_error("GOAWAY", 8, payload.len()));
            }
            Ok(Frame::GoAway {
                last_stream_id: u32::from_be_bytes(payload[..4].try_into().unwrap()) & 0x7fff_ffff,
                code:  ErrCode::from_u32(u32::from_be_bytes(payload[4..8].try_into().unwrap())),
                debug: payload[8..].to_vec(),
            })
        }
        TYPE_WINDOW_UPDATE => {
            // Legal on stream 0 (connection window) or any stream.
            if payload.len() != 4 {
                return Err(frame_size_conn_error("WINDOW_UPDATE", 4, payload.len()));
            }
            let increment = u32::from_be_bytes(payload[..4].try_into().unwrap()) & 0x7fff_ffff;
            if increment == 0 {
                // Zero increment: connection error on stream 0, stream error otherwise.
                return if h.stream_id == 0 {
                    Err(H2Error::Connection(
                        ErrCode::Protocol,
                        "WINDOW_UPDATE with zero increment on connection".into(),
                    ))
                } else {
                    Err(H2Error::Stream(h.stream_id, ErrCode::Protocol))
                };
            }
            Ok(Frame::WindowUpdate { stream_id: h.stream_id, increment })
        }
        TYPE_CONTINUATION => {
            require_stream(h, "CONTINUATION")?;
            Ok(Frame::Continuation {
                stream_id:   h.stream_id,
                fragment:    payload,
                end_headers: h.flags & flags::END_HEADERS != 0,
            })
        }
        other => Ok(Frame::Unknown { ftype: other }),
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Frame types that must be associated with a stream (id != 0).
fn require_stream(h: FrameHeader, name: &str) -> Result<(), H2Error> {
    if h.stream_id == 0 {
        return Err(H2Error::Connection(
            ErrCode::Protocol,
            format!("{name} frame on stream 0"),
        ));
    }
    Ok(())
}

/// Frame types that apply to the whole connection (id == 0).
fn require_conn(h: FrameHeader, name: &str) -> Result<(), H2Error> {
    if h.stream_id != 0 {
        return Err(H2Error::Connection(
            ErrCode::Protocol,
            format!("{name} frame on stream {}", h.stream_id),
        ));
    }
    Ok(())
}

fn frame_size_conn_error(name: &str, want: usize, got: usize) -> H2Error {
    H2Error::Connection(
        ErrCode::FrameSize,
        format!("{name} frame length {got}, expected {want}"),
    )
}

/// Remove PADDED framing (1-byte pad length prefix + trailing pad) and an
/// optional fixed-size prefix (the HEADERS priority fields), returning the
/// remaining payload.
fn strip_padding(h: &FrameHeader, payload: Vec<u8>, prefix: usize) -> Result<Vec<u8>, H2Error> {
    let padded = h.flags & flags::PADDED != 0;
    let mut start = 0usize;
    let mut end   = payload.len();

    if padded {
        if payload.is_empty() {
            return Err(H2Error::Connection(
                ErrCode::FrameSize,
                "PADDED frame with empty payload".into(),
            ));
        }
        let pad_len = payload[0] as usize;
        start = 1;
        // Pad length must leave room for the prefix + at least zero data.
        if pad_len + start + prefix > payload.len() {
            return Err(H2Error::Connection(
                ErrCode::Protocol,
                "padding exceeds frame payload".into(),
            ));
        }
        end = payload.len() - pad_len;
    }

    start += prefix;
    if start > end {
        return Err(H2Error::Connection(
            ErrCode::FrameSize,
            "frame too short for its prefix fields".into(),
        ));
    }
    Ok(payload[start..end].to_vec())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(ftype: u8, fl: u8, stream_id: u32, payload: &[u8]) -> (FrameHeader, Vec<u8>) {
        let mut buf = Vec::new();
        write_frame(&mut buf, ftype, fl, stream_id, payload);
        let mut cursor = io::Cursor::new(buf);
        read_frame(&mut cursor, 16_384).unwrap()
    }

    #[test]
    fn header_roundtrip() {
        let (h, payload) = roundtrip(TYPE_DATA, flags::END_STREAM, 3, b"hello");
        assert_eq!(h.length, 5);
        assert_eq!(h.ftype, TYPE_DATA);
        assert_eq!(h.flags, flags::END_STREAM);
        assert_eq!(h.stream_id, 3);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn reserved_bit_masked() {
        let mut buf = Vec::new();
        write_frame(&mut buf, TYPE_PING, 0, 0, &[0u8; 8]);
        buf[5] |= 0x80; // set the reserved bit on the wire
        let mut cursor = io::Cursor::new(buf);
        let (h, _) = read_frame(&mut cursor, 16_384).unwrap();
        assert_eq!(h.stream_id, 0);
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut buf = Vec::new();
        write_frame(&mut buf, TYPE_DATA, 0, 1, &[0u8; 100]);
        let mut cursor = io::Cursor::new(buf);
        let err = read_frame(&mut cursor, 50).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::FrameSize, _)), "{err:?}");
    }

    #[test]
    fn data_frame_parse() {
        let h = FrameHeader { length: 5, ftype: TYPE_DATA, flags: flags::END_STREAM, stream_id: 1 };
        match parse_frame(h, b"hello".to_vec()).unwrap() {
            Frame::Data { stream_id, data, end_stream, flow_len } => {
                assert_eq!(stream_id, 1);
                assert_eq!(data, b"hello");
                assert!(end_stream);
                assert_eq!(flow_len, 5);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn data_frame_padded() {
        // payload = [pad_len=3][data "ab"][3 pad bytes]
        let payload = vec![3u8, b'a', b'b', 0, 0, 0];
        let h = FrameHeader {
            length: payload.len() as u32,
            ftype: TYPE_DATA,
            flags: flags::PADDED,
            stream_id: 1,
        };
        match parse_frame(h, payload).unwrap() {
            Frame::Data { data, flow_len, .. } => {
                assert_eq!(data, b"ab");
                assert_eq!(flow_len, 6, "flow-controlled length includes padding");
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn data_padding_overflow_rejected() {
        // pad_len 10 > remaining payload
        let payload = vec![10u8, b'a'];
        let h = FrameHeader { length: 2, ftype: TYPE_DATA, flags: flags::PADDED, stream_id: 1 };
        let err = parse_frame(h, payload).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::Protocol, _)), "{err:?}");
    }

    #[test]
    fn data_on_stream_zero_rejected() {
        let h = FrameHeader { length: 0, ftype: TYPE_DATA, flags: 0, stream_id: 0 };
        let err = parse_frame(h, vec![]).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::Protocol, _)));
    }

    #[test]
    fn headers_with_priority_prefix() {
        // 5-byte priority prefix then fragment "xyz".
        let mut payload = vec![0, 0, 0, 1, 16];
        payload.extend_from_slice(b"xyz");
        let h = FrameHeader {
            length: payload.len() as u32,
            ftype: TYPE_HEADERS,
            flags: flags::PRIORITY | flags::END_HEADERS,
            stream_id: 5,
        };
        match parse_frame(h, payload).unwrap() {
            Frame::Headers { fragment, end_headers, end_stream, .. } => {
                assert_eq!(fragment, b"xyz");
                assert!(end_headers);
                assert!(!end_stream);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn headers_padded_and_priority() {
        // [pad_len=2][5-byte priority]["hi"][2 pad]
        let mut payload = vec![2u8];
        payload.extend_from_slice(&[0, 0, 0, 3, 10]);
        payload.extend_from_slice(b"hi");
        payload.extend_from_slice(&[0, 0]);
        let h = FrameHeader {
            length: payload.len() as u32,
            ftype: TYPE_HEADERS,
            flags: flags::PADDED | flags::PRIORITY,
            stream_id: 7,
        };
        match parse_frame(h, payload).unwrap() {
            Frame::Headers { fragment, .. } => assert_eq!(fragment, b"hi"),
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn settings_parse() {
        let payload = vec![
            0x00, 0x04, 0x00, 0x01, 0x00, 0x00, // INITIAL_WINDOW_SIZE = 65536
            0x00, 0x05, 0x00, 0x00, 0x40, 0x00, // MAX_FRAME_SIZE = 16384
        ];
        let h = FrameHeader { length: 12, ftype: TYPE_SETTINGS, flags: 0, stream_id: 0 };
        match parse_frame(h, payload).unwrap() {
            Frame::Settings { ack, params } => {
                assert!(!ack);
                assert_eq!(params, vec![(4, 65536), (5, 16384)]);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn settings_bad_length_rejected() {
        let h = FrameHeader { length: 5, ftype: TYPE_SETTINGS, flags: 0, stream_id: 0 };
        let err = parse_frame(h, vec![0; 5]).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::FrameSize, _)));
    }

    #[test]
    fn settings_ack_with_payload_rejected() {
        let h = FrameHeader { length: 6, ftype: TYPE_SETTINGS, flags: flags::ACK, stream_id: 0 };
        let err = parse_frame(h, vec![0; 6]).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::FrameSize, _)));
    }

    #[test]
    fn settings_on_nonzero_stream_rejected() {
        let h = FrameHeader { length: 0, ftype: TYPE_SETTINGS, flags: 0, stream_id: 1 };
        let err = parse_frame(h, vec![]).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::Protocol, _)));
    }

    #[test]
    fn ping_roundtrip() {
        let h = FrameHeader { length: 8, ftype: TYPE_PING, flags: flags::ACK, stream_id: 0 };
        match parse_frame(h, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap() {
            Frame::Ping { ack, payload } => {
                assert!(ack);
                assert_eq!(payload, [1, 2, 3, 4, 5, 6, 7, 8]);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn ping_wrong_length_rejected() {
        let h = FrameHeader { length: 7, ftype: TYPE_PING, flags: 0, stream_id: 0 };
        let err = parse_frame(h, vec![0; 7]).unwrap_err();
        assert!(matches!(err, H2Error::Connection(ErrCode::FrameSize, _)));
    }

    #[test]
    fn goaway_parse() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&9u32.to_be_bytes());
        payload.extend_from_slice(&(ErrCode::EnhanceYourCalm as u32).to_be_bytes());
        payload.extend_from_slice(b"slow down");
        let h = FrameHeader {
            length: payload.len() as u32,
            ftype: TYPE_GOAWAY,
            flags: 0,
            stream_id: 0,
        };
        match parse_frame(h, payload).unwrap() {
            Frame::GoAway { last_stream_id, code, debug } => {
                assert_eq!(last_stream_id, 9);
                assert_eq!(code, ErrCode::EnhanceYourCalm);
                assert_eq!(debug, b"slow down");
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn window_update_parse() {
        let h = FrameHeader { length: 4, ftype: TYPE_WINDOW_UPDATE, flags: 0, stream_id: 3 };
        match parse_frame(h, 1000u32.to_be_bytes().to_vec()).unwrap() {
            Frame::WindowUpdate { stream_id, increment } => {
                assert_eq!(stream_id, 3);
                assert_eq!(increment, 1000);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn window_update_zero_increment() {
        let h0 = FrameHeader { length: 4, ftype: TYPE_WINDOW_UPDATE, flags: 0, stream_id: 0 };
        assert!(matches!(
            parse_frame(h0, 0u32.to_be_bytes().to_vec()).unwrap_err(),
            H2Error::Connection(ErrCode::Protocol, _)
        ));
        let h1 = FrameHeader { length: 4, ftype: TYPE_WINDOW_UPDATE, flags: 0, stream_id: 5 };
        assert!(matches!(
            parse_frame(h1, 0u32.to_be_bytes().to_vec()).unwrap_err(),
            H2Error::Stream(5, ErrCode::Protocol)
        ));
    }

    #[test]
    fn rst_stream_parse() {
        let h = FrameHeader { length: 4, ftype: TYPE_RST_STREAM, flags: 0, stream_id: 1 };
        match parse_frame(h, (ErrCode::Cancel as u32).to_be_bytes().to_vec()).unwrap() {
            Frame::RstStream { stream_id, code } => {
                assert_eq!(stream_id, 1);
                assert_eq!(code, ErrCode::Cancel);
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn priority_wrong_length_is_stream_error() {
        let h = FrameHeader { length: 4, ftype: TYPE_PRIORITY, flags: 0, stream_id: 3 };
        assert!(matches!(
            parse_frame(h, vec![0; 4]).unwrap_err(),
            H2Error::Stream(3, ErrCode::FrameSize)
        ));
    }

    #[test]
    fn unknown_frame_type_ignored() {
        let h = FrameHeader { length: 3, ftype: 0xEE, flags: 0xFF, stream_id: 12 };
        assert!(matches!(parse_frame(h, vec![1, 2, 3]).unwrap(), Frame::Unknown { ftype: 0xEE }));
    }
}

// SPDX-License-Identifier: Apache-2.0

/// SETTINGS parameters (RFC 9113 §6.5.2).
use super::error::{ErrCode, H2Error};

pub const HEADER_TABLE_SIZE:      u16 = 0x1;
pub const ENABLE_PUSH:            u16 = 0x2;
pub const MAX_CONCURRENT_STREAMS: u16 = 0x3;
pub const INITIAL_WINDOW_SIZE:    u16 = 0x4;
pub const MAX_FRAME_SIZE:         u16 = 0x5;
pub const MAX_HEADER_LIST_SIZE:   u16 = 0x6;

pub const DEFAULT_WINDOW:    u32 = 65_535;
pub const DEFAULT_MAX_FRAME: u32 = 16_384;
pub const MAX_WINDOW:        u32 = (1 << 31) - 1;

/// The maximum concurrent streams we advertise as a server.
pub const DEFAULT_MAX_CONCURRENT_STREAMS: u32 = 250;

/// One endpoint's settings — either ours (what we advertise) or the peer's
/// (what we must respect when sending).
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub header_table_size:      u32,
    pub enable_push:            bool,
    pub max_concurrent_streams: Option<u32>,
    pub initial_window_size:    u32,
    pub max_frame_size:         u32,
    pub max_header_list_size:   Option<u32>,
}

impl Default for Settings {
    /// RFC defaults — the assumed state before any SETTINGS frame arrives.
    fn default() -> Self {
        Self {
            header_table_size:      4096,
            enable_push:            true,
            max_concurrent_streams: None,
            initial_window_size:    DEFAULT_WINDOW,
            max_frame_size:         DEFAULT_MAX_FRAME,
            max_header_list_size:   None,
        }
    }
}

impl Settings {
    /// The settings this implementation advertises.  Push is always disabled;
    /// servers additionally bound concurrent streams.
    pub fn default_ours(max_header_list: u32, server: bool) -> Settings {
        Settings {
            header_table_size:      4096,
            enable_push:            false,
            max_concurrent_streams: server.then_some(DEFAULT_MAX_CONCURRENT_STREAMS),
            initial_window_size:    DEFAULT_WINDOW,
            max_frame_size:         DEFAULT_MAX_FRAME,
            max_header_list_size:   Some(max_header_list),
        }
    }

    /// Serialize into (id, value) pairs for a SETTINGS frame payload.
    /// Values equal to the RFC default are omitted, except ENABLE_PUSH=0
    /// which differs from the default and must be sent.
    pub fn serialize(&self) -> Vec<(u16, u32)> {
        let mut out = Vec::new();
        if self.header_table_size != 4096 {
            out.push((HEADER_TABLE_SIZE, self.header_table_size));
        }
        if !self.enable_push {
            out.push((ENABLE_PUSH, 0));
        }
        if let Some(m) = self.max_concurrent_streams {
            out.push((MAX_CONCURRENT_STREAMS, m));
        }
        if self.initial_window_size != DEFAULT_WINDOW {
            out.push((INITIAL_WINDOW_SIZE, self.initial_window_size));
        }
        if self.max_frame_size != DEFAULT_MAX_FRAME {
            out.push((MAX_FRAME_SIZE, self.max_frame_size));
        }
        if let Some(m) = self.max_header_list_size {
            out.push((MAX_HEADER_LIST_SIZE, m));
        }
        out
    }

    /// Apply a peer SETTINGS frame with RFC 9113 §6.5.2 validation.
    /// Unknown identifiers are ignored.
    ///
    /// Returns the change in `initial_window_size` (new − old) so the caller
    /// can adjust every open stream's send window (RFC 9113 §6.9.2).
    pub fn apply(&mut self, params: &[(u16, u32)]) -> Result<i64, H2Error> {
        let old_window = self.initial_window_size as i64;
        for &(id, value) in params {
            match id {
                HEADER_TABLE_SIZE => self.header_table_size = value,
                ENABLE_PUSH => {
                    self.enable_push = match value {
                        0 => false,
                        1 => true,
                        _ => {
                            return Err(H2Error::Connection(
                                ErrCode::Protocol,
                                format!("ENABLE_PUSH must be 0 or 1, got {value}"),
                            ))
                        }
                    };
                }
                MAX_CONCURRENT_STREAMS => self.max_concurrent_streams = Some(value),
                INITIAL_WINDOW_SIZE => {
                    if value > MAX_WINDOW {
                        return Err(H2Error::Connection(
                            ErrCode::FlowControl,
                            format!("INITIAL_WINDOW_SIZE {value} exceeds 2^31-1"),
                        ));
                    }
                    self.initial_window_size = value;
                }
                MAX_FRAME_SIZE => {
                    if !(DEFAULT_MAX_FRAME..=16_777_215).contains(&value) {
                        return Err(H2Error::Connection(
                            ErrCode::Protocol,
                            format!("MAX_FRAME_SIZE {value} outside 16384..=16777215"),
                        ));
                    }
                    self.max_frame_size = value;
                }
                MAX_HEADER_LIST_SIZE => self.max_header_list_size = Some(value),
                _ => {} // unknown settings must be ignored
            }
        }
        Ok(self.initial_window_size as i64 - old_window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_rfc() {
        let s = Settings::default();
        assert_eq!(s.header_table_size, 4096);
        assert!(s.enable_push);
        assert_eq!(s.max_concurrent_streams, None);
        assert_eq!(s.initial_window_size, 65_535);
        assert_eq!(s.max_frame_size, 16_384);
    }

    #[test]
    fn ours_disables_push() {
        let server = Settings::default_ours(1 << 20, true);
        assert!(!server.enable_push);
        assert_eq!(server.max_concurrent_streams, Some(DEFAULT_MAX_CONCURRENT_STREAMS));
        assert_eq!(server.max_header_list_size, Some(1 << 20));

        let client = Settings::default_ours(1 << 20, false);
        assert_eq!(client.max_concurrent_streams, None);

        let ser = client.serialize();
        assert!(ser.contains(&(ENABLE_PUSH, 0)), "must advertise push disabled: {ser:?}");
    }

    #[test]
    fn apply_updates_and_reports_window_delta() {
        let mut s = Settings::default();
        let delta = s
            .apply(&[(INITIAL_WINDOW_SIZE, 100_000), (MAX_FRAME_SIZE, 20_000)])
            .unwrap();
        assert_eq!(delta, 100_000 - 65_535);
        assert_eq!(s.initial_window_size, 100_000);
        assert_eq!(s.max_frame_size, 20_000);

        // Shrinking reports a negative delta.
        let delta2 = s.apply(&[(INITIAL_WINDOW_SIZE, 1000)]).unwrap();
        assert_eq!(delta2, 1000 - 100_000);
    }

    #[test]
    fn apply_ignores_unknown_ids() {
        let mut s = Settings::default();
        assert_eq!(s.apply(&[(0x99, 42)]).unwrap(), 0);
    }

    #[test]
    fn apply_validates_enable_push() {
        let mut s = Settings::default();
        assert!(matches!(
            s.apply(&[(ENABLE_PUSH, 2)]).unwrap_err(),
            H2Error::Connection(ErrCode::Protocol, _)
        ));
        s.apply(&[(ENABLE_PUSH, 0)]).unwrap();
        assert!(!s.enable_push);
    }

    #[test]
    fn apply_validates_window_size() {
        let mut s = Settings::default();
        assert!(matches!(
            s.apply(&[(INITIAL_WINDOW_SIZE, 1 << 31)]).unwrap_err(),
            H2Error::Connection(ErrCode::FlowControl, _)
        ));
    }

    #[test]
    fn apply_validates_max_frame_size() {
        let mut s = Settings::default();
        assert!(s.apply(&[(MAX_FRAME_SIZE, 16_383)]).is_err());
        assert!(s.apply(&[(MAX_FRAME_SIZE, 16_777_216)]).is_err());
        assert!(s.apply(&[(MAX_FRAME_SIZE, 16_777_215)]).is_ok());
    }
}

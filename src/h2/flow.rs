// SPDX-License-Identifier: Apache-2.0

/// Flow control (RFC 9113 §5.2, §6.9).
///
/// **Send direction** ([`SendWindows`]): tracks the connection window and
/// per-stream windows granted by the peer.  DATA writers call [`reserve`],
/// which parks on a goroutine-aware `Cond` until window is available.
/// WINDOW_UPDATE and SETTINGS handling wake the waiters.
///
/// **Receive direction** ([`RecvWindow`]): connection-level accounting for
/// what we allow the peer to send.  Consumed bytes accumulate and are
/// released back to the peer in ≥32 KiB WINDOW_UPDATE batches.
/// (Per-stream receive accounting lives inside `StreamInbound` in conn.rs.)
use std::collections::HashMap;
use std::sync::Mutex;

use go_lib::sync::Cond;

use super::error::{ErrCode, H2Error};
use super::settings::MAX_WINDOW;

/// Send a WINDOW_UPDATE once this many consumed bytes have accumulated
/// (half the default 64 KiB window).
pub const WINDOW_UPDATE_THRESHOLD: u32 = 32 * 1024;

// ---------------------------------------------------------------------------
// SendWindows
// ---------------------------------------------------------------------------

struct StreamSend {
    window: i64,
    failed: bool,
}

struct SendState {
    conn: i64,
    /// Initial window applied to newly opened streams
    /// (peer's SETTINGS_INITIAL_WINDOW_SIZE).
    initial: i64,
    streams: HashMap<u32, StreamSend>,
    dead: bool,
}

/// Outbound flow-control windows for one connection.
pub struct SendWindows {
    inner: Mutex<SendState>,
    cond:  Cond,
}

impl SendWindows {
    pub fn new(initial: u32) -> SendWindows {
        SendWindows {
            inner: Mutex::new(SendState {
                conn:    super::settings::DEFAULT_WINDOW as i64,
                initial: initial as i64,
                streams: HashMap::new(),
                dead:    false,
            }),
            cond: Cond::new(),
        }
    }

    /// Register a new stream with the current initial window.
    pub fn open_stream(&self, id: u32) {
        let mut s = self.inner.lock().unwrap();
        let initial = s.initial;
        s.streams.insert(id, StreamSend { window: initial, failed: false });
    }

    /// Remove a finished stream.  Any writer still blocked on it errors out.
    pub fn close_stream(&self, id: u32) {
        let mut s = self.inner.lock().unwrap();
        s.streams.remove(&id);
        drop(s);
        self.cond.notify_all();
    }

    /// Block until at least one byte of window is available on both the
    /// connection and stream `id`, then reserve up to `want` bytes.
    /// Returns the number of bytes granted (≥ 1).
    pub fn reserve(&self, id: u32, want: usize) -> Result<usize, H2Error> {
        let mut s = self.inner.lock().unwrap();
        loop {
            if s.dead {
                return Err(H2Error::Closed);
            }
            let stream = match s.streams.get(&id) {
                None => return Err(H2Error::Stream(id, ErrCode::StreamClosed)),
                Some(st) => st,
            };
            if stream.failed {
                return Err(H2Error::Stream(id, ErrCode::Cancel));
            }
            let avail = s.conn.min(stream.window);
            if avail > 0 {
                let grant = (want as i64).min(avail);
                s.conn -= grant;
                s.streams.get_mut(&id).unwrap().window -= grant;
                return Ok(grant as usize);
            }
            s = self.cond.wait(&self.inner, s);
        }
    }

    /// Apply a connection-level WINDOW_UPDATE.
    pub fn add_conn(&self, delta: u32) -> Result<(), H2Error> {
        let mut s = self.inner.lock().unwrap();
        s.conn += delta as i64;
        if s.conn > MAX_WINDOW as i64 {
            return Err(H2Error::Connection(
                ErrCode::FlowControl,
                "connection send window overflow".into(),
            ));
        }
        drop(s);
        self.cond.notify_all();
        Ok(())
    }

    /// Apply a stream-level WINDOW_UPDATE.  Updates for unknown (closed)
    /// streams are ignored, per RFC 9113 §5.1.
    pub fn add_stream(&self, id: u32, delta: u32) -> Result<(), H2Error> {
        let mut s = self.inner.lock().unwrap();
        if let Some(st) = s.streams.get_mut(&id) {
            st.window += delta as i64;
            if st.window > MAX_WINDOW as i64 {
                return Err(H2Error::Stream(id, ErrCode::FlowControl));
            }
        }
        drop(s);
        self.cond.notify_all();
        Ok(())
    }

    /// Apply a SETTINGS_INITIAL_WINDOW_SIZE change: adjust every open
    /// stream's window by the delta (RFC 9113 §6.9.2).  Windows may go
    /// negative; writers stay blocked until WINDOW_UPDATEs bring them back up.
    pub fn apply_initial_window_delta(&self, delta: i64) -> Result<(), H2Error> {
        let mut s = self.inner.lock().unwrap();
        s.initial += delta;
        for st in s.streams.values_mut() {
            st.window += delta;
            if st.window > MAX_WINDOW as i64 {
                return Err(H2Error::Connection(
                    ErrCode::FlowControl,
                    "stream send window overflow after SETTINGS".into(),
                ));
            }
        }
        drop(s);
        if delta > 0 {
            self.cond.notify_all();
        }
        Ok(())
    }

    /// Mark one stream failed (RST_STREAM received); wake its blocked writers.
    pub fn fail_stream(&self, id: u32) {
        let mut s = self.inner.lock().unwrap();
        if let Some(st) = s.streams.get_mut(&id) {
            st.failed = true;
        }
        drop(s);
        self.cond.notify_all();
    }

    /// Mark the whole connection dead; every blocked writer errors out.
    pub fn fail_all(&self) {
        self.inner.lock().unwrap().dead = true;
        self.cond.notify_all();
    }
}

// ---------------------------------------------------------------------------
// RecvWindow — connection-level receive accounting
// ---------------------------------------------------------------------------

struct RecvState {
    /// Bytes the peer may still send us before a WINDOW_UPDATE.
    remaining: i64,
    /// Consumed bytes not yet returned to the peer.
    pending: u32,
}

/// Connection-level receive window.
pub struct RecvWindow {
    inner: Mutex<RecvState>,
}

impl RecvWindow {
    pub fn new() -> RecvWindow {
        RecvWindow {
            inner: Mutex::new(RecvState {
                remaining: super::settings::DEFAULT_WINDOW as i64,
                pending:   0,
            }),
        }
    }

    /// Account for a received DATA frame (flow-controlled length).
    /// Errors if the peer overran our advertised window.
    pub fn on_data(&self, len: u32) -> Result<(), H2Error> {
        let mut s = self.inner.lock().unwrap();
        s.remaining -= len as i64;
        if s.remaining < 0 {
            return Err(H2Error::Connection(
                ErrCode::FlowControl,
                "peer exceeded connection flow-control window".into(),
            ));
        }
        Ok(())
    }

    /// Credit `len` consumed bytes.  Returns `Some(increment)` when enough
    /// has accumulated that a connection WINDOW_UPDATE should be sent.
    pub fn consumed(&self, len: u32) -> Option<u32> {
        let mut s = self.inner.lock().unwrap();
        s.pending += len;
        if s.pending >= WINDOW_UPDATE_THRESHOLD {
            let inc = s.pending;
            s.pending = 0;
            s.remaining += inc as i64;
            Some(inc)
        } else {
            None
        }
    }
}

impl Default for RecvWindow {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn reserve_grants_min_of_windows() {
        let w = SendWindows::new(100);
        w.open_stream(1);
        // Stream window (100) is smaller than conn window (65535).
        assert_eq!(w.reserve(1, 500).unwrap(), 100);
        // Stream exhausted; replenish via WINDOW_UPDATE.
        w.add_stream(1, 40).unwrap();
        assert_eq!(w.reserve(1, 500).unwrap(), 40);
    }

    #[test]
    fn reserve_caps_at_want() {
        let w = SendWindows::new(65_535);
        w.open_stream(1);
        assert_eq!(w.reserve(1, 10).unwrap(), 10);
    }

    #[test]
    fn conn_window_shared_across_streams() {
        let w = SendWindows::new(1 << 20);
        w.open_stream(1);
        w.open_stream(3);
        // Conn window is 65535; stream windows are 1 MiB.
        assert_eq!(w.reserve(1, 60_000).unwrap(), 60_000);
        assert_eq!(w.reserve(3, 60_000).unwrap(), 5_535);
    }

    #[test]
    fn closed_stream_errors() {
        let w = SendWindows::new(65_535);
        w.open_stream(1);
        w.close_stream(1);
        assert!(matches!(
            w.reserve(1, 10).unwrap_err(),
            H2Error::Stream(1, ErrCode::StreamClosed)
        ));
    }

    #[test]
    fn failed_stream_errors() {
        let w = SendWindows::new(65_535);
        w.open_stream(1);
        w.fail_stream(1);
        assert!(matches!(
            w.reserve(1, 10).unwrap_err(),
            H2Error::Stream(1, ErrCode::Cancel)
        ));
    }

    #[test]
    fn dead_connection_errors() {
        let w = SendWindows::new(65_535);
        w.open_stream(1);
        w.fail_all();
        assert!(matches!(w.reserve(1, 10).unwrap_err(), H2Error::Closed));
    }

    #[test]
    fn window_overflow_detected() {
        let w = SendWindows::new(65_535);
        w.open_stream(1);
        assert!(w.add_conn(MAX_WINDOW).is_err());

        let w2 = SendWindows::new(65_535);
        w2.open_stream(1);
        assert!(matches!(
            w2.add_stream(1, MAX_WINDOW).unwrap_err(),
            H2Error::Stream(1, ErrCode::FlowControl)
        ));
    }

    #[test]
    fn settings_delta_can_go_negative() {
        let w = SendWindows::new(1000);
        w.open_stream(1);
        assert_eq!(w.reserve(1, 900).unwrap(), 900); // stream window now 100
        // Peer shrinks initial window by 500: stream window goes to -400.
        w.apply_initial_window_delta(-500).unwrap();
        // Grow it back; only then is window available again.
        w.apply_initial_window_delta(500).unwrap();
        assert_eq!(w.reserve(1, 100).unwrap(), 100);
    }

    /// A blocked writer parks until a WINDOW_UPDATE arrives from another
    /// goroutine.
    #[test]
    #[go_lib::main]
    fn reserve_blocks_until_update() {
        let w = Arc::new(SendWindows::new(10));
        w.open_stream(1);
        assert_eq!(w.reserve(1, 10).unwrap(), 10); // exhaust the stream window

        let w2 = Arc::clone(&w);
        go_lib::go!(move || {
            go_lib::sleep(std::time::Duration::from_millis(20));
            w2.add_stream(1, 25).unwrap();
        });

        // Parks until the goroutine above replenishes the window.
        assert_eq!(w.reserve(1, 100).unwrap(), 25);
    }

    #[test]
    fn recv_window_thresholds() {
        let r = RecvWindow::new();
        r.on_data(40_000).unwrap();
        assert_eq!(r.consumed(10_000), None);
        // Crossing 32 KiB releases the full pending amount.
        assert_eq!(r.consumed(30_000), Some(40_000));
        // Window restored: another 60 KB is fine.
        r.on_data(60_000).unwrap();
        assert!(r.on_data(30_000).is_err(), "overrun must be detected");
    }
}

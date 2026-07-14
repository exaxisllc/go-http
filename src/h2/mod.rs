// SPDX-License-Identifier: Apache-2.0

/// HTTP/2 (RFC 9113) support.
///
/// The server negotiates h2 via ALPN on TLS listeners and — when
/// `Server::enable_h2c` is set — via prior-knowledge cleartext on plain
/// listeners.  The client speaks h2 automatically when ALPN selects it, or
/// with `Transport::h2c_prior_knowledge` for cleartext.
///
/// Handlers and `RoundTripper`s are protocol-agnostic; the types in this
/// module adapt HTTP/2 framing to the same `Handler`, `ResponseWriter`, and
/// `Request`/`Response` interfaces used by HTTP/1.1.
pub mod client;
pub mod conn;
pub mod error;
pub mod flow;
pub mod frame;
pub mod hpack;
pub mod io;
pub mod server;
pub mod settings;

/// The client connection preface (RFC 9113 §3.4).  Sent by clients before any
/// frame; servers verify it before reading the first SETTINGS frame.
pub const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

pub use error::{ErrCode, H2Error};

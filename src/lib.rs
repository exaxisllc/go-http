// SPDX-License-Identifier: Apache-2.0

pub mod context;
pub mod cookie;
pub mod error;
pub mod handler;
pub mod header;
pub mod method;
pub mod mime;
pub mod parse;
pub mod request;
pub mod response;
pub mod server;
pub mod client;
pub mod status;
pub mod tls;
pub mod util;

/// Process-wide mutex serialising unit tests that call `go_lib::run()`.
///
/// go-lib's netpoll backend is a process-global singleton: concurrent
/// `go_lib::run()` calls from different test threads race on the netpoll
/// goroutine-pointer storage, causing SIGSEGV / access violations.
///
/// Any test that invokes `go_lib::run()` must acquire this lock:
/// ```ignore
/// let _g = crate::TEST_NET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
/// ```
#[cfg(test)]
pub static TEST_NET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

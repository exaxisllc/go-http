// SPDX-License-Identifier: Apache-2.0
//! Keep-alive idle timeout.
//!
//! Demonstrates `Server::idle_timeout`, which closes a keep-alive connection
//! if no new request arrives within the configured duration.  This prevents
//! idle clients from holding connections open indefinitely.
//!
//! Run: `cargo run --example idle_timeout`
//!
//! Or test manually:
//!   # First request — fast, succeeds:
//!   curl -v --keepalive-time 0 http://127.0.0.1:8089/ping
//!
//!   # Keep-alive connection with a delay > idle_timeout — closed by server:
//!   (sleep 0.2 && echo -e "GET /ping HTTP/1.1\r\nHost: 127.0.0.1:8089\r\n\r\n") \
//!       | nc 127.0.0.1 8089

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use go_lib::net::TcpStream;
use go_http::{handler::ServeMux, server::Server};

const ADDR: &str = "127.0.0.1:8089";
const IDLE: Duration = Duration::from_millis(100);

#[go_lib::main]
fn main() {
    // ── Server ────────────────────────────────────────────────────────────────
    let mux = Arc::new(ServeMux::new());

    mux.handle_func("/ping", |w, _r| {
        let _ = w.write(b"pong\n");
    });

    let mut srv = Server::new(ADDR);
    srv.handler      = Some(mux);
    srv.idle_timeout = Some(IDLE);

    go_lib::go!(move || { let _ = srv.listen_and_serve(); });
    go_lib::sleep(Duration::from_millis(30));

    // ── Client ────────────────────────────────────────────────────────────────

    // Phase 1: send a request immediately — should succeed.
    let mut stream = TcpStream::connect(ADDR).expect("connect");
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));

    println!("[client] sending first request ...");
    write!(stream,
        "GET /ping HTTP/1.1\r\nHost: {ADDR}\r\n\r\n"
    ).expect("write");
    stream.flush().expect("flush");

    let mut status = String::new();
    reader.read_line(&mut status).expect("read status");
    println!("[client] got: {}", status.trim());
    assert!(status.starts_with("HTTP/1.1 200"), "expected 200, got: {status:?}");

    // Drain response so the connection enters idle state.
    let mut buf = [0u8; 4096];
    let mut saw_body_start = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 { break; }
        if line == "\r\n" { saw_body_start = true; }
        if saw_body_start {
            let n = reader.read(&mut buf).unwrap_or(0);
            if n == 0 { break; }
            // The chunked response ends with "0\r\n\r\n".
            if buf[..n].windows(5).any(|w| w == b"0\r\n\r\n") { break; }
        }
    }
    println!("[client] response drained — entering idle wait ({IDLE:?}) ...");

    // Phase 2: wait longer than idle_timeout without sending another request.
    go_lib::sleep(IDLE + Duration::from_millis(60));

    // The server should have shut down the read side of the socket.
    // A subsequent read returns 0 bytes (EOF).
    println!("[client] attempting read after idle timeout ...");
    let n = stream.read(&mut buf).unwrap_or(0);
    println!("[client] read returned {n} bytes (expected 0)");
    assert_eq!(n, 0, "expected EOF after idle timeout, got {n} bytes");

    println!("OK — idle timeout closed the keep-alive connection.");
}

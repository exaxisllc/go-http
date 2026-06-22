// SPDX-License-Identifier: Apache-2.0
//! Request body size limiting.
//!
//! Demonstrates `Server::max_body_bytes`, which rejects requests whose
//! declared `Content-Length` exceeds the configured cap with
//! `413 Request Entity Too Large` before the handler ever runs.
//!
//! Run: `cargo run --example body_limit`
//!
//! Or test manually with curl:
//!   # Small body — accepted (200):
//!   curl -v -X POST http://127.0.0.1:8086/upload \
//!        -H "Content-Type: text/plain"            \
//!        --data "tiny"
//!
//!   # Large body — rejected (413):
//!   curl -v -X POST http://127.0.0.1:8086/upload \
//!        -H "Content-Type: text/plain"            \
//!        --data "$(python3 -c "print('x' * 4096)")"

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;

use go_lib::net::TcpStream;
use go_http::{handler::ServeMux, server::Server, status};

const ADDR: &str = "127.0.0.1:8086";
const MAX_BODY: u64 = 256; // bytes

/// Send a raw HTTP/1.1 request and return the status code from the first
/// response line.
fn post_raw(body: &[u8]) -> u16 {
    let mut stream = TcpStream::connect(ADDR).expect("connect");
    write!(
        stream,
        "POST /upload HTTP/1.1\r\n\
         Host: {ADDR}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    )
    .expect("write headers");
    stream.write_all(body).expect("write body");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("read status");
    println!("  → {}", status_line.trim());

    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[go_lib::main]
fn main() {
    // ── Server ────────────────────────────────────────────────────────────────
    let mux = Arc::new(ServeMux::new());

    mux.handle_func("/upload", |w, r| {
        let body = r.body_bytes().unwrap_or_default();
        println!("[server] handler received {} bytes", body.len());
        w.header().set("Content-Type", "text/plain; charset=utf-8");
        let reply = format!("accepted {} bytes\n", body.len());
        let _ = w.write(reply.as_bytes());
    });

    go_lib::go!(move || {
        let mut srv = Server::new(ADDR);
        srv.handler        = Some(mux);
        srv.max_body_bytes = Some(MAX_BODY);
        let _ = srv.listen_and_serve();
    });

    go_lib::sleep(Duration::from_millis(50));

    // ── Client ────────────────────────────────────────────────────────────────

    // 1. Body within the cap → 200 OK.
    let small = b"hello, small body";
    println!("[client] POST {} bytes (cap = {MAX_BODY}):", small.len());
    let code = post_raw(small);
    assert_eq!(code, status::OK, "expected 200, got {code}");
    println!("  ✓ accepted");

    // 2. Body exceeding the cap — the server rejects on the Content-Length
    //    header alone, before reading any body bytes → 413.
    let large = "X".repeat(MAX_BODY as usize + 1);
    println!(
        "[client] POST {} bytes (cap = {MAX_BODY}):",
        large.len()
    );
    let code = post_raw(large.as_bytes());
    assert_eq!(
        code,
        status::REQUEST_ENTITY_TOO_LARGE,
        "expected 413, got {code}"
    );
    println!("  ✓ rejected with 413");

    println!("OK — body size limit enforced correctly.");
}

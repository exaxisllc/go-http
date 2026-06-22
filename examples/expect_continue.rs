// SPDX-License-Identifier: Apache-2.0
//! Expect: 100-continue handshake.
//!
//! Demonstrates the two-phase upload handshake where the client declares
//! `Expect: 100-continue`, waits for the server's provisional response, and
//! only then sends the request body.  This lets the server reject bad requests
//! (wrong content-type, auth failure, body too large, …) before the client
//! wastes bandwidth transmitting the body.
//!
//! Run: `cargo run --example expect_continue`
//!
//! Or test manually with curl (curl sends Expect: 100-continue automatically
//! for bodies larger than 1 KiB, or you can force it with -H):
//!   curl -v -X POST http://127.0.0.1:8085/upload \
//!        -H "Expect: 100-continue"               \
//!        -H "Content-Type: text/plain"            \
//!        --data "hello from curl"

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;

use go_lib::net::TcpStream;
use go_http::{handler::ServeMux, server::Server};

const ADDR: &str = "127.0.0.1:8085";

#[go_lib::main]
fn main() {
    // ── Server ────────────────────────────────────────────────────────────────
    let mux = Arc::new(ServeMux::new());

    mux.handle_func("/upload", |w, r| {
        // Read the full body — only reached after 100 Continue was sent.
        let body = r.body_bytes().unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        println!("[server] received {} bytes: {text:?}", body.len());

        w.header().set("Content-Type", "text/plain; charset=utf-8");
        let reply = format!("Stored {} bytes\n", body.len());
        let _ = w.write(reply.as_bytes());
    });

    go_lib::go!(move || {
        let mut srv = Server::new(ADDR);
        srv.handler = Some(mux);
        let _ = srv.listen_and_serve();
    });

    // Wait for the listener to bind.
    go_lib::sleep(Duration::from_millis(50));

    // ── Client — manual Expect: 100-continue handshake ────────────────────────
    //
    // Phase 1: Send request line + headers (no body yet).
    // Phase 2: Read 100 Continue from server.
    // Phase 3: Send the body.
    // Phase 4: Read the final response.

    let body = b"hello, this is the deferred payload";

    let mut stream = TcpStream::connect(ADDR).expect("connect");
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));

    // Phase 1: headers only.
    println!("[client] sending headers with Expect: 100-continue ...");
    write!(
        stream,
        "POST /upload HTTP/1.1\r\n\
         Host: {ADDR}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Expect: 100-continue\r\n\
         \r\n",
        body.len()
    )
    .expect("write headers");
    stream.flush().expect("flush");

    // Phase 2: read the 100 Continue.
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("read status");
    println!("[client] got provisional response: {}", status_line.trim());
    assert!(
        status_line.starts_with("HTTP/1.1 100"),
        "expected 100 Continue, got: {status_line:?}"
    );
    // Drain the blank line that follows 100 Continue.
    let mut blank = String::new();
    reader.read_line(&mut blank).expect("read blank");

    // Phase 3: now safe to send the body.
    println!("[client] sending body ...");
    stream.write_all(body).expect("write body");
    stream.flush().expect("flush body");

    // Phase 4: read the final response status line and echo body.
    let mut final_status = String::new();
    reader.read_line(&mut final_status).expect("read final status");
    println!("[client] final response: {}", final_status.trim());

    // Consume the rest of the response (headers + body) so the example exits cleanly.
    let mut response_body = String::new();
    let mut in_body = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 { break; }
        if line == "\r\n" { in_body = true; continue; }
        if in_body { response_body.push_str(line.trim()); break; }
    }
    if !response_body.is_empty() {
        // Chunked bodies prepend the hex length; skip it for display.
        let display = response_body.trim_start_matches(|c: char| c.is_ascii_hexdigit());
        println!("[client] server reply: {}", display.trim());
    }

    assert!(final_status.starts_with("HTTP/1.1 200"), "unexpected status: {final_status}");
    println!("OK — Expect: 100-continue handshake complete.");
}

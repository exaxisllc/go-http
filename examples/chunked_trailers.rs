// SPDX-License-Identifier: Apache-2.0
//! Chunked transfer encoding with trailer headers.
//!
//! Demonstrates sending a chunked request body followed by a trailer header
//! (here, an integrity checksum).  The server reads the body via
//! `body_bytes()`, which automatically harvests any trailer headers declared
//! by the client, and echoes the checksum back in the response.
//!
//! HTTP/1.1 trailer headers (RFC 7230 §4.1.2) are transmitted *after* the
//! terminal `0\r\n` chunk, letting the sender append metadata that can only
//! be computed once the body is fully written (e.g., a streaming hash).
//!
//! Run: `cargo run --example chunked_trailers`
//!
//! Or test manually with curl (curl sends chunked + trailers with -H "TE:
//! trailers" but most clients strip trailers; use the raw example below):
//!   printf 'POST /upload HTTP/1.1\r\nHost: 127.0.0.1:8087\r\n\
//!   Transfer-Encoding: chunked\r\nTrailer: X-Checksum\r\n\r\n\
//!   5\r\nhello\r\n6\r\n world\r\n0\r\nX-Checksum: sha256:abc123\r\n\r\n' \
//!   | nc 127.0.0.1 8087

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;

use go_lib::net::TcpStream;
use go_http::{handler::ServeMux, server::Server};

const ADDR: &str = "127.0.0.1:8087";

#[go_lib::main]
fn main() {
    // ── Server ────────────────────────────────────────────────────────────────
    let mux = Arc::new(ServeMux::new());

    mux.handle_func("/upload", |w, r| {
        // body_bytes() reads all chunks and populates r.trailer once the
        // terminal chunk and its trailing headers have been read.
        let body = r.body_bytes().unwrap_or_default();
        let text = String::from_utf8_lossy(&body);

        let checksum = r.trailers()
            .get("X-Checksum")
            .unwrap_or("<none>")
            .to_owned();

        println!("[server] body ({} bytes): {text:?}", body.len());
        println!("[server] X-Checksum trailer: {checksum}");

        // Echo the checksum back so the client can verify it arrived.
        w.header().set("Content-Type", "text/plain; charset=utf-8");
        w.header().set("X-Received-Checksum", &checksum);
        let reply = format!("body={text}\nchecksum={checksum}\n");
        let _ = w.write(reply.as_bytes());
    });

    go_lib::go!(move || {
        let mut srv = Server::new(ADDR);
        srv.handler = Some(mux);
        let _ = srv.listen_and_serve();
    });

    go_lib::sleep(Duration::from_millis(50));

    // ── Client — raw chunked request with a trailer ───────────────────────────
    //
    // Wire format:
    //   Transfer-Encoding: chunked
    //   Trailer: X-Checksum          ← declares which trailers follow
    //
    //   5\r\nhello\r\n               ← first chunk
    //   6\r\n world\r\n              ← second chunk
    //   0\r\n                        ← terminal chunk
    //   X-Checksum: sha256:abc123\r\n← trailer header
    //   \r\n                         ← end of trailers

    let chunk1 = b"hello";
    let chunk2 = b" world";
    let checksum = "sha256:abc123";

    let mut stream = TcpStream::connect(ADDR).expect("connect");
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));

    println!("[client] sending chunked body with X-Checksum trailer ...");

    write!(
        stream,
        "POST /upload HTTP/1.1\r\n\
         Host: {ADDR}\r\n\
         Transfer-Encoding: chunked\r\n\
         Trailer: X-Checksum\r\n\
         \r\n\
         {chunk1_len:X}\r\n{chunk1}\r\n\
         {chunk2_len:X}\r\n{chunk2}\r\n\
         0\r\n\
         X-Checksum: {checksum}\r\n\
         \r\n",
        chunk1     = String::from_utf8_lossy(chunk1),
        chunk1_len = chunk1.len(),
        chunk2     = String::from_utf8_lossy(chunk2),
        chunk2_len = chunk2.len(),
    )
    .expect("write");
    stream.flush().expect("flush");

    // Read response headers looking for X-Received-Checksum.
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("read status");
    println!("[client] response: {}", status_line.trim());

    let mut echoed_checksum = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 || line == "\r\n" { break; }
        if line.to_ascii_lowercase().starts_with("x-received-checksum:") {
            echoed_checksum = line
                .split_once(':')
                .map(|x| x.1)
                .unwrap_or("")
                .trim()
                .to_owned();
        }
    }

    println!("[client] X-Received-Checksum header: {echoed_checksum:?}");
    assert!(
        status_line.starts_with("HTTP/1.1 200"),
        "expected 200, got: {status_line}"
    );
    assert_eq!(
        echoed_checksum, checksum,
        "trailer not round-tripped correctly"
    );

    println!("OK — chunked trailer round-trip complete.");
}

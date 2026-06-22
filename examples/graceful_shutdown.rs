// SPDX-License-Identifier: Apache-2.0
//! Graceful shutdown.
//!
//! Demonstrates `Server::shutdown()`, which stops the accept loop and then
//! waits for all in-flight requests to finish before returning.  Callers can
//! therefore clean up resources (flush logs, close databases, etc.) right
//! after `shutdown()` returns without racing active handlers.
//!
//! Run: `cargo run --example graceful_shutdown`

use std::sync::Arc;
use std::time::Duration;

use go_http::{handler::ServeMux, server::Server};

const ADDR: &str = "127.0.0.1:8088";

#[go_lib::main]
fn main() {
    // ── Server ────────────────────────────────────────────────────────────────
    let mux = Arc::new(ServeMux::new());

    mux.handle_func("/work", |w, _r| {
        println!("[server] handler started — simulating 150 ms of work");
        go_lib::sleep(Duration::from_millis(150));
        println!("[server] handler done");
        w.header().set("Content-Type", "text/plain; charset=utf-8");
        let _ = w.write(b"finished\n");
    });

    let srv = Arc::new({
        let mut s = Server::new(ADDR);
        s.handler = Some(mux);
        s
    });
    let srv2 = Arc::clone(&srv);

    go_lib::go!(move || { let _ = srv2.listen_and_serve(); });
    go_lib::sleep(Duration::from_millis(30));

    // ── Client goroutine — sends a slow request ───────────────────────────────
    go_lib::go!(|| {
        use std::io::Write;
        let mut stream = go_lib::net::TcpStream::connect(ADDR).expect("connect");
        write!(stream,
            "GET /work HTTP/1.1\r\nHost: {ADDR}\r\nConnection: close\r\n\r\n"
        ).expect("write");
        stream.flush().expect("flush");
        println!("[client] request sent");

        // Read the response to confirm it arrived completely.
        use std::io::Read;
        let mut body = Vec::new();
        stream.read_to_end(&mut body).expect("read");
        let s = String::from_utf8_lossy(&body);
        println!("[client] response received: {}",
            if s.contains("finished") { "OK (contains 'finished')" } else { "???" });
    });

    // Let the request start but not finish, then call shutdown.
    go_lib::sleep(Duration::from_millis(50));
    println!("[main] calling shutdown — will block until handler finishes ...");

    let t0 = std::time::Instant::now();
    srv.shutdown();
    let elapsed = t0.elapsed();

    println!("[main] shutdown returned after {:.0?}", elapsed);
    assert!(
        elapsed >= Duration::from_millis(80),
        "shutdown returned too early ({elapsed:.0?}) — handler must have been abandoned"
    );
    println!("OK — graceful shutdown waited for the in-flight handler.");
}

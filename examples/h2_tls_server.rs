// SPDX-License-Identifier: Apache-2.0

//! HTTPS server example with HTTP/2 via ALPN (plus HTTP/1.1 fallback).
//!
//! Uses the self-signed test certificate from `testdata/`:
//!
//! ```sh
//! cargo run --example h2_tls_server
//! curl -vk --http2 https://127.0.0.1:8443/hello       # negotiates h2
//! curl -vk --http1.1 https://127.0.0.1:8443/hello     # still works
//! ```

use std::sync::Arc;

use go_http::handler::ServeMux;
use go_http::server::Server;

#[go_lib::main]
fn main() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/hello", |w, r| {
        w.header().set("Content-Type", "text/plain");
        let _ = w.write(format!("Hello over {} + TLS!\n", r.proto).as_bytes());
    });

    let mut srv = Server::new("127.0.0.1:8443");
    srv.handler = Some(mux);

    let dir = env!("CARGO_MANIFEST_DIR");
    println!("HTTPS server listening on https://127.0.0.1:8443 (ALPN: h2, http/1.1)");
    if let Err(e) = srv.listen_and_serve_tls(
        &format!("{dir}/testdata/cert.pem"),
        &format!("{dir}/testdata/key.pem"),
    ) {
        eprintln!("server error: {e}");
    }
}

// SPDX-License-Identifier: Apache-2.0

//! HTTP/2 cleartext (h2c) server example.
//!
//! ```sh
//! cargo run --example h2_server
//! curl -v --http2-prior-knowledge http://127.0.0.1:8080/hello
//! curl -v --http2-prior-knowledge -d 'some data' http://127.0.0.1:8080/echo
//! curl -v http://127.0.0.1:8080/hello          # same port still speaks HTTP/1.1
//! ```

use std::sync::Arc;

use go_http::handler::ServeMux;
use go_http::server::Server;

#[go_lib::main]
fn main() {
    let mux = Arc::new(ServeMux::new());

    mux.handle_func("/hello", |w, r| {
        w.header().set("Content-Type", "text/plain");
        let _ = w.write(format!("Hello over {}!\n", r.proto).as_bytes());
    });

    mux.handle_func("POST /echo", |w, r| {
        let body = r.body_bytes().unwrap_or_default();
        w.header().set("Content-Type", "application/octet-stream");
        let _ = w.write(&body);
    });

    let mut srv = Server::new("127.0.0.1:8080");
    srv.handler = Some(mux);
    srv.enable_h2c = true;

    println!("h2c server listening on http://127.0.0.1:8080 (HTTP/1.1 + HTTP/2)");
    if let Err(e) = srv.listen_and_serve() {
        eprintln!("server error: {e}");
    }
}

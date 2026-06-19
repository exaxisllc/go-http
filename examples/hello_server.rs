// SPDX-License-Identifier: Apache-2.0
//! Minimal "hello world" HTTP server.
//!
//! Run: `cargo run --example hello_server`
//! Then: `curl http://127.0.0.1:8080/hello`

use go_http::{handler::ServeMux, server::Server};

#[go_lib::main]
fn main() {
    let mux = std::sync::Arc::new(ServeMux::new());

    mux.handle_func("/", |w, _r| {
        w.header().set("Content-Type", "text/plain; charset=utf-8");
        let _ = w.write(b"Hello from go-http!\n");
    });

    mux.handle_func("/hello", |w, r| {
        let name = r.url
            .query_pairs()
            .find(|(k, _)| k == "name")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_else(|| "World".to_owned());

        w.header().set("Content-Type", "text/plain; charset=utf-8");
        let _ = w.write(format!("Hello, {name}!\n").as_bytes());
    });

    println!("Listening on http://127.0.0.1:8080");

    let mut srv = Server::new("127.0.0.1:8080");
    srv.handler = Some(mux);
    if let Err(e) = srv.listen_and_serve() {
        eprintln!("server error: {e}");
    }
}

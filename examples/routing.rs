// SPDX-License-Identifier: Apache-2.0
//! ServeMux routing — multiple routes with exact and prefix patterns.
//!
//! Run: `cargo run --example routing`
//! Try:
//!   curl http://127.0.0.1:8082/
//!   curl http://127.0.0.1:8082/api/users
//!   curl http://127.0.0.1:8082/api/v2/items
//!   curl http://127.0.0.1:8082/health

use go_http::{handler::ServeMux, server::Server, status};

#[go_lib::main]
fn main() {
    let mux = std::sync::Arc::new(ServeMux::new());

    // Exact match — only fires for GET /
    mux.handle_func("/", |w, r| {
        if r.url.path() != "/" {
            w.write_header(status::NOT_FOUND);
            let _ = w.write(b"404 Not Found\n");
            return;
        }
        w.header().set("Content-Type", "text/html; charset=utf-8");
        let _ = w.write(b"<h1>go-http routing example</h1>\
            <ul>\
              <li><a href='/api/users'>/api/users</a></li>\
              <li><a href='/api/v2/items'>/api/v2/items</a></li>\
              <li><a href='/health'>/health</a></li>\
            </ul>");
    });

    // Prefix /api/ — matches everything under /api/
    mux.handle_func("/api/", |w, r| {
        w.header().set("Content-Type", "application/json");
        let path = r.url.path();
        let _ = w.write(
            format!("{{\"route\":\"api-root\",\"path\":\"{path}\"}}\n").as_bytes()
        );
    });

    // Longer prefix /api/v2/ — wins over /api/ for /api/v2/...
    mux.handle_func("/api/v2/", |w, r| {
        w.header().set("Content-Type", "application/json");
        let path = r.url.path();
        let _ = w.write(
            format!("{{\"route\":\"api-v2\",\"path\":\"{path}\"}}\n").as_bytes()
        );
    });

    // Exact match /health
    mux.handle_func("/health", |w, _| {
        w.header().set("Content-Type", "application/json");
        let _ = w.write(b"{\"status\":\"ok\"}\n");
    });

    println!("Routing server on http://127.0.0.1:8082");

    let mut srv = Server::new("127.0.0.1:8082");
    srv.handler = Some(mux);
    if let Err(e) = srv.listen_and_serve() {
        eprintln!("server error: {e}");
    }
}

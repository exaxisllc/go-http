// SPDX-License-Identifier: Apache-2.0
//! ServeMux routing — Go 1.22-style patterns.
//!
//! Demonstrates the full pattern syntax:
//!   - Exact and subtree-prefix (legacy)
//!   - Method prefixes: `GET /path`, `POST /path`
//!   - Single-segment wildcards: `/items/{id}`
//!   - Tail wildcards: `/files/{path...}`
//!   - Host-specific routes: `example.com/`
//!
//! Run: `cargo run --example routing`
//! Try:
//!   curl http://127.0.0.1:8082/
//!   curl http://127.0.0.1:8082/items/42
//!   curl -X POST http://127.0.0.1:8082/items/42
//!   curl http://127.0.0.1:8082/files/docs/readme.md
//!   curl -H "Host: api.local" http://127.0.0.1:8082/
//!   curl -X DELETE http://127.0.0.1:8082/items/42    # → 405

use go_http::{handler::ServeMux, server::Server};

#[go_lib::main]
fn main() {
    let mux = std::sync::Arc::new(ServeMux::new());

    // ── Legacy patterns (backward compatible) ─────────────────────────────────

    mux.handle_func("/health", |w, _r| {
        w.header().set("Content-Type", "application/json");
        let _ = w.write(b"{\"status\":\"ok\"}\n");
    });

    mux.handle_func("/api/v2/", |w, r| {
        w.header().set("Content-Type", "application/json");
        let path = r.url.path();
        let _ = w.write(format!("{{\"version\":2,\"path\":\"{path}\"}}\n").as_bytes());
    });

    // ── Method-specific routes ────────────────────────────────────────────────

    mux.handle_func("GET /items/{id}", |w, r| {
        let id = r.path_value("id");
        w.header().set("Content-Type", "application/json");
        let _ = w.write(format!("{{\"action\":\"get\",\"id\":\"{id}\"}}\n").as_bytes());
    });

    mux.handle_func("POST /items/{id}", |w, r| {
        let id = r.path_value("id");
        w.header().set("Content-Type", "application/json");
        let _ = w.write(format!("{{\"action\":\"post\",\"id\":\"{id}\"}}\n").as_bytes());
    });

    // ── Tail wildcard ─────────────────────────────────────────────────────────

    mux.handle_func("/files/{path...}", |w, r| {
        let path = r.path_value("path");
        w.header().set("Content-Type", "text/plain; charset=utf-8");
        let _ = w.write(format!("serving file: {path}\n").as_bytes());
    });

    // ── Host-specific route ───────────────────────────────────────────────────

    mux.handle_func("api.local/", |w, _r| {
        w.header().set("Content-Type", "application/json");
        let _ = w.write(b"{\"host\":\"api.local\"}\n");
    });

    // ── Catch-all ─────────────────────────────────────────────────────────────

    mux.handle_func("/", |w, r| {
        w.header().set("Content-Type", "text/plain; charset=utf-8");
        let _ = w.write(
            format!("go-http routing example\npath: {}\n", r.url.path()).as_bytes()
        );
    });

    println!("Routing server on http://127.0.0.1:8082");
    println!("  GET /items/42          → method-specific wildcard");
    println!("  POST /items/42         → method-specific wildcard");
    println!("  DELETE /items/42       → 405 Method Not Allowed");
    println!("  GET /files/a/b/c.txt   → tail wildcard");
    println!("  GET / (Host: api.local)→ host-specific handler");

    let mut srv = Server::new("127.0.0.1:8082");
    srv.handler = Some(mux);
    if let Err(e) = srv.listen_and_serve() {
        eprintln!("server error: {e}");
    }
}

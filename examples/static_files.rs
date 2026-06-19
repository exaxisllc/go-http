// SPDX-License-Identifier: Apache-2.0
//! Static file server using `file_server` + `strip_prefix`.
//!
//! Serves files from the `./examples/assets/` directory under the URL
//! prefix `/static/`.
//!
//! Run: `cargo run --example static_files`
//! Then:
//!   curl http://127.0.0.1:8083/static/hello.txt
//!   curl http://127.0.0.1:8083/static/index.html

use std::path::Path;
use std::sync::Arc;

use go_http::{
    handler::{file_server, strip_prefix, ServeMux},
    server::Server,
};

#[go_lib::main]
fn main() {
    // Create a small example assets directory if it doesn't exist.
    let assets = Path::new("examples/assets");
    if !assets.exists() {
        std::fs::create_dir_all(assets).expect("could not create examples/assets");
        std::fs::write(assets.join("hello.txt"), b"Hello from go-http static server!\n")
            .expect("write failed");
        std::fs::write(
            assets.join("index.html"),
            b"<!DOCTYPE html><html><body><h1>Static File Example</h1></body></html>\n",
        )
        .expect("write failed");
        println!("Created examples/assets/ with sample files.");
    }

    let mux = Arc::new(ServeMux::new());

    // /static/* → serve from examples/assets/
    let fs = file_server("examples/assets".to_owned());
    mux.handle(
        "/static/",
        strip_prefix("/static".to_owned(), fs),
    );

    // Root — redirect to /static/index.html
    mux.handle_func("/", |w, r| {
        go_http::util::redirect(w, r, "/static/index.html", 302);
    });

    println!("Static file server on http://127.0.0.1:8083");
    println!("  http://127.0.0.1:8083/static/hello.txt");
    println!("  http://127.0.0.1:8083/static/index.html");

    let mut srv = Server::new("127.0.0.1:8083");
    srv.handler = Some(mux);
    if let Err(e) = srv.listen_and_serve() {
        eprintln!("server error: {e}");
    }
}

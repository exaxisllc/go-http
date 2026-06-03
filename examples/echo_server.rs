// SPDX-License-Identifier: Apache-2.0
//! Echo server — reflects method, path, and request headers as JSON.
//!
//! Run: `cargo run --example echo_server`
//! Then: `curl -H "X-Foo: bar" http://127.0.0.1:8081/anything`

use go_http::{handler::ServeMux, server::Server};

fn main() {
    go_lib::run(|| {
        let mux = std::sync::Arc::new(ServeMux::new());

        mux.handle_func("/", |w, r| {
            // Build a simple JSON object with method, path, and headers.
            let mut json = String::from("{\n");
            json.push_str(&format!("  \"method\": \"{}\",\n", r.method));
            json.push_str(&format!("  \"path\": \"{}\",\n", r.url.path()));

            if let Some(q) = r.url.query() {
                json.push_str(&format!("  \"query\": \"{q}\",\n"));
            }

            json.push_str("  \"headers\": {\n");
            let mut headers: Vec<(&str, &[String])> = r.header.iter().collect();
            headers.sort_by_key(|(k, _)| *k);
            let last = headers.len().saturating_sub(1);
            for (i, (name, values)) in headers.iter().enumerate() {
                let val = values.join(", ");
                let comma = if i < last { "," } else { "" };
                json.push_str(&format!("    \"{name}\": \"{val}\"{comma}\n"));
            }
            json.push_str("  }\n}\n");

            w.header().set("Content-Type", "application/json");
            let _ = w.write(json.as_bytes());
        });

        println!("Echo server on http://127.0.0.1:8081");
        println!("Try: curl -H 'X-Custom: hello' http://127.0.0.1:8081/path?q=1");

        let mut srv = Server::new("127.0.0.1:8081");
        srv.handler = Some(mux);
        if let Err(e) = srv.listen_and_serve() {
            eprintln!("server error: {e}");
        }
    });
}

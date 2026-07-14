// SPDX-License-Identifier: Apache-2.0

//! HTTP/2 client example: fetch a URL and report the negotiated protocol.
//!
//! ```sh
//! cargo run --example h2_client -- https://nghttp2.org/
//! cargo run --example h2_client -- http://127.0.0.1:8080/hello --h2c
//! ```
//!
//! HTTPS URLs negotiate HTTP/2 via ALPN automatically (falling back to
//! HTTP/1.1 when the server does not offer h2).  With `--h2c`, cleartext
//! URLs use HTTP/2 prior knowledge.

use std::sync::Arc;

use go_http::client::{Client, Transport};

#[go_lib::main]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(1).cloned().unwrap_or_else(|| "https://nghttp2.org/".to_owned());
    let h2c = args.iter().any(|a| a == "--h2c");

    let mut transport = Transport::new();
    transport.h2c_prior_knowledge = h2c;
    let mut client = Client::new();
    client.transport = Arc::new(transport);
    client.timeout = Some(std::time::Duration::from_secs(15));

    match client.get(&url) {
        Ok(mut resp) => {
            let body = resp.body_bytes().unwrap_or_default();
            println!("{} {} via {}", resp.status, resp.status_text, resp.proto);
            for name in ["Content-Type", "Server"] {
                if let Some(v) = resp.header.get(name) {
                    println!("{name}: {v}");
                }
            }
            println!("body: {} bytes", body.len());
        }
        Err(e) => {
            eprintln!("request failed: {e}");
            std::process::exit(1);
        }
    }
}

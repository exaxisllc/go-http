// SPDX-License-Identifier: Apache-2.0
//! HTTP client example — GET a URL and print the response.
//!
//! Run: `cargo run --example client_get -- https://httpbin.org/get`
//! Or:  `cargo run --example client_get -- http://example.com/`

use go_http::client::Client;

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://example.com/".to_owned());

    let result = go_lib::run(move || {
        let client = Client::new();
        println!("GET {url}");
        client.get(&url)
    });

    match result {
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
        Ok(mut resp) => {
            println!("HTTP/1.1 {} {}", resp.status, resp.status_text);
            let mut keys: Vec<&str> = resp.header.iter().map(|(k, _)| k).collect();
            keys.sort();
            for key in keys {
                for val in resp.header.values(key) {
                    println!("{key}: {val}");
                }
            }
            println!();

            match resp.body_string() {
                Ok(body) => {
                    // Truncate very long bodies for display.
                    if body.len() > 4096 {
                        println!("{}... [truncated {} bytes]", &body[..4096], body.len() - 4096);
                    } else {
                        print!("{body}");
                    }
                }
                Err(e) => eprintln!("body read error: {e}"),
            }
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
//! Middleware composition — logging + timeout wrapper.
//!
//! Demonstrates how to build a middleware stack using `Handler` trait objects.
//!
//! Run: `cargo run --example middleware`
//! Then:
//!   curl http://127.0.0.1:8084/fast      # completes immediately
//!   curl http://127.0.0.1:8084/slow      # times out after 100 ms → 503

use std::sync::Arc;
use std::time::Duration;

use go_http::{
    handler::{timeout_handler, Handler, ServeMux},
    request::Request,
    response::ResponseWriter,
    server::Server,
};

// ---------------------------------------------------------------------------
// Logging middleware
// ---------------------------------------------------------------------------

/// Wraps `inner` and logs each request's method + path to stdout.
struct LoggingHandler {
    inner: Arc<dyn Handler>,
}

impl Handler for LoggingHandler {
    fn serve_http(&self, w: &mut dyn ResponseWriter, r: &Request) {
        let start = std::time::Instant::now();
        self.inner.serve_http(w, r);
        let elapsed = start.elapsed();
        println!(
            "{} {} → {}ms",
            r.method,
            r.url.path(),
            elapsed.as_millis()
        );
    }
}

fn logging(inner: impl Handler + 'static) -> LoggingHandler {
    LoggingHandler { inner: Arc::new(inner) }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    go_lib::run(|| {
        let mux = Arc::new(ServeMux::new());

        // Fast handler — responds immediately.
        mux.handle_func("/fast", |w, _| {
            w.header().set("Content-Type", "text/plain");
            let _ = w.write(b"Fast response!\n");
        });

        // Slow handler — sleeps longer than the timeout.
        mux.handle_func("/slow", |_w, _| {
            go_lib::sleep(Duration::from_secs(10));
        });

        // Wrap the mux in a timeout handler (100 ms), then in the logger.
        let with_timeout = timeout_handler(mux, Duration::from_millis(100), "request timed out\n");
        let with_logging  = logging(with_timeout);

        println!("Middleware example server on http://127.0.0.1:8084");
        println!("  curl http://127.0.0.1:8084/fast");
        println!("  curl http://127.0.0.1:8084/slow  (expect 503 after 100ms)");

        let mut srv = Server::new("127.0.0.1:8084");
        srv.handler = Some(Arc::new(with_logging));
        if let Err(e) = srv.listen_and_serve() {
            eprintln!("server error: {e}");
        }
    });
}

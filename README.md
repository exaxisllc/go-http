# go-http — HTTP/1.1 Server & Client in Rust

A faithful port of Go's `net/http` library to Rust, built on [go-lib](https://github.com/exaxisllc/go-lib) for goroutine-style concurrency.

**Status:** Production-ready. All 98 tests passing. Full HTTP/1.1 support with TLS.

## Features

### Server
- **Goroutine-per-connection model** — each accepted connection spawns a lightweight goroutine
- **Keep-Alive support** — persistent connections reuse TCP sockets
- **Chunked transfer encoding** — streams large responses without buffering
- **Handler routing** — `ServeMux` with longest-prefix matching
- **Middleware** — `strip_prefix`, `file_server`, `timeout_handler`
- **TLS** — HTTPS via rustls with automatic certificate loading

### Client
- **Connection pooling** — idle TCP connections reused per host
- **Redirect following** — automatic POST→GET on 301/302/303, respecting `max_redirects`
- **Cookie jar** — automatic cookie storage and transmission
- **Timeout support** — per-request deadline via go-lib context

### HTTP
- **RFC 7231 parsing** — request/response line, headers, trailers
- **Content negotiation** — Content-Length vs Transfer-Encoding, 1xx/204/304 handling
- **MIME types** — detect, parse, format media types with RFC 2231 continuations
- **Form encoding** — `application/x-www-form-urlencoded` client support

## Quick Start

### Run the Hello Server

```bash
cargo run --example hello_server
```

Then test it:

```bash
curl http://127.0.0.1:8080/
curl "http://127.0.0.1:8080/hello?name=Alice"
```

### Run a Static File Server

```bash
cargo run --example static_files
```

Serves files from `examples/assets/` under `/static/`:

```bash
curl http://127.0.0.1:8083/static/hello.txt
```

### Use the Client

```bash
cargo run --example client_get -- https://httpbin.org/get
```

Or in your own code:

```rust
use go_http::client::Client;

fn main() {
    let result = go_lib::run(|| {
        let client = Client::new();
        client.get("http://example.com/")
    });

    match result {
        Ok(mut resp) => println!("Status: {}", resp.status),
        Err(e) => eprintln!("Error: {e}"),
    }
}
```

## Examples

All examples run under `go_lib::run()` so goroutines and channels are available:

| Example | Purpose |
|---------|---------|
| `hello_server` | Minimal HTTP server with query parameter handling |
| `echo_server` | Echoes request method, path, headers back as JSON |
| `routing` | ServeMux pattern matching (exact vs prefix) |
| `static_files` | File serving with prefix stripping |
| `client_get` | HTTP client library usage |
| `middleware` | Composable handler middleware (logging + timeout) |

Run with: `cargo run --example <name>`

## API Reference — Go → Rust

| Go | Rust |
|----|----|
| `http.Server` | `go_http::server::Server` |
| `http.ListenAndServe` | `go_http::server::listen_and_serve` |
| `http.ListenAndServeTLS` | `go_http::server::listen_and_serve_tls` |
| `http.Handler` | `go_http::handler::Handler` |
| `http.HandlerFunc` | `go_http::handler::handler_func` |
| `http.ServeMux` | `go_http::handler::ServeMux` |
| `http.StripPrefix` | `go_http::handler::strip_prefix` |
| `http.FileServer` | `go_http::handler::file_server` |
| `http.TimeoutHandler` | `go_http::handler::timeout_handler` |
| `http.Client` | `go_http::client::Client` |
| `http.Transport` | `go_http::client::Transport` |
| `http.RoundTripper` | `go_http::client::RoundTripper` |
| `http.Request` | `go_http::request::Request` |
| `http.Response` | `go_http::response::Response` |
| `http.Cookie` | `go_http::cookie::Cookie` |
| `io.Reader` | `std::io::Read` |
| `io.Writer` | `std::io::Write` |
| `net.Conn.Read` / `Write` | `TcpStream: Read + Write` |
| `net.Listener` | `go_lib::net::TcpListener` |
| `context.Context` | `go_lib::context::Context` |
| `sync.Mutex` | `std::sync::Mutex` |

## Architecture

### Core Modules

- **`server.rs`** — HTTP server, `listen_and_serve`, goroutine-per-connection loop
- **`client.rs`** — HTTP client, `Transport` with connection pooling, redirect handling
- **`handler.rs`** — `Handler` trait, `ServeMux` routing, middleware wrappers
- **`request.rs`** — `Request` value type with URL, headers, body, context
- **`response.rs`** — Response types, `ConnResponseWriter` 
- **`parse/`** — HTTP parsing (request, response, chunked encoding, transfer encoding)
- **`mime/`** — MIME type detection, media-type parsing, multipart, quoted-printable
- **`header.rs`** — Case-insensitive HTTP header map
- **`cookie.rs`** — Cookie struct, `CookieJar` trait, `MemoryCookieJar` implementation
- **`tls.rs`** — TLS config helpers (`server_config`, `default_client_config`)
- **`util.rs`** — Helpers (HTTP-date parsing, content-type detection, `set_cookie`, `redirect`)

### Concurrency Model

Each `Server` spawns an accept goroutine that receives connections. For each new connection, a handler goroutine is spawned:

```rust
go_lib::go!(move || {
    serve_conn(stream, handler, max_header_bytes);
});
```

The handler goroutine runs a keep-alive loop:

1. Parse request headers via `read_request(...)`
2. Attach body reader (chunked or content-length)
3. Dispatch to handler via `handler.serve_http(&mut w, &req)`
4. Write response headers and body via `w`
5. Loop for next request or close on `Connection: close`

All blocking I/O (reads, writes, accepts) is transparent to go-lib's netpoll — the scheduler parks goroutines and resumes them when I/O is ready, without blocking OS threads.

## Building & Testing

### Run All Tests

```bash
cargo test
```

This runs:
- **77 unit tests** across all modules
- **13 integration tests** in `tests/server_client.rs` (GET, POST, redirects, cookies, routing, etc.)
- **8 integration tests** in `tests/middleware.rs` (strip_prefix, file_server, timeout_handler)

### Test Structure

Integration tests spawn a server goroutine and client goroutine in the same `go_lib::run()` call — no separate OS threads. This avoids netpoll corruption from concurrent scheduler instances.

Each integration test file uses a `static NET_LOCK: Mutex<()>` to serialize tests (netpoll is a process-global singleton).

### Build Examples

```bash
cargo build --examples
```

### Check Build (no tests)

```bash
cargo build
```

## TLS Support

### Server (HTTPS)

Load a certificate and key, then call `listen_and_serve_tls`:

```rust
use go_http::server::listen_and_serve_tls;
use go_http::handler::handler_func;

fn main() {
    go_lib::run(|| {
        let _ = listen_and_serve_tls(
            "127.0.0.1:8443",
            "cert.pem",
            "key.pem",
            Some(Arc::new(mux)),
        );
    });
}
```

### Client (HTTPS)

By default, the client trusts the Mozilla root CA bundle:

```rust
let mut client = Client::new();
let resp = client.get("https://example.com/").unwrap();
```

To add a custom CA:

```rust
let tls_cfg = go_http::tls::client_config_with_ca("custom-ca.pem").unwrap();
let mut transport = Transport::new();
transport.tls_config = Some(tls_cfg);

let mut client = Client::new();
client.transport = Arc::new(transport);
```

Certificates are loaded via `rustls-pemfile` and must be in PEM format.

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `go-lib` | 0.5.1 | Goroutines, channels, context, netpoll TCP |
| `go-lib-macros` | 0.5.1 | `go!` and `select!` macros |
| `url` | 2 | URL parsing and manipulation |
| `base64` | 0.22 | Base64 encoding/decoding |
| `rustls` | 0.23 | TLS client & server |
| `rustls-pemfile` | 2 | PEM file parsing |
| `webpki-roots` | 0.26 | Mozilla root CA bundle |

## Design Notes

### Why `impl<H: Handler> Handler for Arc<H>`?

This allows middleware to stack `Arc<ServeMux>` directly without wrapper boilerplate:

```rust
let mux = Arc::new(ServeMux::new());
let with_timeout = timeout_handler(mux, Duration::from_secs(5), "timeout");
```

### Why `impl Read + Write for &TcpStream` (in addition to `&mut`)?

`TcpStream::read` and `TcpStream::write` only touch goroutine state, not the struct itself. This is safe with `&` because all blocking is managed by go-lib's netpoll — there are no interior mutability hazards.

### Why `try_clone()` instead of `Rc<RefCell<TcpStream>>`?

`TcpStream::try_clone()` uses `dup(2)` (Unix) or `DuplicateHandle` (Windows) to create independent file descriptors pointing to the same kernel socket. This avoids shared ownership and borrow checker complexity.

In `serve_conn`, the request body and response writer operate on independent clones — true parallelism without contention.

### Separate Integration Test Binaries

Each `.rs` file in `tests/` compiles to its own binary with its own OS process, scheduler, and netpoll instance. This isolation prevents goroutine pointer collisions when tests run in parallel.

## Contributing

- Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Add tests for new features (unit tests in `src/`, integration tests in `tests/`)
- Run `cargo test` before submitting
- Port behavior from Go's `net/http` when possible for API familiarity

## License

Licensed under the Apache License, Version 2.0 or the MIT License, at your option.

## Go Reference

For behavior clarification, consult the [Go net/http documentation](https://golang.org/pkg/net/http/).

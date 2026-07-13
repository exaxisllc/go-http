// SPDX-License-Identifier: Apache-2.0

//! HTTP/2 over TLS (ALPN) integration tests.
//!
//! Uses the self-signed localhost certificate in `testdata/`; the client
//! trusts it via `client_config_with_ca`.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use go_http::client::{Client, Transport};
use go_http::handler::ServeMux;
use go_http::parse::transfer::Body;
use go_http::server::Server;

static PORT: AtomicU16 = AtomicU16::new(19500);

fn next_port() -> u16 {
    PORT.fetch_add(1, Ordering::SeqCst)
}

fn testdata(file: &str) -> String {
    format!("{}/testdata/{file}", env!("CARGO_MANIFEST_DIR"))
}

/// Start a TLS server (ALPN h2 + http/1.1) on its own goroutine and wait
/// until it accepts connections (a fixed sleep races on slow CI runners).
fn start_tls_server(mux: Arc<ServeMux>) -> String {
    let port = next_port();
    let addr = format!("127.0.0.1:{port}");
    let addr2 = addr.clone();
    go_lib::go!(move || {
        let mut srv = Server::new(addr2);
        srv.handler = Some(mux);
        let _ = srv.listen_and_serve_tls(&testdata("cert.pem"), &testdata("key.pem"));
    });
    wait_until_ready(&addr);
    addr
}

/// Probe-connect until the listener is up (the probe connection is dropped
/// immediately; the server's failed handshake on it is harmless).
fn wait_until_ready(addr: &str) {
    for _ in 0..250 {
        if go_lib::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        go_lib::sleep(Duration::from_millis(20));
    }
    panic!("server at {addr} did not become ready");
}

/// A client that trusts the testdata CA (and therefore offers h2 via ALPN,
/// since the CA config carries no explicit ALPN of its own).
fn tls_client() -> Client {
    let mut transport = Transport::new();
    transport.tls_config =
        Some(go_http::tls::client_config_with_ca(&testdata("ca.pem")).unwrap());
    let mut client = Client::new();
    client.transport = Arc::new(transport);
    client
}

#[test]
#[go_lib::main]
fn alpn_negotiates_h2() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/hello", |w, r| {
        assert_eq!(r.proto, "HTTP/2.0", "server side must see HTTP/2");
        w.header().set("Content-Type", "text/plain");
        let _ = w.write(b"hello over TLS");
    });
    let addr = start_tls_server(mux);

    let client = tls_client();
    let mut resp = client.get(&format!("https://localhost:{}/hello", addr.split(':').nth(1).unwrap())).unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.proto, "HTTP/2.0", "client must negotiate h2 via ALPN");
    assert_eq!(resp.body_string().unwrap(), "hello over TLS");
}

#[test]
#[go_lib::main]
fn alpn_h2_post_roundtrip() {
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/echo", |w, r| {
        let body = r.body_bytes().unwrap();
        let _ = w.write(&body);
    });
    let addr = start_tls_server(mux);
    let port = addr.split(':').nth(1).unwrap().to_owned();

    let client = tls_client();
    let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 241) as u8).collect();
    let body = Body::Unbounded(Box::new(std::io::Cursor::new(payload.clone())));
    let mut resp = client
        .post(&format!("https://localhost:{port}/echo"), "application/octet-stream", body)
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.proto, "HTTP/2.0");
    assert_eq!(resp.body_bytes().unwrap(), payload);
}

#[test]
#[go_lib::main]
fn h2_tls_connection_is_pooled() {
    let mux = Arc::new(ServeMux::new());
    let conns = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let conns2 = Arc::clone(&conns);
    mux.handle_func("/r", move |w, r| {
        // remote_addr is stable per TCP connection.
        conns2.lock().unwrap().insert(r.remote_addr.clone());
        let _ = w.write(b"ok");
    });
    let addr = start_tls_server(mux);
    let port = addr.split(':').nth(1).unwrap().to_owned();

    let client = tls_client();
    for _ in 0..5 {
        let mut resp = client.get(&format!("https://localhost:{port}/r")).unwrap();
        assert_eq!(resp.status, 200);
        let _ = resp.body_bytes();
    }
    assert_eq!(
        conns.lock().unwrap().len(),
        1,
        "all five requests must share one multiplexed TLS connection"
    );
}

#[test]
#[go_lib::main]
fn http1_only_client_config_still_works() {
    // A client whose TLS config explicitly pins ALPN to http/1.1 must get
    // an HTTP/1.1 response from the dual-protocol server.
    let mux = Arc::new(ServeMux::new());
    mux.handle_func("/proto", |w, r| {
        let _ = w.write(r.proto.as_bytes());
    });
    let addr = start_tls_server(mux);
    let port = addr.split(':').nth(1).unwrap().to_owned();

    let base = go_http::tls::client_config_with_ca(&testdata("ca.pem")).unwrap();
    let mut cfg = (*base).clone();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];

    let mut transport = Transport::new();
    transport.tls_config = Some(Arc::new(cfg));
    let mut client = Client::new();
    client.transport = Arc::new(transport);

    let mut resp = client.get(&format!("https://localhost:{port}/proto")).unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.proto, "HTTP/1.1");
    assert_eq!(resp.body_string().unwrap(), "HTTP/1.1");
}

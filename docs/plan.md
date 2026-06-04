<!-- SPDX-License-Identifier: Apache-2.0 -->

# Plan: `go-http` — Port of Go `net/http` to Rust

## Guiding principles

- Mirror Go's public API as closely as Rust's type system allows (trait for `Handler`/`ResponseWriter`/`RoundTripper`, struct for `Server`/`Client`/`Request`/`Response`, etc.)
- Use `go-lib` primitives everywhere Go would use goroutines, channels, `sync.WaitGroup`, `context`, etc.
- Use `go-lib::net` for raw TCP; build HTTP framing on top of it.
- No `tokio` or `async`/`await` — concurrency is entirely through `go!` goroutines and channels.

---

## Phase 0 — Scaffolding & dependencies

**Files touched:** `Cargo.toml`, `src/lib.rs`

1. Add dependencies:
   - `go-lib = "0.5.0"` — goroutines, channels, WaitGroup, Mutex, context, net
   - `go-lib-macros = "0.5.0"` — `#[run]` attribute
   - `url` — URL parsing (WHATWG standard; equal or better than Go's `net/url`)
   - `base64` — auth helpers (equivalent to Go's `encoding/base64`)

   The following are **ported from Go's stdlib** rather than pulled from crates (see Phase 1h and Phase 1i):
   - Go's `net/http` internal parser → `src/parse/`
   - Go's `mime` + `mime/multipart` packages → `src/mime/`

2. Establish top-level module layout in `src/lib.rs`:

```
src/
  lib.rs              ← re-exports; sets up #[run] scheduler entry point
  header.rs           ← Header type
  method.rs           ← HTTP method constants
  status.rs           ← HTTP status code constants
  request.rs          ← Request struct
  response.rs         ← Response struct + ResponseWriter trait
  handler.rs          ← Handler trait, HandlerFunc, ServeMux
  server.rs           ← Server struct, ListenAndServe
  client.rs           ← Client struct, Transport, RoundTripper trait
  cookie.rs           ← Cookie / CookieJar
  context.rs          ← thin re-export wrapper of go-lib context
  error.rs            ← HttpError enum
  util.rs             ← DetectContentType, ParseTime, SetCookie helpers
  parse/
    mod.rs            ← public re-exports
    request.rs        ← port of Go net/http request parser
    response.rs       ← port of Go net/http response parser
    chunk.rs          ← port of Go chunked transfer encoding reader/writer
    transfer.rs       ← port of Go transferBodyReader / body framing logic
  mime/
    mod.rs            ← port of Go mime package (ParseMediaType, FormatMediaType, etc.)
    multipart.rs      ← port of Go mime/multipart (Reader, Writer, Form, FileHeader)
    quotedprintable.rs← port of Go mime/quotedprintable
```

---

## Phase 1 — Foundational types (no I/O)

**Goal:** All value types compile with full API; no networking yet.

### 1a. `header.rs` — `Header`
- `Header(HashMap<String, Vec<String>>)` newtype
- Methods: `get`, `set`, `add`, `del`, `values`, `write` (serializes to `impl Write`)
- Matches Go's `http.Header`

### 1b. `method.rs` — method constants
- `pub const GET: &str`, `POST`, `PUT`, `DELETE`, `PATCH`, `HEAD`, `OPTIONS`, `CONNECT`, `TRACE`

### 1c. `status.rs` — status code constants + `status_text()`
- `pub const OK: u16 = 200`, etc. for all standard codes
- `pub fn status_text(code: u16) -> &'static str`

### 1d. `error.rs` — `HttpError`
```rust
pub enum HttpError {
    Io(std::io::Error),
    Parse(ParseError),           // from src/parse/
    InvalidUrl(String),
    Timeout,
    TooManyRedirects,
    BodyRead,
    Mime(String),
}
```
Implement `std::error::Error`, `Display`, `From<io::Error>`.

### 1e. `request.rs` — `Request`
```rust
pub struct Request {
    pub method: String,
    pub url: Url,
    pub proto: String,           // "HTTP/1.1"
    pub header: Header,
    pub body: Option<Body>,      // Body = Box<dyn Read + Send>
    pub content_length: i64,
    pub transfer_encoding: Vec<String>,
    pub host: String,
    pub trailer: Header,
    pub remote_addr: String,
    // context handle from go-lib
    ctx: go_lib::context::Context,
}
```
Methods: `new_request(method, url, body)`, `new_request_with_context(...)`, `with_context`, `context`, `cookie`, `cookies`, `form_value`, `parse_form`, `parse_multipart_form`, `basic_auth`, `user_agent`, `referer`, `write` (serializes for wire).

### 1f. `response.rs` — `Response` + `ResponseWriter`
```rust
pub struct Response {
    pub status: u16,
    pub status_text: String,
    pub proto: String,
    pub header: Header,
    pub body: Option<Body>,
    pub content_length: i64,
    pub transfer_encoding: Vec<String>,
    pub trailer: Header,
    pub request: Option<Arc<Request>>,
}

pub trait ResponseWriter: Send {
    fn header(&mut self) -> &mut Header;
    fn write(&mut self, buf: &[u8]) -> Result<usize, HttpError>;
    fn write_header(&mut self, status_code: u16);
}
```
`response_writer_from_stream` — creates a concrete `ConnResponseWriter` that wraps a `go-lib` TCP stream.

### 1h. `parse/` — Port of Go's `net/http` internal parser

Go's HTTP parser lives across several internal files (`request.go`, `response.go`, `transfer.go`, `chunk.go`). This is a direct port of that logic.

**`parse/request.rs`**
- `read_request(r: &mut impl Read) -> Result<Request, ParseError>`
- Reads request line (method, request-URI, proto) and headers
- Validates method token characters (Go: `validMethod`)
- Enforces `max_header_bytes` limit
- Calls into `transfer.rs` to attach a body reader

**`parse/response.rs`**
- `read_response(r: &mut impl Read, req: &Request) -> Result<Response, ParseError>`
- Reads status line and headers
- HEAD / 1xx / 204 / 304 get no body per RFC 7230

**`parse/transfer.rs`**
- `read_transfer(msg: &mut Message, r: &mut impl Read) -> Result<(), ParseError>`
  - Port of Go's `readTransfer`: resolves body presence from Content-Length vs Transfer-Encoding
  - Wraps reader in `ChunkedReader` or `LimitedReader` as appropriate
  - Handles trailer header population after chunked body consumed
- `write_transfer(msg: &Message, w: &mut impl Write) -> Result<(), HttpError>`
  - Port of Go's `writeTransfer`: chooses chunked vs content-length framing on write path

**`parse/chunk.rs`**
- `ChunkedReader` — port of Go's `internal/chunked` reader: reads `chunk-size CRLF data CRLF` framing, populates trailers on EOF
- `ChunkedWriter` — wraps a `Write`, emits chunked framing, flushes terminal `0\r\n\r\n`

**`parse/mod.rs`**
```rust
pub enum ParseError {
    BadRequestLine,
    BadStatusLine,
    HeaderTooLarge,
    InvalidChunkSize,
    InvalidContentLength,
    UnexpectedEof,
    Other(String),
}
```

### 1i. `mime/` — Port of Go's `mime` and `mime/multipart` packages

**`mime/mod.rs`** — port of Go's `mime` package
- `parse_media_type(s: &str) -> Result<(String, HashMap<String, String>), MimeError>`
  - Port of Go's `ParseMediaType`: handles quoted-string params, RFC 2231 continuations, charset normalization
- `format_media_type(t: &str, params: &HashMap<String, String>) -> String`
  - Port of Go's `FormatMediaType`: re-serializes with proper quoting
- `extension_by_type(typ: &str) -> Option<String>` — looks up file extension for MIME type
- `type_by_extension(ext: &str) -> Option<String>` — looks up MIME type for file extension
- Built-in type map seeded from Go's `mime/type.go` built-in table

**`mime/multipart.rs`** — port of Go's `mime/multipart`
```rust
pub struct Reader { ... }  // wraps impl Read + boundary
pub struct Writer { ... }  // wraps impl Write + boundary

pub struct Form {
    pub value: HashMap<String, Vec<String>>,
    pub file: HashMap<String, Vec<FileHeader>>,
}
pub struct FileHeader {
    pub filename: String,
    pub header: Header,
    pub size: i64,
    // content held in memory or temp file depending on size
    content: FileHeaderContent,
}
```
- `Reader::next_part() -> Result<Option<Part>, MimeError>` — iterates parts
- `Reader::read_form(max_memory: i64) -> Result<Form, MimeError>` — port of Go's `ReadForm`
- `Writer::create_part(header: Header) -> Result<impl Write, MimeError>`
- `Writer::create_form_field(fieldname) -> Result<impl Write, MimeError>`
- `Writer::create_form_file(fieldname, filename) -> Result<impl Write, MimeError>`
- `Writer::close() -> Result<(), MimeError>` — writes closing boundary

**`mime/quotedprintable.rs`** — port of Go's `mime/quotedprintable`
- `QpReader` — wraps `impl Read`, decodes QP encoding
- `QpWriter` — wraps `impl Write`, encodes to QP

### 1g. `handler.rs` — `Handler`, `HandlerFunc`, `ServeMux`
```rust
pub trait Handler: Send + Sync {
    fn serve_http(&self, w: &mut dyn ResponseWriter, r: &Request);
}

pub struct HandlerFunc(pub Box<dyn Fn(&mut dyn ResponseWriter, &Request) + Send + Sync>);
impl Handler for HandlerFunc { ... }
```
`ServeMux`:
- `new()`, `handle(pattern, handler)`, `handle_func(pattern, f)`
- Pattern matching: exact match first, then longest prefix match (Go semantics)
- `serve_http` dispatches to matched handler (or `not_found_handler`)

---

## Phase 2 — Server

**Goal:** `ListenAndServe(addr, handler)` works with goroutine-per-connection model.

### `server.rs`
```rust
pub struct Server {
    pub addr: String,
    pub handler: Option<Arc<dyn Handler>>,
    pub read_timeout: Option<Duration>,
    pub write_timeout: Option<Duration>,
    pub idle_timeout: Option<Duration>,
    pub max_header_bytes: usize,
    // internal shutdown channel
    shutdown_tx: Option<go_lib::chan::Sender<()>>,
}
```

`Server::listen_and_serve(&self) -> Result<(), HttpError>`:
1. Open TCP listener via `go_lib::net::TcpListener::bind(addr)`
2. Accept loop runs in a goroutine (`go! { loop { let conn = listener.accept()... } }`)
3. Each accepted connection spawns a goroutine: `go! { serve_conn(conn, handler.clone()) }`
4. `serve_conn` reads bytes, parses with `crate::parse`, builds `Request`, dispatches to handler, writes response back

`Server::shutdown(&self)` — sends on `shutdown_tx`; accept loop `select!`s on shutdown channel.

Free functions (use `DefaultServeMux`):
- `pub fn listen_and_serve(addr, handler) -> Result<(), HttpError>`
- `pub fn listen_and_serve_tls(addr, cert, key, handler)` — Phase 5
- `pub fn handle(pattern, handler)`
- `pub fn handle_func(pattern, f)`

---

## Phase 3 — Client

**Goal:** `Client::get(url)`, `Client::post(url, body)`, etc. work.

### `client.rs`
```rust
pub trait RoundTripper: Send + Sync {
    fn round_trip(&self, req: Request) -> Result<Response, HttpError>;
}

pub struct Transport {
    pub max_idle_conns: usize,
    pub idle_conn_timeout: Option<Duration>,
    pub dial_timeout: Option<Duration>,
    // connection pool: HashMap<host, VecDeque<TcpStream>>
    pool: Mutex<HashMap<String, VecDeque<go_lib::net::TcpStream>>>,
}
impl RoundTripper for Transport { ... }

pub struct Client {
    pub transport: Arc<dyn RoundTripper>,
    pub timeout: Option<Duration>,
    pub max_redirects: usize,
    pub jar: Option<Arc<dyn CookieJar>>,
}
```

`Client` methods: `get`, `post`, `post_form`, `head`, `do_request`

`do_request` logic:
1. Serialize `Request` to wire format
2. Acquire connection from pool (or dial new via `go_lib::net::TcpStream::connect`)
3. Write bytes; read response via `crate::parse::read_response`
4. Handle redirects (up to `max_redirects`), updating cookies via jar
5. Timeout via `go_lib::context` deadline on the request

Free functions: `pub fn get(url)`, `pub fn post(url, content_type, body)`, `pub fn post_form(url, data)`, `pub fn head(url)`

---

## Phase 4 — Middleware, helpers, and utilities

**Goal:** Match remaining Go `net/http` surface.

### `util.rs`
- `detect_content_type(data: &[u8]) -> String` — inspect first 512 bytes, return MIME
- `parse_time(s: &str) -> Result<SystemTime>` — HTTP-date parsing (RFC 1123 / RFC 850 / asctime)
- `set_cookie(w: &mut dyn ResponseWriter, cookie: &Cookie)`
- `error(w, error: &str, code: u16)` — writes plain-text error response
- `not_found(w, r)` — 404 helper
- `redirect(w, r, url, code)` — writes Location header + redirect body
- `canonical_header_key(s: &str) -> String` — "content-type" → "Content-Type"

### Handler wrappers in `handler.rs`
- `file_server(root: &str) -> impl Handler` — serves files from directory; uses `go_lib` goroutines for reads
- `strip_prefix(prefix, handler) -> impl Handler`
- `timeout_handler(handler, timeout, msg) -> impl Handler` — uses `go_lib` context + goroutine with cancel

### `cookie.rs`
```rust
pub struct Cookie {
    pub name: String, pub value: String,
    pub path: String, pub domain: String,
    pub expires: Option<SystemTime>,
    pub max_age: i32,
    pub secure: bool, pub http_only: bool,
    pub same_site: SameSite,
}
pub trait CookieJar: Send + Sync {
    fn cookies(&self, url: &Url) -> Vec<Cookie>;
    fn set_cookies(&self, url: &Url, cookies: &[Cookie]);
}
pub struct MemoryCookieJar { ... }
```

---

## Phase 5 — TLS (stretch goal)

- Add `rustls` dependency
- `Server::listen_and_serve_tls(addr, cert_file, key_file, handler)` — wraps accepted TCP streams with `rustls::ServerSession`
- `Transport` gains `tls_config: Option<Arc<rustls::ClientConfig>>` for HTTPS

---

## Concurrency model summary

| Go concept | go-http mapping |
|---|---|
| `goroutine` | `go!` macro |
| `channel` | `go_lib::chan::Chan` |
| `sync.WaitGroup` | `go_lib::sync::WaitGroup` |
| `sync.Mutex` | `go_lib::sync::Mutex` |
| `context.Context` | `go_lib::context::Context` |
| TCP `net.Conn` | `go_lib::net::TcpStream` |
| `net.Listener` | `go_lib::net::TcpListener` |
| `runtime.GOMAXPROCS` | `go_lib::runtime::set_gomaxprocs` |
| `time.Sleep` | `go_lib::sleep` |

---

## Implementation order

| # | Phase | Deliverable |
|---|---|---|
| 1 | Phase 0 | Cargo.toml + module skeleton compiles |
| 2 | Phase 1a–1d | Header, method/status constants, error type |
| 3 | Phase 1h | `parse/` — request/response/chunk/transfer ports (unit tests against raw HTTP bytes) |
| 4 | Phase 1i | `mime/` — ParseMediaType, multipart reader/writer, quotedprintable (unit tests) |
| 5 | Phase 1e–1f | Request + Response value types (now backed by `parse/` and `mime/`) |
| 6 | Phase 1g | Handler trait + ServeMux (unit-testable) |
| 7 | Phase 2 | Server + accept loop (integration test: curl localhost) |
| 8 | Phase 3 | Client + Transport + redirect handling |
| 9 | Phase 4 | Helpers, file server, timeout handler, cookies |
| 10 | Phase 5 | TLS (optional) |

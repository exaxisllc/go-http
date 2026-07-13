// SPDX-License-Identifier: Apache-2.0

/// Split read/write I/O for an HTTP/2 connection, over plain TCP or TLS.
///
/// Plain connections split via `TcpStream::try_clone()` (dup'd fds).  TLS
/// connections share one `rustls::Connection` behind a `std::sync::Mutex`;
/// each half owns its own fd clone for the socket I/O.
///
/// ## Lock discipline
///
/// The rustls mutex guards only in-memory TLS record processing.  **All
/// socket reads and writes happen with the lock released**, so a goroutine
/// parked in the netpoll never holds the lock — the std `Mutex` is safe
/// under go-lib's scheduler as long as no park happens while it is held.
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use go_lib::net::TcpStream;

/// The read half of an h2 connection.
pub struct H2ReadHalf(ReadInner);

enum ReadInner {
    Plain(TcpStream),
    Tls {
        conn: Arc<Mutex<rustls::Connection>>,
        sock: TcpStream,
        /// Ciphertext read from the socket but not yet accepted by
        /// `read_tls` (rustls bounds its internal buffers; a full buffer
        /// just means "decrypt and drain before feeding more").
        pending: Vec<u8>,
    },
}

/// The write half of an h2 connection.
pub struct H2WriteHalf(WriteInner);

enum WriteInner {
    Plain(TcpStream),
    Tls {
        conn: Arc<Mutex<rustls::Connection>>,
        sock: TcpStream,
    },
}

/// Split a plain TCP stream into independent read and write halves.
/// Also returns a [`RawFdHandle`] for unblocking a parked reader.
pub fn split_plain(stream: TcpStream) -> io::Result<(H2ReadHalf, H2WriteHalf, RawFdHandle)> {
    let raw = RawFdHandle::of(&stream);
    let write = stream.try_clone()?;
    Ok((
        H2ReadHalf(ReadInner::Plain(stream)),
        H2WriteHalf(WriteInner::Plain(write)),
        raw,
    ))
}

/// Split a TLS session into read and write halves.
///
/// **Precondition:** the handshake must be complete
/// (`!conn.is_handshaking()`), so reads and writes are independent record
/// streams.
pub fn split_tls(
    conn: rustls::Connection,
    sock: TcpStream,
) -> io::Result<(H2ReadHalf, H2WriteHalf, RawFdHandle)> {
    debug_assert!(!conn.is_handshaking(), "split_tls requires a completed handshake");
    let raw = RawFdHandle::of(&sock);
    let sock_w = sock.try_clone()?;
    let conn = Arc::new(Mutex::new(conn));
    Ok((
        H2ReadHalf(ReadInner::Tls { conn: Arc::clone(&conn), sock, pending: Vec::new() }),
        H2WriteHalf(WriteInner::Tls { conn, sock: sock_w }),
        raw,
    ))
}

impl Read for H2ReadHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.0 {
            ReadInner::Plain(s) => s.read(buf),
            ReadInner::Tls { conn, sock, pending } => {
                // Guards against a (should-be-impossible) no-progress spin:
                // consecutive iterations that neither return plaintext nor
                // consume pending ciphertext.
                let mut stalled = 0u32;
                loop {
                    // All in-memory record work happens under the lock.
                    {
                        let mut c = conn.lock().unwrap();
                        match c.reader().read(buf) {
                            Ok(n) => return Ok(n), // includes clean close (0)
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(e) => return Err(e),
                        }
                        // Plaintext is empty: decrypt whatever is already
                        // deframed (this also frees deframer space).
                        c.process_new_packets().map_err(io::Error::other)?;
                        match c.reader().read(buf) {
                            Ok(n) => return Ok(n),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(e) => return Err(e),
                        }
                        // Still nothing: feed pending ciphertext.  A full
                        // deframer surfaces as InvalidData("message buffer
                        // full") — not fatal, it drains on the next
                        // process_new_packets pass.
                        if !pending.is_empty() {
                            let mut slice = pending.as_slice();
                            match c.read_tls(&mut slice) {
                                Ok(_) => {
                                    let consumed = pending.len() - slice.len();
                                    pending.drain(..consumed);
                                    if consumed > 0 {
                                        stalled = 0;
                                        c.process_new_packets().map_err(io::Error::other)?;
                                        continue;
                                    }
                                }
                                Err(e) if e.kind() == io::ErrorKind::InvalidData => {}
                                Err(e) => return Err(e),
                            }
                            stalled += 1;
                            if stalled > 2 {
                                return Err(io::Error::other(
                                    "TLS record processing made no progress",
                                ));
                            }
                            continue;
                        }
                    }
                    // Nothing buffered anywhere: read ciphertext from the
                    // socket with the lock RELEASED (parks in the netpoll).
                    let mut tmp = [0u8; 16 * 1024];
                    let n = sock.read(&mut tmp)?;
                    if n == 0 {
                        // EOF without close_notify: no more plaintext can
                        // arrive.
                        return Ok(0);
                    }
                    pending.extend_from_slice(&tmp[..n]);
                }
            }
        }
    }
}

impl Write for H2WriteHalf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match &mut self.0 {
            WriteInner::Plain(s) => s.write_all(buf),
            WriteInner::Tls { conn, sock } => {
                // rustls buffers plaintext up to an internal limit (64 KiB by
                // default); a single write_all of a larger buffer would stall
                // with WriteZero.  Feed it incrementally, draining the
                // ciphertext after every accepted chunk.
                let mut off = 0;
                while off < buf.len() {
                    // Encrypt under the lock into a local buffer …
                    let mut out = Vec::with_capacity(16 * 1024 + 1024);
                    {
                        let mut c = conn.lock().unwrap();
                        let n = c.writer().write(&buf[off..])?;
                        if n == 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "TLS writer accepted no bytes",
                            ));
                        }
                        off += n;
                        while c.wants_write() {
                            c.write_tls(&mut out)?;
                        }
                    }
                    // … then hit the socket with the lock released.
                    sock.write_all(&out)?;
                }
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RawFdHandle — unblock a parked reader by shutting the socket down
// ---------------------------------------------------------------------------

/// A non-owning view of the connection's socket, used to interrupt a
/// goroutine parked in `read` (go-lib's `TcpStream` has no `shutdown`).
///
/// Same technique as the idle-timeout watchdog in `server.rs`: rebuild a
/// `std::net::TcpStream` from the raw fd, call `shutdown`, then `forget` it
/// so the fd is not double-closed.  The handle must not be used after the
/// owning stream is dropped.
#[derive(Clone, Copy)]
pub struct RawFdHandle {
    #[cfg(unix)]
    fd: std::os::unix::io::RawFd,
    #[cfg(windows)]
    socket: u64,
}

impl RawFdHandle {
    pub fn of(stream: &TcpStream) -> RawFdHandle {
        RawFdHandle {
            #[cfg(unix)]
            fd: stream.as_raw_fd(),
            #[cfg(windows)]
            socket: stream.as_raw_socket() as u64,
        }
    }

    /// Shut down the read side, causing a parked `read` to return.
    pub fn shutdown_read(&self) {
        self.shutdown(std::net::Shutdown::Read);
    }

    /// Shut down both directions.
    pub fn shutdown_both(&self) {
        self.shutdown(std::net::Shutdown::Both);
    }

    fn shutdown(&self, how: std::net::Shutdown) {
        #[cfg(unix)]
        {
            use std::os::unix::io::FromRawFd;
            // SAFETY: the fd is valid while the owning stream is alive;
            // forget prevents double-close since from_raw_fd takes ownership.
            let s = unsafe { std::net::TcpStream::from_raw_fd(self.fd) };
            let _ = s.shutdown(how);
            std::mem::forget(s);
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::FromRawSocket;
            let s = unsafe {
                std::net::TcpStream::from_raw_socket(self.socket as std::os::windows::io::RawSocket)
            };
            let _ = s.shutdown(how);
            std::mem::forget(s);
        }
    }
}

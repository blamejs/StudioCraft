//! The connection under the chunk stream: a TCP socket, or TLS over one.

use std::io::{self, IoSlice, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

/// A TCP socket with an optional deadline. While the deadline is set, each read and write on the
/// socket waits at most the time left before it, and fails with [`io::ErrorKind::TimedOut`] once
/// it has passed, so a peer that sends or reads a byte at a time cannot stretch a step past it.
pub struct Socket {
    stream: TcpStream,
    deadline: Option<Instant>,
}

impl Socket {
    pub fn new(stream: TcpStream, deadline: Option<Instant>) -> Socket {
        Socket { stream, deadline }
    }

    /// The time left before the deadline, or `None` without one.
    fn left(&self) -> io::Result<Option<Duration>> {
        let Some(deadline) = self.deadline else { return Ok(None) };
        let left = deadline.saturating_duration_since(now());
        if left.is_zero() {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        Ok(Some(left))
    }
}

/// The time the deadline is checked against.
#[cfg(not(test))]
fn now() -> Instant {
    Instant::now()
}

#[cfg(test)]
thread_local! {
    static TEST_CLOCK: std::cell::RefCell<Option<Box<dyn FnMut() -> Instant>>> = const { std::cell::RefCell::new(None) };
}

/// The time the deadline is checked against: the calling thread's test clock when one is set.
#[cfg(test)]
fn now() -> Instant {
    TEST_CLOCK.with_borrow_mut(|clock| clock.as_mut().map(|f| f())).unwrap_or_else(Instant::now)
}

/// Sets or clears the clock that sockets on the calling thread check their deadline against.
#[cfg(test)]
pub fn set_test_clock(clock: Option<Box<dyn FnMut() -> Instant>>) {
    TEST_CLOCK.set(clock);
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(left) = self.left()? {
            self.stream.set_read_timeout(Some(left))?;
        }
        self.stream.read(buf)
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(left) = self.left()? {
            self.stream.set_write_timeout(Some(left))?;
        }
        self.stream.write(buf)
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        if let Some(left) = self.left()? {
            self.stream.set_write_timeout(Some(left))?;
        }
        self.stream.write_vectored(bufs)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// A TCP socket, or a TLS session over one. Writes to a TLS session go into rustls's buffer and
/// reach the socket on [`Transport::flush`], so one message is one flush.
pub enum Transport {
    /// Plain RTMP.
    Tcp(Socket),
    /// RTMPS.
    #[cfg(feature = "rtmps")]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, Socket>>),
}

impl Transport {
    /// The socket.
    pub fn socket(&self) -> &TcpStream {
        match self {
            Transport::Tcp(s) => &s.stream,
            #[cfg(feature = "rtmps")]
            Transport::Tls(t) => &t.sock.stream,
        }
    }

    /// Sets or clears the deadline that bounds every read and write on the socket.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        match self {
            Transport::Tcp(s) => s.deadline = deadline,
            #[cfg(feature = "rtmps")]
            Transport::Tls(t) => t.sock.deadline = deadline,
        }
    }

    /// Reads, blocking up to the socket's read timeout. For TLS, rustls reads and decrypts records
    /// until some plaintext arrives.
    pub fn read_blocking(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Transport::Tcp(s) => s.read(buf),
            #[cfg(feature = "rtmps")]
            Transport::Tls(t) => t.read(buf),
        }
    }

    /// Reads what has arrived, up to about `limit` bytes, without waiting, and passes it to
    /// `sink`. Returns `Ok(true)` when the peer closed the connection.
    pub fn read_available(&mut self, limit: usize, sink: &mut dyn FnMut(&[u8])) -> io::Result<bool> {
        self.socket().set_nonblocking(true)?;
        let result = self.drain(limit, sink);
        let restore = self.socket().set_nonblocking(false);
        let closed = result?;
        restore?;
        Ok(closed)
    }

    fn drain(&mut self, limit: usize, sink: &mut dyn FnMut(&[u8])) -> io::Result<bool> {
        let mut buf = [0u8; 4096];
        let mut total = 0;
        match self {
            Transport::Tcp(s) => loop {
                if total >= limit {
                    return Ok(false);
                }
                match s.read(&mut buf) {
                    Ok(0) => return Ok(true),
                    Ok(n) => {
                        total += n;
                        sink(&buf[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            },
            #[cfg(feature = "rtmps")]
            Transport::Tls(t) => loop {
                // Plaintext rustls already holds comes first.
                loop {
                    if total >= limit {
                        return Ok(false);
                    }
                    match t.conn.reader().read(&mut buf) {
                        Ok(0) => return Ok(true),
                        Ok(n) => {
                            total += n;
                            sink(&buf[..n]);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                }
                match t.conn.read_tls(&mut t.sock) {
                    Ok(0) => return Ok(true),
                    Ok(_) => {
                        t.conn.process_new_packets().map_err(io::Error::other)?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            },
        }
    }

    /// Closes the connection: TLS sends close_notify first, then the socket's sending side shuts.
    pub fn shutdown(&mut self) -> io::Result<()> {
        #[cfg(feature = "rtmps")]
        if let Transport::Tls(t) = self {
            t.conn.send_close_notify();
            self.flush()?;
        }
        self.socket().shutdown(Shutdown::Write)
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Transport::Tcp(s) => s.write(buf),
            #[cfg(feature = "rtmps")]
            Transport::Tls(t) => t.conn.writer().write(buf),
        }
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        match self {
            Transport::Tcp(s) => s.write_vectored(bufs),
            #[cfg(feature = "rtmps")]
            Transport::Tls(t) => t.conn.writer().write_vectored(bufs),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Transport::Tcp(s) => s.flush(),
            #[cfg(feature = "rtmps")]
            Transport::Tls(t) => {
                while t.conn.wants_write() {
                    t.conn.write_tls(&mut t.sock)?;
                }
                t.sock.flush()
            }
        }
    }
}

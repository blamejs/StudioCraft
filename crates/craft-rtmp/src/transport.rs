//! The connection under the chunk stream: a TCP socket, or TLS over one.

use std::io::{self, IoSlice, Read, Write};
use std::net::{Shutdown, TcpStream};

/// A TCP socket, or a TLS session over one. Writes to a TLS session go into rustls's buffer and
/// reach the socket on [`Transport::flush`], so one message is one flush.
pub enum Transport {
    /// Plain RTMP.
    Tcp(TcpStream),
    /// RTMPS.
    #[cfg(feature = "rtmps")]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Transport {
    /// The socket.
    pub fn socket(&self) -> &TcpStream {
        match self {
            Transport::Tcp(s) => s,
            #[cfg(feature = "rtmps")]
            Transport::Tls(t) => &t.sock,
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

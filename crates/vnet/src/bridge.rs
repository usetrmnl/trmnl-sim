//! Pumps bytes between a non-blocking host `TcpStream` and a smoltcp TCP socket.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};

use smoltcp::socket::tcp::{self, State};
use smoltcp::time::Duration;

pub(crate) const TCP_BUF: usize = 64 * 1024;

/// Create a smoltcp TCP socket with the buffer sizes/options used on both sides.
pub(crate) fn new_tcp_socket() -> tcp::Socket<'static> {
    let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; TCP_BUF]), tcp::SocketBuffer::new(vec![0; TCP_BUF]));
    s.set_nagle_enabled(false);
    // Abort if the peer stops acknowledging our data for this long.
    s.set_timeout(Some(Duration::from_secs(120)));
    s
}

pub(crate) struct Bridge {
    stream: TcpStream,
    /// Host side reached EOF (we closed our send direction toward the smoltcp peer).
    host_eof: bool,
    /// We've shut down writing to the host (the smoltcp peer sent FIN).
    host_wr_shut: bool,
    /// Whether the smoltcp socket has ever reached a synchronized state.
    synced: bool,
}

pub(crate) enum PumpResult {
    Alive,
    /// Connection finished (or failed); the socket and host stream can be dropped.
    Done,
}

impl Bridge {
    pub fn new(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        let _ = stream.set_nodelay(true);
        Ok(Self { stream, host_eof: false, host_wr_shut: false, synced: false })
    }

    pub fn pump(&mut self, sock: &mut tcp::Socket<'_>) -> PumpResult {
        let state = sock.state();
        match state {
            State::Closed | State::TimeWait => {
                // Deliver whatever the peer sent before closing, best effort.
                self.flush_to_host(sock);
                if !self.synced || state == State::Closed && !self.host_wr_shut {
                    // Reset / never established: make the host side see an abrupt close.
                    let _ = self.stream.shutdown(Shutdown::Both);
                } else {
                    let _ = self.stream.shutdown(Shutdown::Write);
                }
                return PumpResult::Done;
            }
            State::Listen | State::SynSent | State::SynReceived => return PumpResult::Alive,
            _ => self.synced = true,
        }

        // peer -> host
        if !self.flush_to_host(sock) {
            sock.abort();
            let _ = self.stream.shutdown(Shutdown::Both);
            return PumpResult::Done;
        }
        let peer_fin = matches!(sock.state(), State::CloseWait | State::LastAck | State::Closing | State::TimeWait);
        if peer_fin && sock.recv_queue() == 0 && !self.host_wr_shut {
            self.host_wr_shut = true;
            let _ = self.stream.shutdown(Shutdown::Write);
        }

        // host -> peer
        if !self.host_eof && sock.may_send() {
            while sock.can_send() {
                let res = sock.send(|buf| match self.stream.read(buf) {
                    Ok(0) => (0, Ok(true)),
                    Ok(n) => (n, Ok(false)),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => (0, Err(None)),
                    Err(e) if e.kind() == ErrorKind::Interrupted => (0, Ok(false)),
                    Err(e) => (0, Err(Some(e))),
                });
                match res {
                    Ok(Ok(false)) => continue,
                    Ok(Ok(true)) => {
                        self.host_eof = true;
                        sock.close();
                        break;
                    }
                    Ok(Err(None)) => break,
                    Ok(Err(Some(e))) => {
                        log::debug!("vnet: host read error: {e}");
                        sock.abort();
                        return PumpResult::Done;
                    }
                    Err(_) => break,
                }
            }
        }
        PumpResult::Alive
    }

    /// Returns false on a fatal host write error.
    fn flush_to_host(&mut self, sock: &mut tcp::Socket<'_>) -> bool {
        if self.host_wr_shut {
            // Discard anything further (shouldn't happen).
            return true;
        }
        while sock.can_recv() {
            let res = sock.recv(|buf| match self.stream.write(buf) {
                Ok(n) => (n, Ok(n)),
                Err(e) if e.kind() == ErrorKind::WouldBlock => (0, Ok(0)),
                Err(e) if e.kind() == ErrorKind::Interrupted => (0, Ok(1)),
                Err(e) => (0, Err(e)),
            });
            match res {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    log::debug!("vnet: host write error: {e}");
                    return false;
                }
                Err(_) => break,
            }
        }
        true
    }
}

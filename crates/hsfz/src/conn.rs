//! Async HSFZ connection over TCP — the concrete F-series transport.
//!
//! Milestone 1 is a single request/response, so this is a thin wrapper over a
//! `TcpStream`: connect, send a frame, receive a frame (reassembled across TCP
//! segment boundaries), and a `request` helper that skips non-diagnostic frames
//! (e.g. a 0x02 ack) the way Scapy's HSFZ socket does.
//!
//! No `Transport` trait — there is one transport today (CLAUDE.md). A trait is
//! extracted when DoIP is actually added.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::timeout;

use crate::Error;
use crate::frame::{self, HsfzFrame};

/// A connected HSFZ diagnostic channel (TCP 6801).
pub struct HsfzConnection {
    stream: TcpStream,
    peer: SocketAddr,
    /// Per-read timeout (default P2*). A short or absent read surfaces as
    /// `Error::ReadTimeout` instead of blocking forever.
    read_timeout: Duration,
}

impl HsfzConnection {
    /// Connect to the gateway's diagnostic port and set `TCP_NODELAY`.
    ///
    /// `connect_timeout` bounds the TCP connect; `read_timeout` bounds each
    /// subsequent frame read.
    pub async fn connect(
        ip: IpAddr,
        port: u16,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, Error> {
        let peer = SocketAddr::new(ip, port);
        let stream = match timeout(connect_timeout, TcpStream::connect(peer)).await {
            Ok(Ok(s)) => s,
            Ok(Err(source)) => return Err(Error::Connect { peer, source }),
            Err(_) => {
                return Err(Error::ConnectTimeout {
                    peer,
                    timeout: connect_timeout,
                });
            }
        };
        stream
            .set_nodelay(true)
            .map_err(|source| Error::Connect { peer, source })?;
        Ok(Self {
            stream,
            peer,
            read_timeout,
        })
    }

    /// The connected gateway address.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Send one HSFZ frame.
    ///
    /// Delegates to [`frame::write_frame`], the single source of frame encoding.
    pub async fn send_frame(&mut self, frame: &HsfzFrame) -> Result<(), Error> {
        frame::write_frame(&mut self.stream, frame).await
    }

    /// Receive one HSFZ frame, reassembling across TCP segments.
    ///
    /// Delegates to [`frame::read_frame`] — the single source of frame
    /// reassembly — bounding each read by this connection's `read_timeout`.
    pub async fn recv_frame(&mut self) -> Result<HsfzFrame, Error> {
        frame::read_frame(&mut self.stream, self.read_timeout).await
    }

    /// Send a UDS request and return the UDS payload of the diagnostic response.
    ///
    /// Wraps `uds` in a control-0x01 frame (`source` -> `target`), sends it, then
    /// reads frames, skipping any non-diagnostic frame (e.g. a 0x02 ack) until
    /// the 0x01 response arrives. UDS-level concerns — the NRC 0x78
    /// response-pending retry — live above this, in the caller.
    pub async fn request(&mut self, source: u8, target: u8, uds: &[u8]) -> Result<Vec<u8>, Error> {
        let req = HsfzFrame::diagnostic(source, target, uds.to_vec());
        self.send_frame(&req).await?;
        self.recv_response().await
    }

    /// Read frames until a diagnostic (control 0x01) frame arrives; return its
    /// UDS payload. Non-diagnostic frames (acks, keepalives) are skipped. A
    /// stall is bounded by `read_timeout` on each underlying read.
    pub async fn recv_response(&mut self) -> Result<Vec<u8>, Error> {
        loop {
            let frame = self.recv_frame().await?;
            if frame.control == frame::control::DIAGNOSTIC {
                return Ok(frame.payload);
            }
            // Non-diagnostic (ack/keepalive/other) — skip and keep reading.
        }
    }

    /// Split into the owned read/write halves plus the peer and read timeout.
    ///
    /// This is the seam for building a managed session: a background keepalive
    /// task needs to write to the socket while another task is blocked reading a
    /// response, which a single `&mut TcpStream` cannot express. Splitting yields
    /// an [`OwnedWriteHalf`] (shareable behind a lock, for the keepalive) and an
    /// [`OwnedReadHalf`] (owned by the response reader). The `peer` and
    /// `read_timeout` are returned alongside because the session still needs them.
    ///
    /// `TCP_NODELAY`, set in [`HsfzConnection::connect`], is a socket option and
    /// survives the split. This deliberately leaks the tokio half types: the
    /// crate is tokio-committed and the split is fundamental to the keepalive.
    pub fn into_parts(self) -> (OwnedReadHalf, OwnedWriteHalf, SocketAddr, Duration) {
        let (read, write) = self.stream.into_split();
        (read, write, self.peer, self.read_timeout)
    }
}

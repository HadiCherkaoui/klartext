//! Async HSFZ connection over TCP — the concrete F-series transport.
//!
//! A thin wrapper over a `TcpStream`: connect to the gateway (setting
//! `TCP_NODELAY`), then split into the owned read/write halves a managed session
//! drives. The request/response loop — and all frame I/O — lives one layer up, in
//! `klartext-client`.
//!
//! No `Transport` trait — there is one transport today (CLAUDE.md). A trait is
//! extracted when DoIP is actually added.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::timeout;

use crate::Error;

/// A connected HSFZ diagnostic channel (TCP 6801).
pub struct HsfzConnection {
    stream: TcpStream,
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
            read_timeout,
        })
    }

    /// Split into the owned read/write halves plus the read timeout.
    ///
    /// This is the seam for building a managed session: a background keepalive
    /// task needs to write to the socket while another task is blocked reading a
    /// response, which a single `&mut TcpStream` cannot express. Splitting yields
    /// an [`OwnedWriteHalf`] (shareable behind a lock, for the keepalive) and an
    /// [`OwnedReadHalf`] (owned by the response reader). The `read_timeout` is
    /// returned alongside because the session still needs it.
    ///
    /// `TCP_NODELAY`, set in [`HsfzConnection::connect`], is a socket option and
    /// survives the split. This deliberately leaks the tokio half types: the
    /// crate is tokio-committed and the split is fundamental to the keepalive.
    pub fn into_parts(self) -> (OwnedReadHalf, OwnedWriteHalf, Duration) {
        let (read, write) = self.stream.into_split();
        (read, write, self.read_timeout)
    }
}

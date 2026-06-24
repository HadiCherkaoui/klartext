//! HSFZ gateway auto-discovery over UDP broadcast, from `docs/protocol-reference.md` §2.5.
//!
//! On an ENET cable the host and the car's central gateway (ZGW) come up on a
//! link-local `169.254.0.0/16` network. [`discover`] broadcasts the verbatim HSFZ
//! identification request (`00 00 00 00 00 11`) on UDP 6811 and collects the
//! gateways that answer, so the rest of the tool can connect without the user
//! typing an IP.
//!
//! Two values here are reverse-engineered and [verify against capture]:
//!
//! - The gateway's IP is taken from the **UDP source address** of its reply, which
//!   is authoritative and capture-independent — not from the reply body.
//! - The internal layout of the 0x11 identification string is undocumented, so the
//!   VIN is extracted **best-effort** by scanning for a 17-character VIN run; the
//!   full datagram is kept in [`Gateway::raw`] so the real offsets can be resolved
//!   against a capture later.
//!
//! The discovery socket must leave the ENET NIC, so it is bound to that
//! interface's link-local source IP (see [`link_local_bind_ip`]); binding a UDP
//! socket to an interface's address makes a directed broadcast egress that
//! interface, without needing `SO_BINDTODEVICE`/root.

use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::Error;
use crate::frame::HsfzFrame;

/// Length of a VIN in characters (ISO 3779).
const VIN_LEN: usize = 17;

/// A gateway that answered the HSFZ identification broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    /// The gateway's IP — the source address of its reply (authoritative).
    pub ip: IpAddr,
    /// The VIN, if a 17-character VIN run was found in the reply (best-effort).
    pub vin: Option<String>,
    /// The full reply datagram, kept so the 0x11 layout can be resolved later.
    pub raw: Vec<u8>,
}

/// Find a link-local IPv4 (`169.254.0.0/16`) to bind discovery to, if any.
///
/// Returns the first such address across the host's interfaces — on a plugged-in
/// ENET cable that is the address on the cable's NIC. When the host has more than
/// one link-local interface (a VM bridge, a second NIC), the caller should let
/// the user override the bind address rather than rely on this pick.
pub fn link_local_bind_ip() -> Option<Ipv4Addr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .find_map(|iface| match iface.addr.ip() {
            IpAddr::V4(v4) if v4.is_link_local() => Some(v4),
            _ => None,
        })
}

/// Broadcast the identification request and collect the gateways that reply.
///
/// Binds a UDP socket to `bind_ip` (so the broadcast leaves that NIC), enables
/// `SO_BROADCAST`, sends `00 00 00 00 00 11` to `broadcast:port`, and listens for
/// `wait` for replies. An empty `Vec` means nothing answered — the caller turns
/// that into a clear "no gateway found" error rather than a panic.
///
/// # Errors
/// Returns [`Error::DiscoveryBind`] if the socket cannot bind `bind_ip` or enable
/// broadcast (commonly a wrong bind address), and [`Error::Io`] on a send or
/// receive failure.
pub async fn discover(
    bind_ip: Ipv4Addr,
    broadcast: Ipv4Addr,
    port: u16,
    wait: Duration,
) -> Result<Vec<Gateway>, Error> {
    let socket = UdpSocket::bind((bind_ip, 0))
        .await
        .map_err(|source| Error::DiscoveryBind { bind_ip, source })?;
    socket
        .set_broadcast(true)
        .map_err(|source| Error::DiscoveryBind { bind_ip, source })?;

    let probe = HsfzFrame::identification_request().encode();
    let dest = SocketAddrV4::new(broadcast, port);
    socket.send_to(&probe, dest).await.map_err(Error::Io)?;

    // Read replies until the listen window elapses with no further datagram.
    let mut gateways = Vec::new();
    let mut buf = vec![0u8; 2048];
    while let Ok(result) = timeout(wait, socket.recv_from(&mut buf)).await {
        let (n, from) = result.map_err(Error::Io)?;
        let raw = buf[..n].to_vec();
        let vin = scan_vin(&raw);
        gateways.push(Gateway {
            ip: from.ip(),
            vin,
            raw,
        });
    }
    Ok(gateways)
}

/// True for a character allowed in a VIN (ISO 3779 excludes I, O, and Q).
fn is_vin_char(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'N' | b'P' | b'R'..=b'Z')
}

/// Best-effort VIN scan: the first 17-byte run of VIN characters in `bytes`.
///
/// The 0x11 body offsets are undocumented, so rather than hard-code an offset
/// this scans the whole datagram. The header and binary fields are not VIN
/// characters, so a real 17-character VIN stands out. [verify against capture].
fn scan_vin(bytes: &[u8]) -> Option<String> {
    bytes
        .windows(VIN_LEN)
        .find(|window| window.iter().all(|&b| is_vin_char(b)))
        .map(|window| String::from_utf8_lossy(window).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // DERIVED: a synthetic 0x11 reply. The header + a binary field precede a
    // 17-char VIN; the exact offset is unknown ([verify against capture]), so the
    // scan must find the VIN wherever it sits.
    #[test]
    fn scan_vin_finds_a_17_char_vin_in_the_body() {
        let mut datagram = vec![0x00, 0x00, 0x00, 0x1B, 0x00, 0x11]; // HSFZ 0x11 header
        datagram.extend_from_slice(&[0x10, 0xF4, 0x00, 0x01]); // some binary field
        datagram.extend_from_slice(b"WBA3B5C50EK123456"); // 17-char VIN
        assert_eq!(scan_vin(&datagram).as_deref(), Some("WBA3B5C50EK123456"));
    }

    #[test]
    fn scan_vin_returns_none_without_a_vin_run() {
        // Binary-only body with no 17-char printable run.
        let datagram = [0x00, 0x00, 0x00, 0x06, 0x00, 0x11, 0x10, 0xF4, 0x00, 0x01];
        assert_eq!(scan_vin(&datagram), None);
    }

    #[test]
    fn vin_alphabet_excludes_i_o_q() {
        assert!(!is_vin_char(b'I'));
        assert!(!is_vin_char(b'O'));
        assert!(!is_vin_char(b'Q'));
        assert!(is_vin_char(b'H'));
        assert!(is_vin_char(b'P'));
        assert!(is_vin_char(b'Z'));
        assert!(is_vin_char(b'0'));
    }

    // End-to-end over loopback: a mock gateway answers the probe, and `discover`
    // reports its IP (the reply's source address) and the VIN from the body. The
    // broadcast target is pointed at the mock so the round-trip needs no real NIC.
    #[tokio::test]
    async fn discover_reports_source_ip_and_vin() {
        use tokio::net::UdpSocket;

        let gateway = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let gateway_port = gateway.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let (_, from) = gateway.recv_from(&mut buf).await.unwrap();
            let mut reply = vec![0x00, 0x00, 0x00, 0x1B, 0x00, 0x11]; // 0x11 header
            reply.extend_from_slice(b"WBA3B5C50EK123456"); // VIN in the body
            gateway.send_to(&reply, from).await.unwrap();
        });

        // "Broadcast" straight at the mock on loopback — exercises the full path.
        let gateways = discover(
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::LOCALHOST,
            gateway_port,
            Duration::from_millis(500),
        )
        .await
        .unwrap();

        assert_eq!(gateways.len(), 1);
        assert_eq!(gateways[0].ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(gateways[0].vin.as_deref(), Some("WBA3B5C50EK123456"));
    }
}

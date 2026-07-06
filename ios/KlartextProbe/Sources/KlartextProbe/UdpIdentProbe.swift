import Foundation
import Darwin
import KlartextHSFZ

/// Unicast HSFZ identification probe — THE discovery experiment for iOS.
///
/// Sends the verbatim 6-byte 0x11 ident request (mirrors crates/hsfz discover.rs)
/// to `<gateway IP>:6811` over a plain BSD UDP socket — the exact path tokio's
/// `UdpSocket` uses. Unicast needs NO multicast entitlement; the spec §2.3 wall
/// applies only to broadcast/receive-broadcast. So if the ZGW answers a UNICAST
/// ident, entitlement-free discovery = this datagram swept across the subnet, and
/// the broadcast entitlement stays optional. No open source (ediabaslib included)
/// documents whether the ZGW answers unicast ident — this button is the test.
///
/// The socket is `connect()`ed so an ICMP port-unreachable surfaces as
/// ECONNREFUSED ("host there, no ident service") instead of being
/// indistinguishable from silence. No interface bind, same as PosixProbe: this
/// also probes whether default routing egresses the ENET adapter for UDP.
enum UdpIdentProbe {
    enum Outcome {
        case replied(ms: Int, frame: HsfzFrame?, vin: String?, raw: [UInt8])
        case refused        // ICMP port unreachable — host up, nothing on 6811
        case timedOut       // silence: no host, mute gateway, or wrong egress interface
        case failed(String)
    }

    static func run(host: String, port: UInt16, timeoutSeconds: Int) -> Outcome {
        let fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
        guard fd >= 0 else { return .failed("socket() errno \(errno)") }
        defer { close(fd) }

        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        guard inet_pton(AF_INET, host, &addr.sin_addr) == 1 else {
            return .failed("not an IPv4 address: \(host)")
        }
        // UDP connect() sends nothing — it just pins the peer and enables ICMP errors.
        let rc = withUnsafePointer(to: &addr) { raw in
            raw.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                Darwin.connect(fd, sa, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard rc == 0 else { return .failed("connect() errno \(errno)") }

        let probe = [UInt8](Hsfz.encodeIdentificationRequest())
        let start = Date()
        let sent = probe.withUnsafeBytes { send(fd, $0.baseAddress, $0.count, 0) }
        guard sent == probe.count else { return .failed("send() errno \(errno)") }

        var pfd = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
        let ready = poll(&pfd, 1, Int32(timeoutSeconds * 1000))
        if ready == 0 { return .timedOut }
        if ready < 0 { return .failed("poll() errno \(errno)") }

        var buf = [UInt8](repeating: 0, count: 2048)
        let n = recv(fd, &buf, buf.count, 0)
        if n < 0 {
            return errno == ECONNREFUSED ? .refused : .failed("recv() errno \(errno)")
        }
        let raw = Array(buf[0..<n])
        var frames = FrameBuffer()
        frames.append(Data(raw))
        let frame = frames.nextFrame()   // announcement is one whole frame per datagram
        return .replied(ms: Int(Date().timeIntervalSince(start) * 1000),
                        frame: frame,
                        vin: Hsfz.scanVin(in: raw),
                        raw: raw)
    }
}

import Foundation
import Darwin

/// Plain BSD-socket TCP connect — by default with NO interface bind, the exact path
/// tokio uses underneath. If this reaches a link-local gateway with cellular up, the
/// sans-I/O refactor can be deferred; if it times out, the bound retry (`bindIP` =
/// the ENET source address, cf. discover.rs's bind-before-send technique — also
/// tokio-compatible) tells whether source-binding fixes egress before falling back
/// to NWConnection (design spec §2.2, §4). Non-blocking connect + poll() so it is
/// strictly time-bounded.
enum PosixProbe {
    enum Outcome: Equatable {
        case connected(ms: Int)
        case timedOut
        case failed(String)
    }

    static func connect(host: String, port: UInt16, timeoutSeconds: Int,
                        bindIP: String? = nil) -> Outcome {
        let fd = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP)
        guard fd >= 0 else { return .failed("socket() errno \(errno)") }
        defer { close(fd) }

        if let bindIP {
            var src = sockaddr_in()
            src.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
            src.sin_family = sa_family_t(AF_INET)
            src.sin_port = 0
            guard inet_pton(AF_INET, bindIP, &src.sin_addr) == 1 else {
                return .failed("not an IPv4 bind address: \(bindIP)")
            }
            let brc = withUnsafePointer(to: &src) { raw in
                raw.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                    Darwin.bind(fd, sa, socklen_t(MemoryLayout<sockaddr_in>.size))
                }
            }
            guard brc == 0 else { return .failed("bind(\(bindIP)) errno \(errno)") }
        }

        // Non-blocking so connect() returns immediately with EINPROGRESS.
        let flags = fcntl(fd, F_GETFL, 0)
        _ = fcntl(fd, F_SETFL, flags | O_NONBLOCK)

        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        guard inet_pton(AF_INET, host, &addr.sin_addr) == 1 else {
            return .failed("not an IPv4 address: \(host)")
        }

        let start = Date()
        let rc = withUnsafePointer(to: &addr) { raw in
            raw.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                Darwin.connect(fd, sa, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        if rc == 0 { return .connected(ms: 0) } // immediate (unlikely, but valid)
        if errno != EINPROGRESS { return .failed("connect() errno \(errno)") }

        var pfd = pollfd(fd: fd, events: Int16(POLLOUT), revents: 0)
        let ready = poll(&pfd, 1, Int32(timeoutSeconds * 1000))
        if ready == 0 { return .timedOut }
        if ready < 0 { return .failed("poll() errno \(errno)") }

        // Writable — check SO_ERROR to tell success from refused/unreachable.
        var soErr: Int32 = 0
        var len = socklen_t(MemoryLayout<Int32>.size)
        getsockopt(fd, SOL_SOCKET, SO_ERROR, &soErr, &len)
        if soErr != 0 { return .failed("SO_ERROR \(soErr)") }
        return .connected(ms: Int(Date().timeIntervalSince(start) * 1000))
    }
}

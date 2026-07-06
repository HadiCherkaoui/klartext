import Foundation
import Darwin

/// Lists this device's IPv4 interfaces + addresses via getifaddrs (supported on iOS).
/// Note: this shows only OUR OWN addresses — not the peer's. The gateway's IP is not
/// discoverable here (ARP is sandbox-blocked on iOS; see the design spec §2.3), which
/// is why the gateway IP is entered manually.
enum InterfaceInspector {
    static func ipv4Interfaces() -> [(name: String, ip: String, netmask: String)] {
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return [] }
        defer { freeifaddrs(head) }

        var result: [(name: String, ip: String, netmask: String)] = []
        var ptr: UnsafeMutablePointer<ifaddrs>? = first
        while let cur = ptr {
            defer { ptr = cur.pointee.ifa_next }
            guard let sa = cur.pointee.ifa_addr,
                  sa.pointee.sa_family == UInt8(AF_INET) else { continue }
            let name = String(cString: cur.pointee.ifa_name)
            let ip = Self.numericHost(cur.pointee.ifa_addr)
            let mask = Self.numericHost(cur.pointee.ifa_netmask)
            result.append((name: name, ip: ip, netmask: mask))
        }
        return result
    }

    private static func numericHost(_ sa: UnsafeMutablePointer<sockaddr>?) -> String {
        guard let sa else { return "" }
        var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
        let rc = getnameinfo(sa, socklen_t(sa.pointee.sa_len),
                             &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST)
        return rc == 0 ? String(cString: host) : ""
    }
}

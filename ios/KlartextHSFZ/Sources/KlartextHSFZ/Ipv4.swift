import Foundation

/// Pure IPv4 helpers for source-interface selection — the Swift mirror of
/// crates/hsfz/src/discover.rs `link_local_bind_ip`, generalized for the probe:
/// prefer the interface whose subnet CONTAINS the gateway (handles the car's
/// static-IP config, DID 0x172A), fall back to the first link-local interface
/// (the APIPA ENET default). No I/O, no platform frameworks — tests run on Linux;
/// the app feeds it `getifaddrs` results (InterfaceInspector).
public enum Ipv4 {
    /// Dotted-quad → host-order UInt32. Strict: exactly four ASCII-digit octets ≤ 255.
    public static func parse(_ s: String) -> UInt32? {
        let parts = s.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else { return nil }
        var addr: UInt32 = 0
        for part in parts {
            guard !part.isEmpty, part.count <= 3,
                  part.allSatisfy({ $0.isASCII && $0.isWholeNumber }),
                  let octet = UInt32(part), octet <= 255 else { return nil }
            addr = (addr << 8) | octet
        }
        return addr
    }

    /// 169.254.0.0/16 (RFC 3927 link-local / APIPA).
    public static func isLinkLocal(_ addr: UInt32) -> Bool {
        addr >> 16 == 0xA9FE
    }

    /// 127.0.0.0/8.
    static func isLoopback(_ addr: UInt32) -> Bool {
        addr >> 24 == 127
    }

    /// Pick the source interface for reaching `gateway`.
    ///
    /// Order of evidence: (1) the first non-loopback interface whose subnet
    /// contains the gateway — strongest, and covers static ENET configs; (2) the
    /// first link-local interface — the plain-cable APIPA case, also the answer
    /// when the gateway field is unparseable. A zero netmask never matches (it
    /// would "contain" everything). Returns nil when nothing qualifies.
    public static func pickSource(
        gateway: String,
        interfaces: [(name: String, ip: String, netmask: String)]
    ) -> (name: String, ip: String)? {
        let candidates: [(name: String, ip: String, addr: UInt32, mask: UInt32)] =
            interfaces.compactMap { iface in
                guard let addr = parse(iface.ip), !isLoopback(addr),
                      let mask = parse(iface.netmask) else { return nil }
                return (iface.name, iface.ip, addr, mask)
            }
        if let gw = parse(gateway),
           let match = candidates.first(where: { $0.mask != 0 && ($0.addr & $0.mask) == (gw & $0.mask) }) {
            return (match.name, match.ip)
        }
        return candidates.first(where: { isLinkLocal($0.addr) }).map { ($0.name, $0.ip) }
    }
}

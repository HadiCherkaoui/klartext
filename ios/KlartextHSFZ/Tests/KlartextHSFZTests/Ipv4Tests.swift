import Testing
import KlartextHSFZ

// Source-interface selection — the Swift mirror of crates/hsfz/src/discover.rs
// `link_local_bind_ip`, generalized: prefer the interface whose subnet CONTAINS the
// gateway (covers the car's static-IP config, DID 0x172A), fall back to the first
// link-local interface (the APIPA ENET default). Pure logic, `swift test` on Linux.
struct Ipv4Tests {
    private let loopback = (name: "lo0", ip: "127.0.0.1", netmask: "255.0.0.0")
    private let wifi = (name: "en0", ip: "192.168.1.10", netmask: "255.255.255.0")
    private let apipa = (name: "en3", ip: "169.254.10.20", netmask: "255.255.0.0")
    private let staticEnet = (name: "en3", ip: "192.168.17.42", netmask: "255.255.255.0")

    @Test func parsesDottedQuads() throws {
        #expect(Ipv4.parse("169.254.1.2") == 0xA9FE_0102)
        #expect(Ipv4.parse("192.168.17.151") == 0xC0A8_1197)
        #expect(Ipv4.parse("0.0.0.0") == 0)
        #expect(Ipv4.parse("255.255.255.255") == 0xFFFF_FFFF)
    }

    @Test(arguments: ["", "1.2.3", "1.2.3.4.5", "256.1.1.1", "a.b.c.d", "1.2.3.-4", "1.2.3.+4", "1.2.3.4 ", "1..3.4"])
    func rejectsMalformedAddresses(_ candidate: String) {
        #expect(Ipv4.parse(candidate) == nil)
    }

    @Test func recognizesLinkLocal() throws {
        #expect(Ipv4.isLinkLocal(try #require(Ipv4.parse("169.254.0.1"))))
        #expect(!Ipv4.isLinkLocal(try #require(Ipv4.parse("192.168.17.151"))))
        #expect(!Ipv4.isLinkLocal(try #require(Ipv4.parse("169.253.255.255"))))
    }

    @Test func picksTheSubnetMatchForAStaticGatewayConfig() {
        // Car statically at 192.168.17.151/24 (the capture's 0x172A config), phone's
        // adapter manually in that subnet: the adapter must win over Wi-Fi.
        let source = Ipv4.pickSource(gateway: "192.168.17.151",
                                     interfaces: [loopback, wifi, staticEnet])
        #expect(source?.name == "en3")
        #expect(source?.ip == "192.168.17.42")
    }

    @Test func picksTheSubnetMatchForALinkLocalGateway() {
        // Plain ENET cable: both sides APIPA — subnet match and link-local coincide.
        let source = Ipv4.pickSource(gateway: "169.254.44.55",
                                     interfaces: [loopback, wifi, apipa])
        #expect(source?.name == "en3")
        #expect(source?.ip == "169.254.10.20")
    }

    @Test func subnetMatchBeatsTheLinkLocalFallback() {
        // Adapter reconfigured static while some other interface sits on APIPA:
        // containing-subnet evidence outranks the link-local heuristic.
        let otherApipa = (name: "en5", ip: "169.254.99.1", netmask: "255.255.0.0")
        let source = Ipv4.pickSource(gateway: "192.168.17.151",
                                     interfaces: [otherApipa, staticEnet])
        #expect(source?.name == "en3")
        #expect(source?.ip == "192.168.17.42")
    }

    @Test func fallsBackToLinkLocalWhenNoSubnetContainsTheGateway() {
        // Typed a static gateway IP while the adapter is still on APIPA: the
        // link-local interface is the best guess for the ENET side.
        let source = Ipv4.pickSource(gateway: "192.168.17.151",
                                     interfaces: [loopback, wifi, apipa])
        #expect(source?.name == "en3")
        #expect(source?.ip == "169.254.10.20")
    }

    @Test func returnsNilWithoutASubnetMatchOrLinkLocal() {
        let source = Ipv4.pickSource(gateway: "192.168.17.151",
                                     interfaces: [loopback, wifi])
        #expect(source == nil)
    }

    @Test func neverPicksLoopback() {
        let source = Ipv4.pickSource(gateway: "127.0.0.1", interfaces: [loopback])
        #expect(source == nil)
    }

    @Test func aZeroNetmaskMatchesNothing() {
        // A 0.0.0.0 mask (some tunnels) would "contain" every gateway — must not win.
        let tunnel = (name: "utun0", ip: "10.8.0.2", netmask: "0.0.0.0")
        let source = Ipv4.pickSource(gateway: "192.168.17.151",
                                     interfaces: [tunnel, apipa])
        #expect(source?.name == "en3")
    }

    @Test func anUnparseableGatewayStillFallsBackToLinkLocal() {
        // Garbled IP field: subnet matching is impossible, but the APIPA adapter
        // remains the sensible source suggestion.
        let source = Ipv4.pickSource(gateway: "not-an-ip", interfaces: [wifi, apipa])
        #expect(source?.name == "en3")
    }

    @Test func skipsInterfacesWithUnparseableEntries() {
        let junk = (name: "en9", ip: "garbage", netmask: "255.255.255.0")
        let source = Ipv4.pickSource(gateway: "169.254.44.55", interfaces: [junk, apipa])
        #expect(source?.name == "en3")
    }
}

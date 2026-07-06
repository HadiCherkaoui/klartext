import Testing
import Foundation
import KlartextHSFZ

// Pure-codec tests — run with `swift test` on Linux (no iOS SDK / device needed).
// Byte vectors shared verbatim with crates/hsfz/src/frame.rs tests.
//
// NOTE: `FrameBuffer.nextFrame()` is `mutating`. The #expect/#require macros capture their
// argument as an immutable value, so calling a mutating method *inside* them fails to
// compile ("cannot use mutating member on immutable value"). Always call nextFrame() into
// a local first, then assert on that local.
struct HsfzCodecTests {
    @Test func encodesTesterPresentToGateway() {
        let out = Hsfz.encodeDiagnostic(src: 0xF4, tgt: 0x10, uds: [0x3E, 0x00])
        // LENGTH = 2 (src+tgt) + 2 (uds) = 4
        #expect(Array(out) == [0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0xF4, 0x10, 0x3E, 0x00])
    }

    @Test func encodesVinRequestToGateway() {
        let out = Hsfz.encodeDiagnostic(src: 0xF4, tgt: 0x10, uds: [0x22, 0xF1, 0x90])
        // LENGTH = 2 (src+tgt) + 3 (uds) = 5
        #expect(Array(out) == [0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0xF4, 0x10, 0x22, 0xF1, 0x90])
    }

    @Test func decodesAWholeVinResponse() throws {
        // 62 F1 90 + "WBA3B5C50EK123456" (17 bytes) => LENGTH = 2 + 3 + 17 = 0x16
        var bytes: [UInt8] = [0x00, 0x00, 0x00, 0x16, 0x00, 0x01, 0x10, 0xF4, 0x62, 0xF1, 0x90]
        bytes += Array("WBA3B5C50EK123456".utf8)
        var buf = FrameBuffer()
        buf.append(Data(bytes))

        let decoded = buf.nextFrame()            // mutating call — hoist out of the macro
        let frame = try #require(decoded)
        #expect(frame.src == 0x10)
        #expect(frame.tgt == 0xF4)
        #expect(Array(frame.payload.prefix(3)) == [0x62, 0xF1, 0x90])

        let leftover = buf.nextFrame()
        #expect(leftover == nil)                 // only one frame present
    }

    @Test func reassemblesAcrossTwoChunks() throws {
        let bytes: [UInt8] = [0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x10, 0xF4, 0x7E, 0x00]
        var buf = FrameBuffer()
        buf.append(Data(bytes[0..<5]))           // partial: header not yet complete
        let partial = buf.nextFrame()
        #expect(partial == nil)

        buf.append(Data(bytes[5...]))            // remainder arrives
        let decoded = buf.nextFrame()
        let complete = try #require(decoded)
        #expect(complete == HsfzFrame(control: 0x0001, src: 0x10, tgt: 0xF4, payload: [0x7E, 0x00]))
    }

    @Test func rejectsAnOversizedLength() {
        // LENGTH = 0xFFFFFFFF must fault, not buffer forever waiting for bytes.
        var buf = FrameBuffer()
        buf.append(Data([0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x01]))
        let result = buf.nextFrame()
        #expect(result == nil)
        #expect(buf.isFaulted)
    }

    @Test func encodesIdentificationRequest() {
        // The bare discovery datagram: LENGTH = 0, control 0x11 — the verbatim
        // 6 bytes crates/hsfz sends to UDP 6811 (frame.rs
        // discovery_request_round_trips_verbatim). Unicast vs broadcast is the
        // sender's choice; the bytes are identical.
        let out = Hsfz.encodeIdentificationRequest()
        #expect(Array(out) == [0x00, 0x00, 0x00, 0x00, 0x00, 0x11])
    }

    @Test func decodesAnIdentAnnouncementWithoutAddresses() throws {
        // A 0x11 announcement carries an identification string, not SRC/TGT — the
        // whole body must land in payload. Pins the existing non-diagnostic
        // FrameBuffer path the UDP ident probe now relies on.
        var bytes: [UInt8] = [0x00, 0x00, 0x00, 0x11, 0x00, 0x11] // LENGTH = 17
        bytes += Array("WBA3B5C50EK123456".utf8)
        var buf = FrameBuffer()
        buf.append(Data(bytes))

        let decoded = buf.nextFrame()            // mutating call — hoist out of the macro
        let frame = try #require(decoded)
        #expect(frame.control == 0x0011)
        #expect(frame.src == nil)
        #expect(frame.tgt == nil)
        #expect(frame.payload == Array("WBA3B5C50EK123456".utf8))
    }

    @Test func scanVinPrefersTheBmwvinMarkerOverAFalsePrefixRun() {
        // The real 0x11 body shape (verified 2026-07-03 against the F20 capture):
        // DIAGADR<addr>BMWMAC<mac>BMWVIN<vin>. The prefix contains a 17-char
        // VIN-alphabet run (AGADR10BMWMAC001A) that a naive scan wrongly returns;
        // the marker-anchored parse must return the true VIN. Vector shared with
        // crates/hsfz/src/discover.rs.
        var datagram: [UInt8] = [0x00, 0x00, 0x00, 0x32, 0x00, 0x11]
        datagram += Array("DIAGADR10BMWMAC001A37265429BMWVINWBA3B5C50EK123456".utf8)
        #expect(Hsfz.scanVin(in: datagram) == "WBA3B5C50EK123456")
    }

    @Test func scanVinFallsBackToA17CharRunWithoutTheMarker() {
        // No BMWVIN marker (other announcement shapes) — the first 17-char
        // VIN-alphabet run anywhere in the datagram is the answer.
        var datagram: [UInt8] = [0x00, 0x00, 0x00, 0x1B, 0x00, 0x11, 0x10, 0xF4, 0x00, 0x01]
        datagram += Array("WBA3B5C50EK123456".utf8)
        #expect(Hsfz.scanVin(in: datagram) == "WBA3B5C50EK123456")
    }

    @Test func scanVinRejectsIllegalVinLettersIOQ() {
        // I, O, Q are not VIN characters (ISO 3779): a marker followed by a
        // 17-char string containing 'I' must not parse, and every fallback window
        // here overlaps an illegal letter — so the scan returns nil.
        var datagram: [UInt8] = [0x00, 0x00, 0x00, 0x17, 0x00, 0x11]
        datagram += Array("BMWVINWBA3B5C50EK1234I6".utf8)
        #expect(Hsfz.scanVin(in: datagram) == nil)
    }

    @Test func scanVinReturnsNilWithoutAVinRun() {
        // Binary-only body with no 17-char printable run.
        let datagram: [UInt8] = [0x00, 0x00, 0x00, 0x06, 0x00, 0x11, 0x10, 0xF4, 0x00, 0x01]
        #expect(Hsfz.scanVin(in: datagram) == nil)
    }
}

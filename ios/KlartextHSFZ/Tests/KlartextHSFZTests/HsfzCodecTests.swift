import Testing
import Foundation
import KlartextHSFZ

// Pure-codec tests — run with `swift test` on Linux (no iOS SDK / device needed).
// Byte vectors shared verbatim with crates/hsfz/src/frame.rs tests.
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

        let frame = try #require(buf.nextFrame())
        #expect(frame.src == 0x10)
        #expect(frame.tgt == 0xF4)
        #expect(Array(frame.payload.prefix(3)) == [0x62, 0xF1, 0x90])
        #expect(buf.nextFrame() == nil) // only one frame present
    }

    @Test func reassemblesAcrossTwoChunks() throws {
        let bytes: [UInt8] = [0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x10, 0xF4, 0x7E, 0x00]
        var buf = FrameBuffer()
        buf.append(Data(bytes[0..<5]))           // partial: header not yet complete
        #expect(buf.nextFrame() == nil)
        buf.append(Data(bytes[5...]))            // remainder arrives

        let frame = try #require(buf.nextFrame())
        #expect(frame == HsfzFrame(control: 0x0001, src: 0x10, tgt: 0xF4, payload: [0x7E, 0x00]))
    }

    @Test func rejectsAnOversizedLength() {
        // LENGTH = 0xFFFFFFFF must fault, not buffer forever waiting for bytes.
        var buf = FrameBuffer()
        buf.append(Data([0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x01]))
        #expect(buf.nextFrame() == nil)
        #expect(buf.isFaulted)
    }
}

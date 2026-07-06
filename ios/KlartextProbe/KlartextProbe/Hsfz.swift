import Foundation

/// HSFZ frame encode/decode — pure, no I/O. Mirrors crates/hsfz/src/frame.rs.
/// Wire: [LENGTH u32 BE][CONTROL u16 BE][SRC][TGT][UDS], LENGTH = 2 + len(UDS)
/// (SRC+TGT+UDS; the control word is NOT counted). See protocol-reference.md §2.1.
enum Hsfz {
    static let controlDiagnostic: UInt16 = 0x0001
    static let controlAck: UInt16 = 0x0002
    static let headerLen = 6
    static let maxFrameLen: UInt32 = 64 * 1024

    /// Encode a diagnostic (control 0x01) frame carrying `uds` from `src` to `tgt`.
    static func encodeDiagnostic(src: UInt8, tgt: UInt8, uds: [UInt8]) -> Data {
        let body: [UInt8] = [src, tgt] + uds
        var out = Data()
        var length = UInt32(body.count).bigEndian
        withUnsafeBytes(of: &length) { out.append(contentsOf: $0) }
        var control = controlDiagnostic.bigEndian
        withUnsafeBytes(of: &control) { out.append(contentsOf: $0) }
        out.append(contentsOf: body)
        return out
    }
}

struct HsfzFrame: Equatable {
    let control: UInt16
    let src: UInt8?
    let tgt: UInt8?
    let payload: [UInt8]
}

/// Accumulates bytes from a byte-stream (e.g. NWConnection.receive) and pops whole
/// frames as they complete. TCP is a byte stream, so frames may split across or
/// coalesce within reads — this is the single point of reassembly.
struct FrameBuffer {
    private var bytes: [UInt8] = []
    /// Set once a decoded length exceeds the sanity cap — a misframe (wrong
    /// endianness or an off-by-two) that the caller must surface rather than wait on.
    private(set) var isFaulted = false

    mutating func append(_ data: Data) {
        bytes.append(contentsOf: data)
    }

    /// Pop the next complete frame, or nil if one is not yet fully buffered (or the
    /// buffer has faulted).
    mutating func nextFrame() -> HsfzFrame? {
        guard !isFaulted, bytes.count >= Hsfz.headerLen else { return nil }
        let length = (UInt32(bytes[0]) << 24) | (UInt32(bytes[1]) << 16)
                   | (UInt32(bytes[2]) << 8) | UInt32(bytes[3])
        if length > Hsfz.maxFrameLen {
            isFaulted = true
            return nil
        }
        let control = (UInt16(bytes[4]) << 8) | UInt16(bytes[5])
        let total = Hsfz.headerLen + Int(length)
        guard bytes.count >= total else { return nil } // wait for more bytes
        let body = Array(bytes[Hsfz.headerLen..<total])
        bytes.removeFirst(total)

        let carriesAddrs = (control == Hsfz.controlDiagnostic || control == Hsfz.controlAck)
        if carriesAddrs, body.count >= 2 {
            return HsfzFrame(control: control, src: body[0], tgt: body[1],
                             payload: Array(body[2...]))
        }
        return HsfzFrame(control: control, src: nil, tgt: nil, payload: body)
    }
}

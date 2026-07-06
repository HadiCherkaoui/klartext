import Foundation

/// HSFZ frame encode/decode — pure, no I/O, no platform frameworks (builds & tests on
/// Linux). Mirrors crates/hsfz/src/frame.rs. Wire: [LENGTH u32 BE][CONTROL u16 BE][SRC]
/// [TGT][UDS], LENGTH = 2 + len(UDS) (SRC+TGT+UDS; control word NOT counted). See
/// protocol-reference.md §2.1.
public enum Hsfz {
    public static let controlDiagnostic: UInt16 = 0x0001
    public static let controlAck: UInt16 = 0x0002
    /// Vehicle identification / announcement (discovery). UDP 6811.
    public static let controlIdentification: UInt16 = 0x0011
    static let headerLen = 6
    static let maxFrameLen: UInt32 = 64 * 1024

    /// Encode a diagnostic (control 0x01) frame carrying `uds` from `src` to `tgt`.
    public static func encodeDiagnostic(src: UInt8, tgt: UInt8, uds: [UInt8]) -> Data {
        let body: [UInt8] = [src, tgt] + uds
        var out = Data()
        var length = UInt32(body.count).bigEndian
        withUnsafeBytes(of: &length) { out.append(contentsOf: $0) }
        var control = controlDiagnostic.bigEndian
        withUnsafeBytes(of: &control) { out.append(contentsOf: $0) }
        out.append(contentsOf: body)
        return out
    }

    /// The bare identification/discovery request (control 0x11, empty body) — the
    /// verbatim 6-byte datagram sent to UDP 6811. Mirrors crates/hsfz frame.rs
    /// `identification_request`; unicast vs broadcast is the sender's choice, the
    /// bytes are identical.
    public static func encodeIdentificationRequest() -> Data {
        Data([0x00, 0x00, 0x00, 0x00, 0x00, 0x11])
    }

    /// Length of a VIN in characters (ISO 3779).
    static let vinLength = 17
    /// Marker preceding the VIN in the 0x11 identification body (confirmed from a
    /// real F20 announcement, verified 2026-07-03: `DIAGADR<addr>BMWMAC<mac>BMWVIN<vin>`).
    static let vinMarker = Array("BMWVIN".utf8)

    /// True for a character allowed in a VIN (ISO 3779 excludes I, O, and Q).
    static func isVinChar(_ byte: UInt8) -> Bool {
        switch byte {
        case UInt8(ascii: "0")...UInt8(ascii: "9"),
             UInt8(ascii: "A")...UInt8(ascii: "H"),
             UInt8(ascii: "J")...UInt8(ascii: "N"),
             UInt8(ascii: "P"),
             UInt8(ascii: "R")...UInt8(ascii: "Z"):
            return true
        default:
            return false
        }
    }

    /// Extract the VIN from a 0x11 announcement datagram (header included).
    /// Mirrors crates/hsfz/src/discover.rs `scan_vin`: prefer the 17 VIN-alphabet
    /// characters immediately after the `BMWVIN` marker; fall back to the first
    /// 17-char VIN-alphabet run when the marker is absent. The marker parse avoids
    /// the false run inside the `DIAGADR…BMWMAC…` prefix — those bytes are
    /// themselves valid VIN characters, so an unanchored scan returns a wrong VIN.
    public static func scanVin(in bytes: [UInt8]) -> String? {
        // Marker-anchored: the 17 characters after "BMWVIN", if all are VIN chars.
        if bytes.count >= vinMarker.count {
            for start in 0...(bytes.count - vinMarker.count)
            where Array(bytes[start..<start + vinMarker.count]) == vinMarker {
                let vinStart = start + vinMarker.count
                let vinEnd = vinStart + vinLength
                if vinEnd <= bytes.count {
                    let candidate = Array(bytes[vinStart..<vinEnd])
                    if candidate.allSatisfy(isVinChar) {
                        return String(decoding: candidate, as: UTF8.self)
                    }
                }
                break // only the first marker is meaningful
            }
        }
        // Fallback: the first 17-character VIN-alphabet run anywhere in the body.
        guard bytes.count >= vinLength else { return nil }
        for start in 0...(bytes.count - vinLength) {
            let window = Array(bytes[start..<start + vinLength])
            if window.allSatisfy(isVinChar) {
                return String(decoding: window, as: UTF8.self)
            }
        }
        return nil
    }
}

public struct HsfzFrame: Equatable, Sendable {
    public let control: UInt16
    public let src: UInt8?
    public let tgt: UInt8?
    public let payload: [UInt8]

    public init(control: UInt16, src: UInt8?, tgt: UInt8?, payload: [UInt8]) {
        self.control = control
        self.src = src
        self.tgt = tgt
        self.payload = payload
    }
}

/// Accumulates bytes from a byte-stream (e.g. NWConnection.receive) and pops whole
/// frames as they complete. TCP is a byte stream, so frames may split across or
/// coalesce within reads — this is the single point of reassembly.
public struct FrameBuffer {
    private var bytes: [UInt8] = []
    /// Set once a decoded length exceeds the sanity cap — a misframe the caller must
    /// surface rather than wait on.
    public private(set) var isFaulted = false

    public init() {}

    public mutating func append(_ data: Data) {
        bytes.append(contentsOf: data)
    }

    /// Pop the next complete frame, or nil if one is not yet fully buffered (or faulted).
    public mutating func nextFrame() -> HsfzFrame? {
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

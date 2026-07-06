import Foundation
import Observation

/// The probe's on-screen log. Main-actor state observed by SwiftUI; `hex` is a pure
/// helper safe to call from any context (e.g. a background probe formatting bytes).
@Observable
@MainActor
final class ProbeLog {
    var lines: [String] = []

    func log(_ s: String) {
        lines.append(s)
    }

    nonisolated func hex(_ bytes: [UInt8]) -> String {
        bytes.map { String(format: "%02X", $0) }.joined(separator: " ")
    }
}

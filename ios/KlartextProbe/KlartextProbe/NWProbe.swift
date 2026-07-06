import Foundation
import Network

enum NWProbeError: Error {
    case connection(String)
    case timedOut
}

/// Opens an `NWConnection` pinned to the wired Ethernet interface — Apple's sanctioned
/// path for a link-local peer over a USB-C adapter (design spec §2.2) — sends one framed
/// UDS request, and returns the first complete HSFZ frame in reply. Bounded by `timeout`.
enum NWProbe {
    static func roundTrip(host: String, port: UInt16, uds: [UInt8],
                          timeout: Duration) async throws -> HsfzFrame {
        guard let nwPort = NWEndpoint.Port(rawValue: port) else {
            throw NWProbeError.connection("invalid port \(port)")
        }
        // Race the round-trip against a timeout; whichever finishes first wins, and the
        // loser is cancelled — which (via drive's cancellation handler) tears down the
        // connection so no continuation is left parked.
        return try await withThrowingTaskGroup(of: HsfzFrame.self) { group in
            group.addTask { try await drive(host: host, port: nwPort, uds: uds) }
            group.addTask {
                try await Task.sleep(for: timeout)
                throw NWProbeError.timedOut
            }
            defer { group.cancelAll() }
            guard let result = try await group.next() else { throw NWProbeError.timedOut }
            return result
        }
    }

    /// connect → send → receive-until-a-whole-frame. If the surrounding task is
    /// cancelled (timeout), the connection is cancelled so any parked continuation
    /// resumes rather than leaking.
    private static func drive(host: String, port: NWEndpoint.Port,
                              uds: [UInt8]) async throws -> HsfzFrame {
        let params = NWParameters.tcp
        params.requiredInterfaceType = .wiredEthernet
        // NWConnection is thread-safe (its callbacks are serialized on the queue below)
        // but is not marked Sendable; nonisolated(unsafe) lets it cross into the
        // @Sendable cancellation handler without silencing type-wide checks.
        nonisolated(unsafe) let conn = NWConnection(host: NWEndpoint.Host(host),
                                                    port: port, using: params)
        let queue = DispatchQueue(label: "ch.cherkaoui.klartext.nwprobe")

        return try await withTaskCancellationHandler {
            try await waitReady(conn, queue: queue)
            try await send(conn, Hsfz.encodeDiagnostic(src: 0xF4, tgt: 0x10, uds: uds))
            var buf = FrameBuffer()
            while true {
                try Task.checkCancellation()
                let chunk = try await receive(conn)
                buf.append(chunk)
                if let frame = buf.nextFrame() { return frame }
            }
        } onCancel: {
            conn.cancel()
        }
    }

    /// The state handler fires repeatedly; resume exactly once, then detach it so a
    /// later transition can't resume again. The handler is set BEFORE `start()` so a
    /// fast `.ready` cannot be missed.
    private static func waitReady(_ conn: NWConnection, queue: DispatchQueue) async throws {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            conn.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    conn.stateUpdateHandler = nil
                    cont.resume()
                case .failed(let error):
                    conn.stateUpdateHandler = nil
                    cont.resume(throwing: NWProbeError.connection("failed: \(error)"))
                case .waiting(let error):
                    conn.stateUpdateHandler = nil
                    cont.resume(throwing: NWProbeError.connection("waiting: \(error)"))
                case .cancelled:
                    conn.stateUpdateHandler = nil
                    cont.resume(throwing: CancellationError())
                default:
                    break // .setup / .preparing — not terminal
                }
            }
            conn.start(queue: queue)
        }
    }

    private static func send(_ conn: NWConnection, _ data: Data) async throws {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            conn.send(content: data, completion: .contentProcessed { error in
                if let error {
                    cont.resume(throwing: NWProbeError.connection("send: \(error)"))
                } else {
                    cont.resume()
                }
            })
        }
    }

    private static func receive(_ conn: NWConnection) async throws -> Data {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Data, Error>) in
            conn.receive(minimumIncompleteLength: 1, maximumLength: 4096) {
                data, _, isComplete, error in
                if let error {
                    cont.resume(throwing: NWProbeError.connection("receive: \(error)"))
                } else if let data, !data.isEmpty {
                    cont.resume(returning: data)
                } else if isComplete {
                    cont.resume(throwing: NWProbeError.connection("connection closed by peer"))
                } else {
                    cont.resume(returning: Data()) // empty read; caller loops for more
                }
            }
        }
    }
}

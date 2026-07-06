import SwiftUI
import KlartextHSFZ

/// The single probe screen: enter/cache the gateway IP, run the three tests, read the log.
struct ProbeView: View {
    @AppStorage("gatewayIP") private var gatewayIP = "192.168.17.151"
    @State private var log = ProbeLog()

    private let port: UInt16 = 6801

    var body: some View {
        NavigationStack {
            VStack(spacing: 12) {
                LabeledContent("Gateway IP") {
                    TextField("IP", text: $gatewayIP)
                        .textFieldStyle(.roundedBorder)
                        .keyboardType(.numbersAndPunctuation)
                        .autocorrectionDisabled()
                }

                HStack {
                    Button("Interfaces", action: inspectInterfaces)
                    Button("POSIX connect", action: posixConnect)
                    Button("Read VIN", action: readVIN)
                }
                .buttonStyle(.bordered)

                ScrollView {
                    Text(log.lines.joined(separator: "\n"))
                        .font(.system(.footnote, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .textSelection(.enabled)
                        .padding(8)
                }
                .background(Color(.secondarySystemBackground), in: .rect(cornerRadius: 8))
            }
            .padding()
            .navigationTitle("KlartextProbe")
        }
    }

    private func inspectInterfaces() {
        log.log("== interfaces (getifaddrs) ==")
        let ifaces = InterfaceInspector.ipv4Interfaces()
        if ifaces.isEmpty { log.log("  (none — is the adapter attached?)") }
        for i in ifaces { log.log("  \(i.name): \(i.ip)  mask \(i.netmask)") }
    }

    private func posixConnect() {
        let host = gatewayIP
        let port = port
        let log = log
        log.log("== POSIX connect \(host):\(port) (no bind, 5s) ==")
        Task.detached {
            let outcome = PosixProbe.connect(host: host, port: port, timeoutSeconds: 5)
            await MainActor.run {
                switch outcome {
                case .connected(let ms): log.log("  CONNECTED in \(ms) ms → BSD path viable")
                case .timedOut:          log.log("  TIMED OUT → commit to NWConnection")
                case .failed(let why):   log.log("  FAILED: \(why)")
                }
            }
        }
    }

    private func readVIN() {
        let host = gatewayIP
        let port = port
        let log = log
        log.log("== NWConnection VIN read \(host):\(port) (22 F1 90) ==")
        Task {
            do {
                let frame = try await NWProbe.roundTrip(
                    host: host, port: port, uds: [0x22, 0xF1, 0x90], timeout: .seconds(5))
                log.log("  RX \(log.hex(frame.payload))")
                if frame.payload.count > 3,
                   Array(frame.payload.prefix(3)) == [0x62, 0xF1, 0x90],
                   let vin = String(bytes: frame.payload.dropFirst(3), encoding: .ascii) {
                    log.log("  VIN = \(vin)  ← end-to-end OK")
                } else {
                    log.log("  connected + framed, but not a VIN response")
                }
            } catch is CancellationError {
                log.log("  cancelled")
            } catch {
                log.log("  FAILED: \(error)")
            }
        }
    }
}

#Preview {
    ProbeView()
}

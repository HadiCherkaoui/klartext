import SwiftUI

/// The single probe screen: enter/cache the gateway IP, run the three tests, read the log.
struct ProbeView: View {
    @AppStorage("gatewayIP") private var gatewayIP = "192.168.17.151"
    @State private var log = ProbeLog()

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

    // Wired incrementally in Tasks 4–6.
    private func inspectInterfaces() { log.log("— interfaces: not implemented —") }
    private func posixConnect() { log.log("— POSIX connect: not implemented —") }
    private func readVIN() { log.log("— read VIN: not implemented —") }
}

#Preview {
    ProbeView()
}

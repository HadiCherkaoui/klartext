# KlartextProbe (iOS networking feasibility probe) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Swift skills:** When implementing a Swift task, invoke the relevant installed skill first — `swiftui-pro` (views/data flow), `swift-concurrency-pro` (NWConnection/async bridging, cancellation), `swift-testing-pro` (the codec tests). Run the matching skill as a review gate before each task's commit.

**Goal:** Ship a small pure-Swift SwiftUI app that, run on the owner's iPhone against the real F-series gateway, answers whether tokio-owned BSD sockets or `Network.framework` must own the mobile transport — and proves an end-to-end HSFZ round-trip from iOS.

**Architecture:** One SwiftUI screen drives three independent probes (interface inspect, POSIX connect, `NWConnection` + HSFZ VIN round-trip) that append to a shared observable log. A pure-Swift HSFZ frame codec (mirroring `crates/hsfz/src/frame.rs`) is the only unit-testable-offline piece and is built TDD; the network probes end in manual on-car verification (matching the project's hardware-in-the-loop rule). The codec and `NWProbe` are the pieces that survive into the real app.

**Tech Stack:** Swift 6.2, SwiftUI, Observation (`@Observable`), `Network.framework` (`NWConnection`), Darwin BSD sockets (`getifaddrs`, `socket`/`connect`/`poll`), Swift Testing. No Rust, no third-party packages. Xcode 26.x, deployment target iOS 17.0.

## Global Constraints

_Every task's requirements implicitly include this section. Values copied from the spec `2026-07-06-mobile-ios-networking-probe-design.md`._

- **Pure Swift only** — no Rust, no UniFFI, no cross-compile, no third-party packages (spec §3.3).
- **Deployment target iOS 17.0**, Swift 6.2, SwiftUI; avoid UIKit unless required (`swiftui-pro`). `@Observable` (Observation) for state, not `ObservableObject`.
- **`NSLocalNetworkUsageDescription` MUST be in Info.plist** or iOS silently blocks all local-network connections — over wired Ethernet too (spec §2.2).
- **Test 3 transport = `NWConnection` with `requiredInterfaceType = .wiredEthernet`** (spec §2.2).
- **Discovery = manual gateway IP, cached in `UserDefaults`.** No broadcast, no ARP (spec §2.3). Default the field to `192.168.17.151` (the capture's static IP, `protocol-reference.md:142`); the owner overrides it.
- **HSFZ frame (from `crates/hsfz/src/frame.rs`, `protocol-reference.md` §2.1):** `[LENGTH:u32 BE][CONTROL:u16 BE][SRC:u8][TGT:u8][UDS…]`, where `LENGTH = 2 + len(UDS)` (SRC+TGT+UDS; the control word is NOT counted). Diagnostic control word = `0x0001`. Tester (SRC) = `0xF4`, central gateway (TGT) = `0x10`, TCP port `6801`. **No routing activation** — send UDS immediately after TCP connect (`protocol-reference.md` §2.5.4).
- **VIN read:** UDS `22 F1 90` → response `62 F1 90` + 17 ASCII VIN bytes.
- **Unit-test framing offline; the on-car round-trip is a MANUAL step.** Never claim a hardware round-trip works until the owner runs it (CLAUDE.md hardware-in-the-loop).
- **Execution branch:** `feat/mobile-ios-probe` (do not commit to `main`).

---

## File Structure

```
ios/
  .gitignore                      # Xcode/DerivedData cruft
  KlartextProbe/
    KlartextProbe.xcodeproj/      # created by the Xcode wizard (Task 1)
    KlartextProbe/
      KlartextProbeApp.swift      # @main app entry (Task 1)
      Hsfz.swift                  # codec: encodeDiagnostic + HsfzFrame + FrameBuffer (Task 2)
      ProbeLog.swift              # @Observable log model (Task 3)
      ProbeView.swift             # the single screen: IP field, buttons, log (Task 3, extended 4–6)
      InterfaceInspector.swift    # getifaddrs wrapper — Test 1 (Task 4)
      PosixProbe.swift            # BSD connect(), no bind, timed — Test 2 (Task 5)
      NWProbe.swift               # NWConnection(.wiredEthernet) + round-trip — Test 3 (Task 6)
    KlartextProbeTests/
      HsfzCodecTests.swift        # Swift Testing, offline (Task 2)
```

Each file has one responsibility. `Hsfz.swift` holds the three tightly-coupled codec types as one cohesive unit (a reviewer would not split a frame codec); everything else is one type per file per `swiftui-pro`.

---

## Task 1: Project scaffold, Info.plist, gitignore

**Files:**
- Create (Xcode wizard): `ios/KlartextProbe/KlartextProbe.xcodeproj` + `KlartextProbeApp.swift`
- Create: `ios/.gitignore`
- Modify: the target's Info settings (add `NSLocalNetworkUsageDescription`)

**Interfaces:**
- Consumes: nothing.
- Produces: a buildable, launchable iOS app named `KlartextProbe` with a Swift Testing test target `KlartextProbeTests`, iOS 17.0 deployment, and the local-network usage string set.

- [ ] **Step 1: Create the Xcode project (manual, on the macOS VM)**

In Xcode: File → New → Project → iOS → App. Set:
- Product Name: `KlartextProbe`
- Interface: **SwiftUI**, Language: **Swift**
- Storage: **None**, **Include Tests: ON** (creates the Swift Testing bundle)
- Save into `ios/KlartextProbe/` inside the repo.

Then in the target's **General** tab set **Minimum Deployments → iOS 17.0**, and in **Build Settings** set **Swift Language Version → 6** and **Strict Concurrency Checking → Complete**.

- [ ] **Step 2: Add the local-network usage string**

Target → **Info** tab → add a row: Key `Privacy - Local Network Usage Description` (`NSLocalNetworkUsageDescription`), Value:
```
KlartextProbe connects to your car's diagnostic gateway over the USB-C Ethernet link.
```

- [ ] **Step 3: Replace the generated app entry**

Replace `KlartextProbe/KlartextProbeApp.swift` with:

```swift
import SwiftUI

@main
struct KlartextProbeApp: App {
    var body: some Scene {
        WindowGroup {
            Text("KlartextProbe")
                .font(.title2)
                .padding()
        }
    }
}
```

- [ ] **Step 4: Create `ios/.gitignore`**

```gitignore
# Xcode
DerivedData/
build/
*.xcuserstate
xcuserdata/
*.xcscmblueprint
.DS_Store
```

- [ ] **Step 5: Build & launch to verify the scaffold**

Run (adjust the simulator name to one you have — `xcrun simctl list devices` shows them):
```bash
cd ios/KlartextProbe
xcodebuild -scheme KlartextProbe -destination 'platform=iOS Simulator,name=iPhone 16' build
```
Expected: `** BUILD SUCCEEDED **`. Launching in the simulator shows "KlartextProbe".

- [ ] **Step 6: Commit**

```bash
git checkout -b feat/mobile-ios-probe
git add ios/
git commit -m "feat(ios-probe): scaffold KlartextProbe app + local-network usage string"
```

---

## Task 2: HSFZ codec (TDD, offline)

**Files:**
- Create: `ios/KlartextProbe/KlartextProbe/Hsfz.swift`
- Test: `ios/KlartextProbe/KlartextProbeTests/HsfzCodecTests.swift`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `enum Hsfz { static func encodeDiagnostic(src: UInt8, tgt: UInt8, uds: [UInt8]) -> Data }`
  - `struct HsfzFrame: Equatable { let control: UInt16; let src: UInt8?; let tgt: UInt8?; let payload: [UInt8] }`
  - `struct FrameBuffer { mutating func append(_ data: Data); mutating func nextFrame() -> HsfzFrame? }`

- [ ] **Step 1: Write the failing tests**

Create `KlartextProbeTests/HsfzCodecTests.swift`:

```swift
import Testing
import Foundation
@testable import KlartextProbe

struct HsfzCodecTests {
    // Byte vectors shared verbatim with crates/hsfz/src/frame.rs tests.

    @Test func encodesTesterPresentToGateway() {
        let out = Hsfz.encodeDiagnostic(src: 0xF4, tgt: 0x10, uds: [0x3E, 0x00])
        #expect(Array(out) == [0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0xF4, 0x10, 0x3E, 0x00])
    }

    @Test func encodesVinRequestToGateway() {
        let out = Hsfz.encodeDiagnostic(src: 0xF4, tgt: 0x10, uds: [0x22, 0xF1, 0x90])
        // LENGTH = 2 (src+tgt) + 3 (uds) = 5
        #expect(Array(out) == [0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0xF4, 0x10, 0x22, 0xF1, 0x90])
    }

    @Test func decodesAWholeVinResponse() {
        // 62 F1 90 + "WBA3B5C50EK123456" (17 bytes) => LENGTH = 2 + 3 + 17 = 0x16
        var bytes: [UInt8] = [0x00, 0x00, 0x00, 0x16, 0x00, 0x01, 0x10, 0xF4, 0x62, 0xF1, 0x90]
        bytes += Array("WBA3B5C50EK123456".utf8)
        var buf = FrameBuffer()
        buf.append(Data(bytes))
        let frame = buf.nextFrame()
        #expect(frame?.src == 0x10)
        #expect(frame?.tgt == 0xF4)
        #expect(frame?.payload.prefix(3) == [0x62, 0xF1, 0x90])
        #expect(buf.nextFrame() == nil) // only one frame present
    }

    @Test func reassemblesAcrossTwoChunks() {
        var bytes: [UInt8] = [0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x10, 0xF4, 0x7E, 0x00]
        var buf = FrameBuffer()
        buf.append(Data(bytes[0..<5]))          // partial: header not complete
        #expect(buf.nextFrame() == nil)
        buf.append(Data(bytes[5...]))           // rest arrives
        #expect(buf.nextFrame() == HsfzFrame(control: 0x0001, src: 0x10, tgt: 0xF4, payload: [0x7E, 0x00]))
    }

    @Test func rejectsAnOversizedLength() {
        // LENGTH = 0xFFFFFFFF must be rejected as a misframe, not buffered forever.
        var buf = FrameBuffer()
        buf.append(Data([0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x01]))
        #expect(buf.nextFrame() == nil)
        #expect(buf.isFaulted == true)
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cd ios/KlartextProbe
xcodebuild test -scheme KlartextProbe -destination 'platform=iOS Simulator,name=iPhone 16' -only-testing:KlartextProbeTests/HsfzCodecTests
```
Expected: FAIL — `cannot find 'Hsfz' in scope` / `FrameBuffer`.

- [ ] **Step 3: Implement the codec**

Create `KlartextProbe/Hsfz.swift`:

```swift
import Foundation

/// HSFZ frame encode/decode — pure, no I/O. Mirrors crates/hsfz/src/frame.rs.
/// Wire: [LENGTH u32 BE][CONTROL u16 BE][SRC][TGT][UDS], LENGTH = 2 + len(UDS).
enum Hsfz {
    static let controlDiagnostic: UInt16 = 0x0001
    static let headerLen = 6
    static let maxFrameLen: UInt32 = 64 * 1024

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

/// Accumulates bytes from a byte-stream (NWConnection.receive) and pops whole frames.
struct FrameBuffer {
    private var bytes: [UInt8] = []
    /// Set once a length exceeds the sanity cap — a misframe the caller must surface.
    private(set) var isFaulted = false

    mutating func append(_ data: Data) { bytes.append(contentsOf: data) }

    mutating func nextFrame() -> HsfzFrame? {
        guard !isFaulted, bytes.count >= Hsfz.headerLen else { return nil }
        let length = (UInt32(bytes[0]) << 24) | (UInt32(bytes[1]) << 16)
                   | (UInt32(bytes[2]) << 8) | UInt32(bytes[3])
        if length > Hsfz.maxFrameLen { isFaulted = true; return nil }
        let control = (UInt16(bytes[4]) << 8) | UInt16(bytes[5])
        let total = Hsfz.headerLen + Int(length)
        guard bytes.count >= total else { return nil } // wait for more
        let body = Array(bytes[Hsfz.headerLen..<total])
        bytes.removeFirst(total)

        let carriesAddrs = (control == Hsfz.controlDiagnostic || control == 0x0002)
        if carriesAddrs, body.count >= 2 {
            return HsfzFrame(control: control, src: body[0], tgt: body[1], payload: Array(body[2...]))
        }
        return HsfzFrame(control: control, src: nil, tgt: nil, payload: body)
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
xcodebuild test -scheme KlartextProbe -destination 'platform=iOS Simulator,name=iPhone 16' -only-testing:KlartextProbeTests/HsfzCodecTests
```
Expected: PASS — 5 tests.

- [ ] **Step 5: Commit**

```bash
git add ios/KlartextProbe/KlartextProbe/Hsfz.swift ios/KlartextProbe/KlartextProbeTests/HsfzCodecTests.swift
git commit -m "feat(ios-probe): HSFZ frame codec with offline tests mirroring the Rust crate"
```

---

## Task 3: Observable log + ProbeView skeleton with cached IP

**Files:**
- Create: `ios/KlartextProbe/KlartextProbe/ProbeLog.swift`
- Create: `ios/KlartextProbe/KlartextProbe/ProbeView.swift`
- Modify: `ios/KlartextProbe/KlartextProbe/KlartextProbeApp.swift`
- Test: `ios/KlartextProbe/KlartextProbeTests/HsfzCodecTests.swift` (add a `ProbeLog` suite)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `@Observable final class ProbeLog { var lines: [String]; func log(_ s: String); func hex(_ bytes: [UInt8]) -> String }`
  - `struct ProbeView: View` with a `UserDefaults`-persisted `gatewayIP` field and an empty results log. Buttons are wired empty here and filled in Tasks 4–6.

- [ ] **Step 1: Write the failing test for the log helper**

Add to `HsfzCodecTests.swift`:

```swift
struct ProbeLogTests {
    @Test func formatsHexUppercaseSpaced() {
        let log = ProbeLog()
        #expect(log.hex([0x62, 0xF1, 0x90, 0x0A]) == "62 F1 90 0A")
    }

    @Test func appendsLines() {
        let log = ProbeLog()
        log.log("a"); log.log("b")
        #expect(log.lines == ["a", "b"])
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run:
```bash
xcodebuild test -scheme KlartextProbe -destination 'platform=iOS Simulator,name=iPhone 16' -only-testing:KlartextProbeTests/ProbeLogTests
```
Expected: FAIL — `cannot find 'ProbeLog' in scope`.

- [ ] **Step 3: Implement `ProbeLog`**

Create `KlartextProbe/ProbeLog.swift`:

```swift
import Foundation
import Observation

@Observable
final class ProbeLog {
    var lines: [String] = []

    func log(_ s: String) { lines.append(s) }

    func hex(_ bytes: [UInt8]) -> String {
        bytes.map { String(format: "%02X", $0) }.joined(separator: " ")
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run:
```bash
xcodebuild test -scheme KlartextProbe -destination 'platform=iOS Simulator,name=iPhone 16' -only-testing:KlartextProbeTests/ProbeLogTests
```
Expected: PASS — 2 tests.

- [ ] **Step 5: Implement the `ProbeView` skeleton**

Create `KlartextProbe/ProbeView.swift`:

```swift
import SwiftUI

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
                }
                .background(Color(.secondarySystemBackground))
            }
            .padding()
            .navigationTitle("KlartextProbe")
        }
    }

    // Filled in Tasks 4–6.
    private func inspectInterfaces() { log.log("— interfaces: not implemented —") }
    private func posixConnect() { log.log("— POSIX connect: not implemented —") }
    private func readVIN() { log.log("— read VIN: not implemented —") }
}

#Preview { ProbeView() }
```

- [ ] **Step 6: Show `ProbeView` from the app entry**

Replace the body of `KlartextProbeApp.swift`'s `WindowGroup` with `ProbeView()`:

```swift
import SwiftUI

@main
struct KlartextProbeApp: App {
    var body: some Scene {
        WindowGroup {
            ProbeView()
        }
    }
}
```

- [ ] **Step 7: Build to verify the UI compiles**

Run:
```bash
xcodebuild -scheme KlartextProbe -destination 'platform=iOS Simulator,name=iPhone 16' build
```
Expected: `** BUILD SUCCEEDED **`. In the simulator, the IP field shows `192.168.17.151`, persists across relaunch, and the three buttons log their "not implemented" lines.

- [ ] **Step 8: Commit**

```bash
git add ios/KlartextProbe/KlartextProbe/ProbeLog.swift ios/KlartextProbe/KlartextProbe/ProbeView.swift ios/KlartextProbe/KlartextProbe/KlartextProbeApp.swift ios/KlartextProbe/KlartextProbeTests/HsfzCodecTests.swift
git commit -m "feat(ios-probe): observable log + ProbeView skeleton with cached gateway IP"
```

---

## Task 4: InterfaceInspector (Test 1)

**Files:**
- Create: `ios/KlartextProbe/KlartextProbe/InterfaceInspector.swift`
- Modify: `ios/KlartextProbe/KlartextProbe/ProbeView.swift` (wire the "Interfaces" button)

**Interfaces:**
- Consumes: `ProbeLog`.
- Produces: `enum InterfaceInspector { static func ipv4Interfaces() -> [(name: String, ip: String, netmask: String)] }`.

- [ ] **Step 1: Implement `InterfaceInspector`**

Create `KlartextProbe/InterfaceInspector.swift`:

```swift
import Foundation
import Darwin

/// Lists this device's IPv4 interfaces + addresses via getifaddrs (supported on iOS).
/// Shows the peer's IP is NOT here — only our own addresses (see spec §2.3).
enum InterfaceInspector {
    static func ipv4Interfaces() -> [(name: String, ip: String, netmask: String)] {
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return [] }
        defer { freeifaddrs(head) }

        var result: [(String, String, String)] = []
        var ptr: UnsafeMutablePointer<ifaddrs>? = first
        while let cur = ptr {
            defer { ptr = cur.pointee.ifa_next }
            guard let sa = cur.pointee.ifa_addr, sa.pointee.sa_family == UInt8(AF_INET) else { continue }
            let name = String(cString: cur.pointee.ifa_name)
            let ip = Self.addr(cur.pointee.ifa_addr)
            let mask = Self.addr(cur.pointee.ifa_netmask)
            result.append((name, ip, mask))
        }
        return result
    }

    private static func addr(_ sa: UnsafeMutablePointer<sockaddr>?) -> String {
        guard let sa else { return "" }
        var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
        let r = getnameinfo(sa, socklen_t(sa.pointee.sa_len),
                            &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST)
        return r == 0 ? String(cString: host) : ""
    }
}
```

- [ ] **Step 2: Wire the "Interfaces" button**

In `ProbeView.swift` replace `inspectInterfaces()`:

```swift
    private func inspectInterfaces() {
        log.log("== interfaces (getifaddrs) ==")
        let ifaces = InterfaceInspector.ipv4Interfaces()
        if ifaces.isEmpty { log.log("  (none — is the adapter attached?)") }
        for i in ifaces { log.log("  \(i.name): \(i.ip)  mask \(i.netmask)") }
    }
```

- [ ] **Step 3: Build to verify it compiles**

Run:
```bash
xcodebuild -scheme KlartextProbe -destination 'platform=iOS Simulator,name=iPhone 16' build
```
Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 4: Manual on-device verification (Test 1)**

Deploy to the iPhone (Xcode → Run on device; see spec §3.4 for the usbfluxd bridge). With the USB-C Ethernet adapter attached and a static/APIPA IP assigned, tap **Interfaces**.
Expected: a line for the wired interface (an `enX`) with a `192.168.17.x` (or `169.254.x.x`) address. Record which — it decides the static-IP recommendation (spec §2.3). **This is a manual observation, not an automated test.**

- [ ] **Step 5: Commit**

```bash
git add ios/KlartextProbe/KlartextProbe/InterfaceInspector.swift ios/KlartextProbe/KlartextProbe/ProbeView.swift
git commit -m "feat(ios-probe): interface inspector (Test 1) via getifaddrs"
```

---

## Task 5: PosixProbe (Test 2 — the load-bearing socket test)

**Files:**
- Create: `ios/KlartextProbe/KlartextProbe/PosixProbe.swift`
- Modify: `ios/KlartextProbe/KlartextProbe/ProbeView.swift` (wire the "POSIX connect" button)

**Interfaces:**
- Consumes: `ProbeLog`.
- Produces: `enum PosixProbe { enum Outcome { case connected(ms: Int); case timedOut; case failed(String) }; static func connect(host: String, port: UInt16, timeoutSeconds: Int) -> Outcome }`.

- [ ] **Step 1: Implement `PosixProbe`**

Create `KlartextProbe/PosixProbe.swift`. Non-blocking `connect()` + `poll()` so the connect is time-bounded (a blocking connect can hang far past any UI timeout):

```swift
import Foundation
import Darwin

/// Plain BSD-socket TCP connect with NO interface bind — the exact path tokio uses.
/// If this reaches a link-local gateway with cellular up, the sans-I/O refactor can be
/// deferred; if it times out, we commit to NWConnection (spec §2.2, §4).
enum PosixProbe {
    enum Outcome: Equatable { case connected(ms: Int), timedOut, failed(String) }

    static func connect(host: String, port: UInt16, timeoutSeconds: Int) -> Outcome {
        let fd = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP)
        guard fd >= 0 else { return .failed("socket() errno \(errno)") }
        defer { close(fd) }

        // Non-blocking so connect() returns immediately with EINPROGRESS.
        let flags = fcntl(fd, F_GETFL, 0)
        _ = fcntl(fd, F_SETFL, flags | O_NONBLOCK)

        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        guard inet_pton(AF_INET, host, &addr.sin_addr) == 1 else { return .failed("bad IP \(host)") }

        let start = Date()
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        if rc == 0 { return .connected(ms: 0) }              // immediate (unlikely)
        if errno != EINPROGRESS { return .failed("connect() errno \(errno)") }

        var pfd = pollfd(fd: fd, events: Int16(POLLOUT), revents: 0)
        let ready = poll(&pfd, 1, Int32(timeoutSeconds * 1000))
        if ready == 0 { return .timedOut }
        if ready < 0 { return .failed("poll() errno \(errno)") }

        // Writable — check SO_ERROR to distinguish success from refused/unreachable.
        var soErr: Int32 = 0
        var len = socklen_t(MemoryLayout<Int32>.size)
        getsockopt(fd, SOL_SOCKET, SO_ERROR, &soErr, &len)
        if soErr != 0 { return .failed("SO_ERROR \(soErr)") }
        return .connected(ms: Int(Date().timeIntervalSince(start) * 1000))
    }
}
```

- [ ] **Step 2: Wire the "POSIX connect" button (off the main thread)**

In `ProbeView.swift` replace `posixConnect()`:

```swift
    private func posixConnect() {
        let host = gatewayIP, port = self.port
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
```

- [ ] **Step 3: Build to verify it compiles**

Run:
```bash
xcodebuild -scheme KlartextProbe -destination 'platform=iOS Simulator,name=iPhone 16' build
```
Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 4: Manual on-device verification (Test 2)**

On the iPhone, adapter attached, **cellular left ON**, gateway IP set. Tap **POSIX connect**.
Expected outcome is the decision (spec §4): `CONNECTED` → the BSD/tokio path works, sans-I/O refactor deferrable; `TIMED OUT` → confirms the wrong-source-IP bug, commit to NWConnection. **Record the result — this is the load-bearing observation.** Manual, not automated.

- [ ] **Step 5: Commit**

```bash
git add ios/KlartextProbe/KlartextProbe/PosixProbe.swift ios/KlartextProbe/KlartextProbe/ProbeView.swift
git commit -m "feat(ios-probe): POSIX connect probe (Test 2), the tokio-socket decision"
```

---

## Task 6: NWProbe + HSFZ VIN round-trip (Test 3 — end-to-end proof)

**Files:**
- Create: `ios/KlartextProbe/KlartextProbe/NWProbe.swift`
- Modify: `ios/KlartextProbe/KlartextProbe/ProbeView.swift` (wire the "Read VIN" button)

**Interfaces:**
- Consumes: `Hsfz`, `HsfzFrame`, `FrameBuffer`, `ProbeLog`.
- Produces: `actor NWProbe { func roundTrip(host: String, port: UInt16, uds: [UInt8], timeout: Duration) async throws -> HsfzFrame }` and `enum NWProbeError: Error { case connection(String), timedOut }`.

- [ ] **Step 1: Implement `NWProbe`**

Create `KlartextProbe/NWProbe.swift`. `NWConnection` pinned to wired Ethernet; connect/send/receive bridged to async via checked continuations (see `swift-concurrency-pro` `references/bridging.md`), reassembly via `FrameBuffer`:

```swift
import Foundation
import Network

enum NWProbeError: Error { case connection(String), timedOut }

/// NWConnection pinned to the wired Ethernet interface — Apple's sanctioned path for a
/// link-local peer over a USB-C adapter (spec §2.2). Sends one framed UDS request and
/// returns the first complete HSFZ frame in reply.
actor NWProbe {
    func roundTrip(host: String, port: UInt16, uds: [UInt8], timeout: Duration) async throws -> HsfzFrame {
        let params = NWParameters.tcp
        params.requiredInterfaceType = .wiredEthernet
        let conn = NWConnection(
            host: .init(host),
            port: .init(rawValue: port)!,
            using: params
        )

        return try await withThrowingTaskGroup(of: HsfzFrame.self) { group in
            group.addTask { try await Self.drive(conn, uds: uds) }
            group.addTask {
                try await Task.sleep(for: timeout)
                throw NWProbeError.timedOut
            }
            defer { group.cancelAll(); conn.cancel() }
            let frame = try await group.next()!
            return frame
        }
    }

    private static func drive(_ conn: NWConnection, uds: [UInt8]) async throws -> HsfzFrame {
        try await connect(conn)
        try await send(conn, Hsfz.encodeDiagnostic(src: 0xF4, tgt: 0x10, uds: uds))
        var buf = FrameBuffer()
        while true {
            let chunk = try await receive(conn)
            buf.append(chunk)
            if let frame = buf.nextFrame() { return frame }
        }
    }

    private static func connect(_ conn: NWConnection) async throws {
        try await withCheckedThrowingContinuation { (c: CheckedContinuation<Void, Error>) in
            conn.stateUpdateHandler = { state in
                switch state {
                case .ready: c.resume()
                case .failed(let e), .waiting(let e): c.resume(throwing: NWProbeError.connection("\(e)"))
                default: break
                }
            }
            conn.start(queue: .global())
        }
    }

    private static func send(_ conn: NWConnection, _ data: Data) async throws {
        try await withCheckedThrowingContinuation { (c: CheckedContinuation<Void, Error>) in
            conn.send(content: data, completion: .contentProcessed { err in
                if let err { c.resume(throwing: NWProbeError.connection("\(err)")) } else { c.resume() }
            })
        }
    }

    private static func receive(_ conn: NWConnection) async throws -> Data {
        try await withCheckedThrowingContinuation { (c: CheckedContinuation<Data, Error>) in
            conn.receive(minimumIncompleteLength: 1, maximumLength: 4096) { data, _, isComplete, err in
                if let err { c.resume(throwing: NWProbeError.connection("\(err)")); return }
                if let data, !data.isEmpty { c.resume(returning: data); return }
                if isComplete { c.resume(throwing: NWProbeError.connection("closed")); return }
                c.resume(returning: Data())
            }
        }
    }
}
```

- [ ] **Step 2: Wire the "Read VIN" button**

In `ProbeView.swift` replace `readVIN()`:

```swift
    private func readVIN() {
        let host = gatewayIP, port = self.port
        log.log("== NWConnection VIN read \(host):\(port) (22 F1 90) ==")
        Task {
            do {
                let frame = try await NWProbe().roundTrip(
                    host: host, port: port, uds: [0x22, 0xF1, 0x90], timeout: .seconds(5))
                log.log("  RX \(log.hex(frame.payload))")
                if frame.payload.count > 3, Array(frame.payload.prefix(3)) == [0x62, 0xF1, 0x90],
                   let vin = String(bytes: frame.payload.dropFirst(3), encoding: .ascii) {
                    log.log("  VIN = \(vin)  ← end-to-end OK")
                } else {
                    log.log("  connected + framed, but not a VIN response")
                }
            } catch {
                log.log("  FAILED: \(error)")
            }
        }
    }
```

- [ ] **Step 3: Build to verify it compiles**

Run:
```bash
xcodebuild -scheme KlartextProbe -destination 'platform=iOS Simulator,name=iPhone 16' build
```
Expected: `** BUILD SUCCEEDED **`.

- [ ] **Step 4: Manual on-device verification (Test 3 — the payoff)**

On the iPhone, adapter attached to the car, gateway IP set, grant the local-network prompt on first run. Tap **Read VIN**.
Expected: `VIN = <your 17-char VIN> ← end-to-end OK`. That proves the phone speaks HSFZ to the car over `NWConnection`, and validates the Swift codec against the real gateway. **Manual observation — the probe's headline success signal.**

- [ ] **Step 5: Commit**

```bash
git add ios/KlartextProbe/KlartextProbe/NWProbe.swift ios/KlartextProbe/KlartextProbe/ProbeView.swift
git commit -m "feat(ios-probe): NWConnection HSFZ VIN round-trip (Test 3), end-to-end proof"
```

---

## Self-Review

**1. Spec coverage:**
- §3.1 Test 1 (interface inspect) → Task 4. Test 2 (POSIX connect) → Task 5. Test 3 (NWConnection + VIN) → Task 6. ✓
- §3.2 components: `HsfzCodec` → Task 2; `InterfaceInspector` → Task 4; `PosixProbe` → Task 5; `NWProbe` → Task 6; `ProbeView` → Task 3 (extended 4–6); `NSLocalNetworkUsageDescription` → Task 1. ✓
- §3.3 YAGNI: no Rust/UniFFI/discovery/session — none appear. ✓
- §3.4 location `ios/KlartextProbe/`, gitignore, deploy loop → Task 1 + manual steps. ✓
- §3.5 testing: codec offline (Task 2), network tests manual on-device (Tasks 4–6 Step 4). ✓
- §2.3 static-IP default `192.168.17.151`, `UserDefaults` cache → Task 3 (`@AppStorage`). ✓
- §2.2 `.wiredEthernet`, local-network string → Task 6 / Task 1. ✓

**2. Placeholder scan:** No "TBD"/"add error handling"/"similar to Task N". Every code step shows complete code; the Task 3 empty button bodies are explicitly replaced in Tasks 4–6, not left vague. ✓

**3. Type consistency:** `Hsfz.encodeDiagnostic(src:tgt:uds:)`, `HsfzFrame(control:src:tgt:payload:)`, `FrameBuffer.append/nextFrame/isFaulted`, `ProbeLog.log/hex/lines`, `InterfaceInspector.ipv4Interfaces()`, `PosixProbe.Outcome`/`connect(host:port:timeoutSeconds:)`, `NWProbe.roundTrip(host:port:uds:timeout:)` — names/signatures used in `ProbeView` match their definitions. `FrameBuffer.isFaulted` is used in the Task 2 test and defined in the same task. ✓

_Note on TDD honesty:_ only Task 2 and the Task 3 log helper are true red/green/refactor. Tasks 4–6 are implement-then-manually-verify-on-car, because a socket to a physical gateway cannot run in CI — this is per spec §3.5 and CLAUDE.md's hardware-in-the-loop rule, and each has an explicit expected on-device observation instead of an assertion.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-06-mobile-ios-networking-probe.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration. Note: the on-device steps (Task 1 Xcode wizard, and Tasks 4–6 Step 4 on-car verification) are yours to run on the VM/phone; subagents can write and simulator-build the code but cannot reach the car.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints for review.

Which approach?

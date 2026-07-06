# Mobile iOS diagnostics — Phase 0: networking feasibility probe (design)

**Date:** 2026-07-06 · **Status:** design approved (owner), not yet implemented.
**Milestone:** M11 item 6 — mobile iOS over USB-C Ethernet. See
`2026-07-03-m11-ista-parity-roadmap.md` §6.

This spec covers **Phase 0 only**: a small, pure-Swift SwiftUI app (`KlartextProbe`)
that answers the iOS-networking questions deciding the mobile transport architecture,
run on the owner's iPhone against the real car. The full mobile app — the UniFFI
binding, any transport refactor, the SwiftUI diagnostics UI — is a **follow-on
milestone** whose shape depends on this probe's results and is explicitly out of scope
here (§5).

---

## 1. Why a probe first

The klartext core is already mature for reads. `klartext-client` exposes
`discover_and_connect`/`connect`, `read_dtcs`, `read_fault_detail`, `read_did`,
`read_ecu_list`/`identify_vehicle`, `read_dynamic_measurement`, and the
confirmation-gated writes — all `Send`-asserted, async/tokio, and unit-tested against
loopback mocks (`crates/client`; the read-path crates build and test green,
verified 2026-07-06). So the mobile risk is **not** the Rust core, and **not** the data
(BYO-data: the owner supplies his own ISTA DB, as on desktop). The risk is entirely in
**iOS networking** — whether the phone can reach a link-local gateway over a USB-C
Ethernet adapter at all, and which socket API must own the connection. A few hundred
lines of throwaway-grade Swift retire those unknowns *before* we invest in the UniFFI
toolchain and a possible transport-layer refactor.

**Owner decisions (2026-07-06):**
- iOS is a hard requirement (not Android).
- Deploy from an **x86 macOS VM**.
- Discovery is **manual-IP-with-caching**; auto-discovery and ARP were explored and
  rejected (§2.3).
- The first deliverable is a **pure-Swift** probe — no Rust/UniFFI yet.
- The USB-C→Ethernet adapter already attaches on the owner's iPhone and takes a
  manually-assigned IP (confirmed on-device by the owner). This removes the "does the
  link even come up" unknown.

## 2. Research findings (recorded constraints)

Four questions were researched on 2026-07-06. The bottom lines are load-bearing for the
whole milestone and are recorded here so they need not be re-derived. §2.2 and §2.3
directly shape the probe; §2.1 and §2.4 inform the follow-on milestone and the deploy
setup.

### 2.1 FFI binding — UniFFI 0.32, async via a tokio attribute *(informs follow-on)*

Use **UniFFI** (Mozilla; v0.32.0, 2026-06-30; MIT) over `swift-bridge` (a solo, pre-1.0
project). Async Rust crosses to a Swift `async throws` function via
`#[uniffi::export(async_runtime = "tokio")]` — **mandatory** for our tokio TCP core, or
the future is polled with no tokio reactor and any tokio I/O panics ("no reactor
running"). Keep the FFI surface **coarse** (a few high-level calls, not per-frame
streaming). UniFFI has **no built-in cancellation** — we roll our own. Packaging:
`cargo-swift` (0.11.1) currently pins uniffi **0.31**, so either pin 0.31 or use the
manual `uniffi-bindgen-swift` + `xcodebuild -create-xcframework` path. Rust target
triples: `aarch64-apple-ios` (device), `aarch64-apple-ios-sim` (Apple-silicon
simulator). *Deferred — not needed for the probe.*

Sources: crates.io/crates/uniffi (0.32.0); mozilla.github.io/uniffi-rs
(async-overview, futures); uniffi-rs issues #2576, #2811.

### 2.2 Socket ownership — `Network.framework` owns it; Rust goes sans-I/O *(shapes the probe)*

Plain BSD/tokio `connect()` to a `169.254.x.x` link-local address is **not reliable** on
iOS: with another interface up (an iPhone always has cellular), the kernel selects the
wrong source IP and the SYN egresses the wrong interface — a documented failure (Apple
DTS, forum thread 725414, 2023). The robust, Apple-recommended path is **`NWConnection`
with `requiredInterfaceType = .wiredEthernet`**, which fixes interface/source selection.

**Consequence for the core:** `klartext-hsfz` will need a **sans-I/O split** — the frame
codec and protocol logic separated from socket I/O — so Swift's `NWConnection` can own
the socket and hand raw bytes to Rust. This is a standard pattern (the codec already
reassembles frames off a length prefix; see `crates/hsfz/src/frame.rs::read_frame`) and
matches the roadmap. **The probe's headline test (§3.1, Test 2 vs Test 3) decides whether
this refactor is required now or can be deferred.**

Two API-independent facts:
- Unicast TCP needs **no entitlement** (the multicast entitlement is only for
  broadcast/multicast).
- The iOS 14+ **local-network privacy prompt applies to wired Ethernet + link-local**,
  enforced below the socket API regardless of BSD-vs-Network.framework. `KlartextProbe`
  must ship **`NSLocalNetworkUsageDescription`** in Info.plist from the first build or
  connections are silently blocked.

Sources: Apple DTS forums 725414 (2023), 76711 (2017), 663848 (2020); TN3179
(rev. 2024-10-31); `com.apple.developer.networking.multicast` entitlement docs.

### 2.3 Discovery — manual/static IP + cache (ARP is dead on iOS) *(shapes the probe)*

Reading the ARP/neighbor table via `sysctl(CTL_NET, PF_ROUTE, …, NET_RT_FLAGS,
RTF_LLINFO)` is **sandbox-blocked on modern iOS** — it returns junk
`02:00:00:00:00:00` MACs on real devices, no entitlement unlocks it, and sideloading
does not lift the sandbox (only a jailbreak would, which is out of scope). Broadcast
discovery (HSFZ `0x11` / UDP 6811) *would* work but needs the **multicast entitlement**,
which requires a paid account + Apple approval and is unavailable on a free personal
team.

**Therefore v1 discovery = the user enters the gateway IP once, cached in
`UserDefaults`.** Robustness win, found in the owner's own capture: the car supports a
**static IP config** — DID `0x172A` returned `IP 192.168.17.151 / GW 192.168.17.1` in
the dissec.to capture (`protocol-reference.md:142`) — so running the ENET link with a
static config makes the cached IP deterministic (no APIPA staleness, which is the thing
ARP would have solved). The probe can read `0x172A` after connecting to confirm/display
the config. Still **[verify against a capture]** on the owner's own car (the DID set is
ECU/model-specific).

Sources: Apple DTS forums 741616 (2023), 84607 (2017), 799956 (2025); multicast news
0oi77447 (2020).

### 2.4 Build & deploy — free Apple ID; x86 VM needs the usbfluxd bridge *(deploy setup)*

A **free Apple ID** ("Personal Team") suffices to run your own app on your own iPhone:
7-day re-sign, Developer Mode on, no paid membership. On an **x86 macOS VM**, raw iPhone
USB passthrough is fragile (the phone re-enumerates on "Trust" and drops mid-deploy); the
reliable path is the **`usbfluxd` network bridge** — the host owns the phone over real
USB and shares `usbmuxd` to the guest over TCP, so Xcode in the VM sees the iPhone as
locally attached. (Running macOS on non-Apple hardware is an Apple EULA matter — the
owner's call.)

Sources: OSX-KVM Xcode-Tutorial + usbfluxd (#37); Corellium USBFlux; Apple "Choosing a
Membership"; xcodereleases.com (Xcode 26.x current).

## 3. The probe: `KlartextProbe`

A single-screen SwiftUI app whose entire job is to surface facts about iOS networking,
run against the real gateway.

### 3.1 Purpose & tests

Three tests, each with one job:

| # | Test | Question it answers | Decision it drives |
|---|------|---------------------|--------------------|
| 1 | **Interface inspect** (`getifaddrs`) | Is the USB-C Ethernet interface visible from a sandboxed app, and what IP did it get? | Confirms the link is usable; shows whether we're on static `192.168.17.x` or `169.254` APIPA |
| 2 | **POSIX `connect()`** (no interface bind), cellular up | Does a plain BSD socket — what tokio uses underneath — *complete a TCP connect* to the gateway, or die on the wrong-source-IP bug? (connect-level only; no framing) | **Connects → tokio can own sockets; the sans-I/O refactor can be deferred. Timeout → commit to `NWConnection` + sans-I/O core.** The load-bearing result. |
| 3 | **`NWConnection(.wiredEthernet)` + HSFZ VIN round-trip** | Does the sanctioned path work end-to-end — real frame out, VIN back? | Proves the transport we'll almost certainly keep, and validates the Swift HSFZ codec that *becomes* that transport shim |

Test 3 returning the actual VIN (`62 F1 90 <17 ASCII>`) is the unambiguous "it works"
signal. Test 2 vs Test 3 is the architecture decision.

### 3.2 Components

Five small, independently-understandable units:

- **`HsfzCodec`** (pure Swift) — `encode(src,tgt,uds) -> Data` and a streaming `decode`
  that reassembles frames off the 4-byte length prefix. Wire layout mirrors
  `crates/hsfz/src/frame.rs` exactly: `[LENGTH u32 BE][CONTROL u16 BE][SRC][TGT][UDS]`,
  where `LENGTH = 2 + len(UDS)` and control `0x0001` is diagnostic
  (`protocol-reference.md` §2.1). Gets **Swift-Testing unit tests using the same byte
  vectors as the Rust crate** (e.g. `3E 00` to gateway `0x10` encodes
  `00 00 00 04 00 01 F4 10 3E 00`), proving the port matches the Rust wire format
  offline.
- **`InterfaceInspector`** — wraps `getifaddrs`; lists interface name / IP / netmask
  (Test 1).
- **`PosixProbe`** — BSD socket, `connect()` with no interface bind, timed; reports
  connected vs timeout (Test 2).
- **`NWProbe`** — `NWConnection` pinned to `.wiredEthernet`; send/receive, frames via
  `HsfzCodec` (Test 3).
- **`ProbeView`** (SwiftUI) — gateway-IP text field (persisted to `UserDefaults` = the
  "enter once, cache" mechanism), three test buttons, and a scrolling monospaced
  hex/text log (bytes sent, bytes received, decoded VIN, timings, errors). The app *is*
  an error-surfacing tool, so "error handling" = show the exact failure verbatim.

Info.plist ships `NSLocalNetworkUsageDescription` (§2.2).

HSFZ needs **no routing activation** — `protocol-reference.md` §2.5.4: "you may send UDS
immediately after TCP connect" (unlike DoIP). So a round-trip is exactly: connect
TCP 6801 → send one framed `22 F1 90` (tester `0xF4` → gateway `0x10`) → read back
`62 F1 90 <VIN>`.

### 3.3 Deliberately out of scope (YAGNI)

No Rust, no UniFFI, no cross-compile. No UDS decoding beyond reading the VIN as ASCII (no
DTCs, no semantic DB, no scaling). No discovery (broadcast needs the paid entitlement;
ARP is dead on iOS). No session/keepalive/multi-ECU machinery. No visual polish. The only
pieces that survive into the real app are `HsfzCodec` and `NWProbe`; everything else is
scaffold.

### 3.4 Location, build & deploy

- **Location:** a new top-level `ios/` directory in the klartext repo
  (`ios/KlartextProbe/`), consistent with the "each binary in its own top-level dir"
  convention (CLAUDE.md). Xcode user-cruft (`xcuserdata/`, `*.xcuserstate`,
  `DerivedData/`) added to `.gitignore`. Kept in-repo because `HsfzCodec` is reused by
  the real transport shim.
- **Install (probe only — minimal):** in the macOS guest, **Xcode 26.x + Command Line
  Tools**; sign in with a free Apple ID (Personal Team). On the x86 host, `usbmuxd` +
  `usbfluxd` to bridge the iPhone into the VM. On the phone: enable **Developer Mode**
  and trust the Mac. **No Rust toolchain is needed for this phase.**
- **Loop:** open `ios/KlartextProbe` in Xcode → select the device + Personal Team → Run.
  Re-sign every 7 days.

### 3.5 Testing

`HsfzCodec` is unit-tested offline (Swift Testing, byte vectors shared with the Rust
crate). Tests 1–3 are inherently the **manual on-car validation** — that is the point of
the probe, and it matches the project's hardware-in-the-loop rule (CLAUDE.md): framing is
unit-tested; the wire round-trip is a manual step the owner runs. No claim of a hardware
round-trip is made until the owner runs it.

## 4. What the probe's results decide

- **Test 2 succeeds reliably (cellular up):** the BSD-socket path works over USB-C
  Ethernet; tokio can own sockets, and the `klartext-hsfz` sans-I/O refactor can be
  deferred or skipped. (Less likely, per §2.2.)
- **Test 2 times out but Test 3 succeeds:** confirms the research — commit to
  `NWConnection` owning the socket and a sans-I/O `klartext-hsfz`. (Expected.)
- **Either way:** Test 1 tells us static vs APIPA addressing (feeding the §2.3 static-IP
  recommendation), and Test 3 validates the Swift `HsfzCodec` against the real gateway —
  the codec that becomes the real transport shim.

These outcomes are the entry conditions for the follow-on milestone.

## 5. Follow-on milestone (out of scope here)

Informed by the probe: stand up the UniFFI binding (§2.1), perform the `klartext-hsfz`
sans-I/O split if Test 2 required it (§2.2), and build the SwiftUI diagnostics app on
top of `klartext-client` — connect via cached/static IP, `identify_vehicle`,
`read_faults`, live data. Its own brainstorm → spec → plan cycle.

## 6. Open questions / risks

- **Static vs APIPA on the owner's car** — DID `0x172A` is capture-derived from another
  car; needs confirming on this F20 (§2.3). Not a blocker: the probe works with whatever
  IP the interface gets; Test 1 reports it.
- **usbfluxd first-pairing** — whether the initial trust handshake is fully carried over
  the socket (no USB ever needed into the guest) is high-confidence but uncertified;
  validate during setup.
- **NWConnection framing cadence** — `receive` delivers arbitrary chunks; `NWProbe` must
  reassemble on frame boundaries via `HsfzCodec` (same contract as the Rust
  `read_frame`). Called out so it is not mistaken for one-frame-per-receive.

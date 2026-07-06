# KlartextProbe (iOS) — build & deploy from Linux with xtool

The pure-Swift iOS networking probe (see `docs/superpowers/specs/2026-07-06-mobile-ios-networking-probe-design.md`),
built, signed, and installed on a physical iPhone **from Linux — no Xcode, no macOS** —
using [xtool](https://xtool.sh).

> **Status — verified 2026-07-06:** codec tests pass on Linux (11/11); `xtool dev build`
> links the app for iOS; `xtool dev` **installed + verified** it on a physical iPhone
> (iOS 26). So build → sign → install works end-to-end without a Mac. The remaining step is
> the on-*car* test against the gateway (needs the hardware).
>
> **Home target is native Linux** (the normal path below). **Windows/WSL is a special case**
> (§ "Windows + WSL") that needs an extra workaround for a WSL USB bug.

## Layout — two SwiftPM packages
```
ios/
  KlartextHSFZ/    # pure-Foundation HSFZ codec — `swift test` runs on Linux, no device
  KlartextProbe/   # the SwiftUI app — built/installed by xtool; depends on ../KlartextHSFZ
```
Why split: `swift test` builds the whole package graph, so isolating the codec lets its
tests run on Linux without dragging in the iOS-only SwiftUI/Network app target (which can't
compile for the Linux host).

## Prerequisites (Linux)
- **Swift 6.3 toolchain** — <https://swift.org/install/linux> (swiftly is easiest). Arch:
  `swiftly`, or `swift-bin` (AUR).
- **usbmuxd** — device comms over USB. Debian/Ubuntu `sudo apt-get install usbmuxd`; Arch
  `sudo pacman -S usbmuxd`.
- **xtool** — the AppImage:
  ```bash
  curl -fL "https://github.com/xtool-org/xtool/releases/latest/download/xtool-$(uname -m).AppImage" -o xtool
  chmod +x xtool && sudo mv xtool /usr/local/bin/
  ```
  Needs FUSE (`libfuse2` Debian/Ubuntu, `fuse2` Arch), or run with `--appimage-extract-and-run`.
  **Arch AUR:** `yay -S xtool-appimage` (recommended — it *is* the official AppImage, kept
  updated). A source-build `xtool` AUR package also exists (heavier; check its PKGBUILD).
- **Xcode `.xip`** (Xcode 26) from <https://developer.apple.com/download/all> — xtool
  extracts the iOS SDK from it on Linux; you never install Xcode.
- A **free Apple ID** (paid optional).
- **Claude Code Swift skills** — nothing to install: `swiftui-pro`, `swift-concurrency-pro`,
  and `swift-testing-pro` are **vendored in-repo** at `.claude/skills/` (MIT, from the
  `twostraws/*-Agent-Skill` repos — upstream links + update recipe in
  `.claude/skills/README.md`). Claude Code picks them up automatically in this project.
  These caught the real Swift issues in this probe (`Sendable` across a `TaskGroup`,
  `#require` with a mutating method, modern SwiftUI/concurrency).

## One-time setup
```bash
xtool setup       # auth: choose "Password" for a free Apple ID; then give the Xcode .xip path
swift sdk list    # should list a "darwin" SDK
```

## Codec tests — Linux, no device
```bash
cd ios/KlartextHSFZ && swift test      # 11/11 pure-Foundation tests
```

## Build the app — no device needed
```bash
cd ios/KlartextProbe && xtool dev build   # compiles + links KlartextProbe.app for iOS
```
Use this to catch build errors before involving the phone.

## Deploy to the iPhone

### Native Linux (bare metal) — the normal path
1. Plug in the iPhone; ensure `usbmuxd` is running (`pgrep usbmuxd`; else `sudo usbmuxd` /
   start the systemd unit).
2. On the phone: tap **Trust**; enable **Developer Mode** (Settings → Privacy & Security →
   Developer Mode → reboot).
3. `cd ios/KlartextProbe && xtool dev` — build → sign → install → (attempt) launch.

### Windows + WSL — SPECIAL CASE
WSL's usbmuxd can't handle the iPhone's **>64 KB USB packets**, so `xtool dev` over `usbipd`
**hangs forever at `[Connecting] 100%`** (xtool issue #19 — confirmed by the maintainers).
**Verified workaround: route usbmux through Windows' Apple Mobile Device Service instead of
forwarding USB into WSL.**

1. **Windows:** install the **Apple Devices** app (or iTunes) from the Microsoft Store, open
   it, connect the iPhone, tap **Trust**. It runs a *working* usbmuxd on Windows at
   `127.0.0.1:27015`. Do **not** `usbipd attach` the phone — it must stay on the Windows side.
2. **Windows (admin PowerShell)** — forward that port into WSL. Find your `vEthernet (WSL…)`
   adapter IP with `ipconfig` (NAT-mode WSL; e.g. `172.29.128.1`):
   ```powershell
   New-NetFirewallRule -DisplayName "WSL-usbmux" -Direction Inbound -InterfaceAlias "vEthernet (WSL (Hyper-V firewall))" -Action Allow
   netsh interface portproxy set v4tov4 listenport=27015 connectaddress=127.0.0.1 connectport=27015 listenaddress=<vEthernet IP>
   ```
3. **WSL** — point xtool at the Windows usbmuxd:
   ```bash
   export USBMUXD_SOCKET_ADDRESS=<vEthernet IP>:27015     # persisted in ~/.bashrc on this box
   cd ios/KlartextProbe && xtool dev
   ```
   *(Mirrored-mode WSL: skip the portproxy and use `127.0.0.1:27015`. If the vEthernet IP
   changes after a reboot, redo the portproxy and this value — they must match.)*

Verified 2026-07-06: this took the install past `[Connecting]` → `[Installing]` →
`[Verifying]` 100%.

## On the phone (first launch)
- **Developer Mode** on (above), or a dev-signed app installs but refuses to launch.
- First tap → **"Untrusted Developer"** → **Settings → General → VPN & Device Management →
  your Apple ID → Trust** → tap the app.
- **7-day expiry:** free-cert apps stop launching after ~7 days; re-run `xtool dev` to
  reinstall. (Developer Mode itself does not expire.)

## On-car test (the point of the probe)
Adapter into the gateway, enter the IP, tap in order: **Interfaces** → **POSIX connect**
(cellular on) → **Read VIN** → **UDP ident**. What the first three decide is in the design
spec §3.1/§4. **UDP ident** is the discovery experiment on top: it sends the verbatim
6-byte 0x11 identification request *unicast* to `<IP>:6811` (unicast needs no multicast
entitlement — spec §2.3's wall applies only to broadcast). A reply (ideally with the VIN)
proves entitlement-free discovery: the same datagram swept across the subnet replaces the
gateway-IP field. Silence is NOT proof of absence — first confirm from the laptop what the
gateway answers to (e.g. `printf '\x00\x00\x00\x00\x00\x11' | socat -t3 - udp:<IP>:6811 | xxd`),
so you know which IP to expect and whether unicast ident is answered at all.

## Known issues / findings (2026-07-06)
- **WSL usbmuxd >64 KB packet bug** → the AMDS workaround above (xtool #19). Native Linux is
  unaffected — this is purely a WSL/usbipd artifact.
- **`xtool dev` launch/attach is immature** — after a successful install it can hang at the
  launch step (xtool can't drive a debug session yet). The **install completes regardless**;
  just tap the app. `xtool dev build` is the clean build-only command.
- **App shape xtool requires:** the app is a **`.library` product** (not `.executable`) —
  xtool synthesizes the app entry from the library that contains the `@main App`. Public
  types returned across a `TaskGroup` must be `Sendable`. `#Preview` doesn't compile (its
  macro plugin is Xcode-only). These are baked into the current sources.

## Does the FULL klartext-mobile work over this path?
The probe (pure Swift): **yes, verified.** The full app adds the **Rust core via UniFFI** —
the UniFFI-*generated Swift* is fine, but linking the compiled Rust static lib / xcframework
into the xtool SwiftPM build is **unverified** (binary targets are xtool's weak spot).
Before committing the full app to the VM-free path: cross-compile the core to
`aarch64-apple-ios` on Linux (feasible with the SDK xtool already extracts), then confirm
`xtool dev build` links a trivial UniFFI function on-device. Do that spike first.

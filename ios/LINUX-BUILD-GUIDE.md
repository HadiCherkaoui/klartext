# KlartextProbe — build & deploy from Linux/WSL with xtool

Build, sign, and install the iOS probe on your iPhone from **Linux/WSL — no Xcode, no
macOS**. Commands verified against xtool's docs (`Installation-Linux.md`, v1.17, 2026-07)
and tailored to your machine: **WSL2 Ubuntu 24.04, x86_64, iPhone on iOS 26**.

> All app config already lives in the repo: `Package.swift`, `xtool.yml`, `Info.plist`.
> You run the toolchain steps below; nothing needs editing to build.

---

## 0. One-time downloads / accounts
- A **free Apple ID** works (paid Apple Developer Program optional).
- **Xcode 26 `.xip`** — download from <https://developer.apple.com/download/all/?q=Xcode>
  (sign in with your Apple ID; ~10 GB). xtool extracts the iOS SDK from it **on Linux** —
  you never install Xcode. Note where you save it.

## 1. WSL prerequisites (run in Ubuntu)
```bash
sudo apt-get update
sudo apt-get install -y usbmuxd curl libfuse2
```
`usbmuxd` = talks to the iPhone over USB; `libfuse2` = lets the xtool AppImage run.

## 2. Swift 6.3 toolchain
Install per <https://swift.org/install/linux> (swiftly is easiest):
```bash
curl -O https://download.swift.org/swiftly/linux/swiftly-$(uname -m).tar.gz
tar zxf swiftly-$(uname -m).tar.gz
./swiftly init          # follow prompts, then open a fresh shell
swiftly install latest
swift --version         # expect Swift 6.3.x
```

## 3. Install xtool (AppImage)
```bash
curl -fL "https://github.com/xtool-org/xtool/releases/latest/download/xtool-$(uname -m).AppImage" -o xtool
chmod +x xtool
sudo mv xtool /usr/local/bin/
xtool --version
```

## 4. xtool setup (Apple auth + iOS SDK)
```bash
xtool setup
```
- Authentication: choose **Password** (works with any/free Apple ID); API Key is for paid
  accounts.
- SDK: when prompted, give the path to the Xcode `.xip`, e.g. `~/Downloads/Xcode_26.xip`.

Verify the SDK registered:
```bash
swift sdk list          # should list a Darwin/iOS SDK
```

## 5. Sanity check WITHOUT the phone — run the codec tests on Linux
```bash
cd /mnt/c/CMI-Github/klartext/ios/KlartextProbe
swift test
```
Expect **5 passing** `KlartextHSFZTests` (pure-Foundation codec — builds natively on
Linux). If `swift test` tries to compile the SwiftUI app target and errors on Linux, run
`swift test --filter KlartextHSFZTests`. *(Tip: building under `/mnt/c` is slow — for
faster iteration, `git clone` the repo into your WSL home instead.)*

## 6. USB passthrough into WSL (skip on native Linux)
On **Windows** (admin PowerShell), one-time install:
```powershell
winget install --exact dorssel.usbipd-win
```
Every session, iPhone plugged in:
```powershell
usbipd list                          # note the iPhone's BUSID, e.g. 2-4
usbipd bind --busid <BUSID>          # one-time per device (admin)
usbipd attach --wsl --busid <BUSID>  # re-run after each replug
```
Then in WSL the device is visible to `usbmuxd`. On the iPhone: tap **Trust This Computer**,
and enable **Settings → Privacy & Security → Developer Mode** (reboots the phone).

## 7. 🔌 Build, sign & deploy — plug the phone in now
```bash
cd /mnt/c/CMI-Github/klartext/ios/KlartextProbe
xtool dev
```
Builds → signs (free-account cert) → installs → launches on the iPhone. **On first launch,
allow the Local Network permission prompt**, or every connection is silently blocked.

## 8. On-car tests
Attach the USB-C→Ethernet adapter to the car, enter the gateway IP in the app, then:
1. **Interfaces** — confirms the wired interface is up + its IP (static `192.168.17.x` or
   APIPA `169.254.x.x`).
2. **POSIX connect** (leave **cellular ON**) — *connects* → tokio/BSD sockets are viable;
   *times out* → we must use `NWConnection` (the app already does for Read VIN).
3. **Read VIN** — your 17-char VIN back = HSFZ round-trip over iOS works end-to-end.

---

## Troubleshooting
- **7-day expiry (free account):** the signed app stops launching after 7 days — re-run
  `xtool dev` to reinstall.
- **AppImage won't start:** `sudo apt-get install -y libfuse2`, or run
  `xtool --appimage-extract-and-run <args>`.
- **Device won't attach / xtool can't see it:** re-plug and re-run `usbipd attach`; make
  sure `usbmuxd` is running (`sudo usbmuxd -f` in a spare shell); confirm you tapped
  **Trust** and enabled Developer Mode. This usbipd→usbmuxd hop is the finickiest step.
- **Strict-concurrency build errors:** if the compiler flags isolation on the probe
  methods, add `@MainActor` to `ProbeView` (`Sources/KlartextProbe/ProbeView.swift`).
  `NWConnection` is already handled with `nonisolated(unsafe)`.
- **xtool rejects `xtool.yml`:** cross-check keys against a throwaway `xtool new` — schema
  is `version` + `bundleID` (required), `infoPath`/`iconPath`/`entitlementsPath`/`resources`
  (optional).

## Environment notes (your machine, observed 2026-07-06)
- WSL distros present: `Ubuntu` (default), `archlinux`, `docker-desktop`.
- Docker Desktop is installed but its **Linux engine wasn't running** and **WSL
  integration for Ubuntu was off** — if you'd rather use Docker for `swift test`
  (`docker run --rm -v "$PWD":/pkg -w /pkg swift:6 swift test`), enable it in Docker
  Desktop → Settings → Resources → WSL Integration.
- WSL `sudo` needs your password (fine when you run it interactively).

## Does the FULL klartext-mobile work over this path?
The **probe: yes**. The **full app** (Rust core via UniFFI + SwiftUI): the SwiftUI and the
UniFFI-generated Swift are fine; the open question is **linking the compiled Rust core**
(a binary/static-lib target) into the xtool SwiftPM build — the one area xtool is weakest.
Cross-compile the Rust core to `aarch64-apple-ios` on Linux (feasible with the SDK xtool
extracted), then confirm `xtool dev` links it with a small UniFFI spike **before**
committing the whole app to the VM-free path.

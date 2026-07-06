# KlartextProbe (iOS) — build from Linux/WSL with xtool

A pure-Swift SwiftUI networking probe that validates iOS↔car connectivity over USB-C
Ethernet (see `docs/superpowers/specs/2026-07-06-mobile-ios-networking-probe-design.md`).
Built and deployed **without Xcode or macOS** using [xtool](https://xtool.sh).

## Layout (SwiftPM)

```
ios/KlartextProbe/
  Package.swift                     # SPM package: iOS 17 min, 3 targets
  xtool.yml                         # app manifest (bundleID, product, Info.plist)
  Info.plist                        # NSLocalNetworkUsageDescription (required!)
  Sources/KlartextHSFZ/             # pure-Foundation HSFZ codec — Linux-testable
  Sources/KlartextProbe/            # the SwiftUI app (@main, views, probes)
  Tests/KlartextHSFZTests/          # codec tests — run on Linux
```

## One-time setup (Linux / WSL)

1. Install a Swift 6 toolchain (swift.org) and [xtool](https://xtool.sh) (see its README
   for the current install — Docker image or prebuilt binary).
2. `xtool setup` — authenticates your **Apple ID** (a **free** Apple ID works; a paid
   Developer Program account is optional) and extracts the iOS SDK from an **`Xcode.xip`**
   you download from Apple (one-time, ~10 GB; processed on Linux, no macOS needed).
3. **WSL only:** attach the iPhone to WSL with `usbipd-win` (`usbipd attach --wsl --busid …`).
   Native Linux: just have `usbmuxd` running. Enable **Developer Mode** on the phone
   (Settings → Privacy & Security → Developer Mode) — this iPhone runs iOS 26.

## Run the codec tests (no phone, no SDK — pure Linux)

```bash
cd ios/KlartextProbe
swift test          # builds KlartextHSFZ + KlartextHSFZTests only (Foundation-only)
```

## Build, sign & deploy to the iPhone

```bash
cd ios/KlartextProbe
xtool dev           # builds the app, signs it, installs + launches on the device
```

**Plug the iPhone into the machine (or `usbipd attach` on WSL) before `xtool dev`.** On
first launch, iOS shows the Local Network permission prompt — allow it, or connections are
silently blocked.

## Notes

- **Free Apple ID → 7-day resign:** apps signed with a free account expire after 7 days;
  re-run `xtool dev` to reinstall. (Confirm the exact behavior on first deploy.)
- **No asset catalog / AppIcon** by design (a probe). If an icon is added later, note that
  asset-catalog compilation (`actool`) is macOS-only.
- The `bundleID`, `product`, and `version` in `xtool.yml` should match what `xtool new`
  scaffolds — cross-check on first build if xtool rejects the manifest.

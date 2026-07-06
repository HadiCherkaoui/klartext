// swift-tools-version: 6.3
import PackageDescription

// Standalone, pure-Foundation HSFZ codec. No Apple-platform restriction, so `swift test`
// builds and runs it natively on Linux (no iOS SDK, no device). The app package next door
// depends on it by path and builds it for iOS via xtool.
let package = Package(
    name: "KlartextHSFZ",
    products: [
        .library(name: "KlartextHSFZ", targets: ["KlartextHSFZ"]),
    ],
    targets: [
        .target(name: "KlartextHSFZ"),
        .testTarget(name: "KlartextHSFZTests", dependencies: ["KlartextHSFZ"]),
    ]
)

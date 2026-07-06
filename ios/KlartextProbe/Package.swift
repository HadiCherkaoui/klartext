// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "KlartextProbe",
    platforms: [.iOS(.v17)],
    products: [
        .executable(name: "KlartextProbe", targets: ["KlartextProbe"]),
    ],
    targets: [
        // Pure-Foundation HSFZ frame codec — builds & tests on Linux (no iOS SDK).
        .target(name: "KlartextHSFZ"),
        // The iOS app (SwiftUI + Network). xtool packs this executable into the .app.
        .executableTarget(
            name: "KlartextProbe",
            dependencies: ["KlartextHSFZ"]
        ),
        // Codec tests — `swift test` runs these on Linux, no device.
        .testTarget(
            name: "KlartextHSFZTests",
            dependencies: ["KlartextHSFZ"]
        ),
    ]
)

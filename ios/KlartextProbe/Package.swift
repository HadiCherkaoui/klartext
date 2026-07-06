// swift-tools-version: 6.0
import PackageDescription

// The iOS app (SwiftUI + Network). xtool packs this executable into the .app.
// The HSFZ codec lives in a sibling package so its tests run on Linux (see ../KlartextHSFZ);
// here it's a normal dependency, compiled for iOS as part of the app build.
let package = Package(
    name: "KlartextProbe",
    platforms: [.iOS(.v17)],
    products: [
        .executable(name: "KlartextProbe", targets: ["KlartextProbe"]),
    ],
    dependencies: [
        .package(path: "../KlartextHSFZ"),
    ],
    targets: [
        .executableTarget(
            name: "KlartextProbe",
            dependencies: [.product(name: "KlartextHSFZ", package: "KlartextHSFZ")]
        ),
    ]
)

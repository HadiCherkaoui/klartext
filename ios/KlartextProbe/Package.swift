// swift-tools-version: 6.3
import PackageDescription

// An xtool project must expose exactly ONE library product representing the app; xtool
// packs that library into the .app (the @main App type lives in this target). The HSFZ
// codec is a sibling package (../KlartextHSFZ) so its tests run on Linux; here it's a
// normal dependency, compiled for iOS as part of the app build.
let package = Package(
    name: "KlartextProbe",
    platforms: [
        .iOS(.v26),
        .macOS(.v14),
    ],
    products: [
        .library(name: "KlartextProbe", targets: ["KlartextProbe"]),
    ],
    dependencies: [
        .package(path: "../KlartextHSFZ"),
    ],
    targets: [
        .target(
            name: "KlartextProbe",
            dependencies: [.product(name: "KlartextHSFZ", package: "KlartextHSFZ")]
        ),
    ]
)

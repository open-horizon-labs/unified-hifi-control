// swift-tools-version: 5.9
//
// UHCKit — the platform-neutral UHC *control* client (#619).
//
// Deliberately free of MusicKit and of every other iOS-only framework: the
// watchOS build links this package, and a single `import MusicKit` anywhere in
// it breaks that build. The Apple Music bridge stays where it is, in
// `companion/apple_music_ios`, which is the opposite direction of travel (UHC
// drives the phone) and is not a control client at all.
//
// macOS is listed only so `swift test` runs the contract suite on the host
// toolchain without a simulator.
import PackageDescription

let package = Package(
    name: "UHCKit",
    platforms: [
        .iOS(.v17),
        .watchOS(.v10),
        .macOS(.v14),
    ],
    products: [
        .library(name: "UHCKit", targets: ["UHCKit"]),
    ],
    targets: [
        .target(name: "UHCKit"),
        // The contract suite reads tests/fixtures/uhckit_contract.json from the
        // repository root — the same file tests/uhckit_contract.rs guards — so
        // there is deliberately no bundled resource copy to drift from it.
        .testTarget(name: "UHCKitTests", dependencies: ["UHCKit"]),
    ]
)

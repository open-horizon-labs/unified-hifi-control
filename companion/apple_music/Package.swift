// swift-tools-version: 5.9
// The companion is a macOS-only target. UHC's Linux/QNAP builds never compile
// this package; they use the Rust MusicKitCompanion transport boundary.
import PackageDescription

let package = Package(
    name: "AppleMusicCompanion",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "AppleMusicCompanion", targets: ["AppleMusicCompanion"]),
        .executable(name: "AppleMusicCompanionApp", targets: ["AppleMusicCompanionApp"]),
    ],
    targets: [
        .target(name: "AppleMusicCompanion"),
        // Keeping the host in the package makes it buildable from SwiftPM and
        // gives Xcode a concrete executable target to sign. The target still
        // needs an Xcode app wrapper for distribution entitlements and the
        // bundled Info.plist (see Host/README.md).
        .executableTarget(
            name: "AppleMusicCompanionApp",
            dependencies: ["AppleMusicCompanion"],
            path: "Host"
        ),
    ]
)

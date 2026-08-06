// swift-tools-version: 5.9
// The companion is a macOS-only target. UHC's Linux/QNAP builds never compile
// this package; they use the Rust MusicKitCompanion transport boundary.
import PackageDescription

let package = Package(
    name: "AppleMusicCompanion",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "AppleMusicCompanion", targets: ["AppleMusicCompanion"]),
    ],
    targets: [
        .target(name: "AppleMusicCompanion"),
    ]
)

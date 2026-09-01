// swift-tools-version: 5.9
//
// This target is intentionally iOS-only. UHC's Linux/QNAP builds never
// compile it; they use the Rust MusicKitCompanion transport boundary.
import PackageDescription

let package = Package(
    name: "AppleMusicIOSCompanion",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "AppleMusicIOSCompanion", targets: ["AppleMusicIOSCompanion"]),
    ],
    targets: [
        .target(name: "AppleMusicIOSCompanion"),
    ]
)

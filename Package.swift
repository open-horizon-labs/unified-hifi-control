// swift-tools-version: 5.9
//
// Root package entry point for consumers that resolve UHC's reusable Apple
// libraries directly from the Unified Hi-Fi Control Git repository. Sources
// remain in their companion packages so local and remote consumers share one
// implementation.
import PackageDescription

let package = Package(
    name: "UnifiedHiFiControl",
    platforms: [
        .iOS(.v17),
        .watchOS(.v10),
        .macOS(.v14),
    ],
    products: [
        .library(name: "UHCKit", targets: ["UHCKit"]),
        .library(
            name: "AppleMusicIOSCompanion",
            targets: ["AppleMusicIOSCompanion"]
        ),
    ],
    targets: [
        .target(
            name: "UHCKit",
            path: "companion/uhckit/Sources/UHCKit"
        ),
        .target(
            name: "AppleMusicIOSCompanion",
            path: "companion/apple_music_ios/Sources/AppleMusicIOSCompanion"
        ),
    ]
)

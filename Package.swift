// swift-tools-version: 5.9
//
// Root package entry point for consumers that resolve UHCKit directly from the
// Unified Hi-Fi Control Git repository. The package source remains under
// companion/uhckit so the Apple workspace and local development package share
// one implementation.
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
    ],
    targets: [
        .target(
            name: "UHCKit",
            path: "companion/uhckit/Sources/UHCKit"
        ),
        .testTarget(
            name: "UHCKitTests",
            dependencies: ["UHCKit"],
            path: "companion/uhckit/Tests/UHCKitTests"
        ),
    ]
)

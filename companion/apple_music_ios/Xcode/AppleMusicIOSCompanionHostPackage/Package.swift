// swift-tools-version: 6.1
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let package = Package(
    name: "AppleMusicIOSCompanionHostFeature",
    platforms: [.iOS(.v17)],
    products: [
        // Products define the executables and libraries a package produces, making them visible to other packages.
        .library(
            name: "AppleMusicIOSCompanionHostFeature",
            targets: ["AppleMusicIOSCompanionHostFeature"]
        ),
    ],
    dependencies: [
        .package(path: "../../"),
        // The shared UHC control client (#619). Discovery lives here now, so
        // the iOS companion and the watchOS controller browse for _uhc._tcp
        // through exactly one implementation.
        .package(path: "../../../uhckit"),
    ],
    targets: [
        // Targets are the basic building blocks of a package, defining a module or a test suite.
        // Targets can depend on other targets in this package and products from dependencies.
        .target(
            name: "AppleMusicIOSCompanionHostFeature",
            dependencies: [
                .product(name: "AppleMusicIOSCompanion", package: "apple_music_ios"),
                .product(name: "UHCKit", package: "uhckit"),
            ]
        ),
        .testTarget(
            name: "AppleMusicIOSCompanionHostFeatureTests",
            dependencies: [
                "AppleMusicIOSCompanionHostFeature"
            ]
        ),
    ]
)

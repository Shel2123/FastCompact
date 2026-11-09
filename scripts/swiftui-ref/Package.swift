// swift-tools-version: 6.0
//
// SwiftUI reference for the stage 5 radial menu (docs/spike-5.md, G10).
// Not part of the cargo workspace and not product code.
import PackageDescription

let package = Package(
    name: "SwiftUIRef",
    platforms: [.macOS("26.0")],
    targets: [
        .executableTarget(
            name: "SwiftUIRef",
            path: "Sources/SwiftUIRef",
            // Carbon hot-key callbacks are C function pointers; the strict
            // Swift 6 concurrency checks add noise without value here.
            swiftSettings: [.swiftLanguageMode(.v5)]
        )
    ]
)

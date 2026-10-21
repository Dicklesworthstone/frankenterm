// swift-tools-version: 6.0
import PackageDescription

// This optional product requires the real native-ffi frankenterm-term artifact.
// Keeping it outside Kit allows the offline workspace app to build independently.
var targets: [Target] = [
    .target(name: "FrankenTermTerminal", dependencies: ["CFrankenTermTerminal",
        .product(name: "FrankenTermKit", package: "FrankenTermKit")],
        // The Rust static library's native dependency (rustc --print native-static-libs).
        linkerSettings: [.linkedLibrary("iconv", .when(platforms: [.iOS, .macOS]))])
]
#if os(Linux)
// Header-only binding target; supply the Rust static library on the link path.
targets.append(.systemLibrary(name: "CFrankenTermTerminal"))
#else
// Apple hosts link the XCFramework (iOS device, arm64 simulator, macOS) built by
// scripts/build-apple-terminal-xcframework.sh from the canonical term crate, so
// the tests below drive the real Rust engine rather than an injected model.
targets.append(.binaryTarget(name: "CFrankenTermTerminal", path: "Artifacts/CFrankenTermTerminal.xcframework"))
targets.append(.testTarget(name: "FrankenTermTerminalTests", dependencies: ["FrankenTermTerminal",
    .product(name: "FrankenTermKit", package: "FrankenTermKit")]))
#endif

let package = Package(
    name: "FrankenTermTerminal",
    platforms: [.iOS("26.0"), .macOS("14.0")],
    products: [.library(name: "FrankenTermTerminal", targets: ["FrankenTermTerminal"])],
    dependencies: [.package(path: "../FrankenTermKit")],
    targets: targets,
    swiftLanguageModes: [.v6]
)

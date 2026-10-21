// swift-tools-version: 6.0
import PackageDescription

// This optional product requires the real native-ffi frankenterm-term artifact.
// Keeping it outside Kit allows the offline workspace app to build independently.
let package = Package(
    name: "FrankenTermTerminal",
    platforms: [.iOS("26.0"), .macOS("14.0")],
    products: [.library(name: "FrankenTermTerminal", targets: ["FrankenTermTerminal"])],
    dependencies: [.package(path: "../FrankenTermKit")],
    targets: [
        .systemLibrary(name: "CFrankenTermTerminal"),
        .target(name: "FrankenTermTerminal", dependencies: ["CFrankenTermTerminal",
            .product(name: "FrankenTermKit", package: "FrankenTermKit")])
    ],
    swiftLanguageModes: [.v6]
)

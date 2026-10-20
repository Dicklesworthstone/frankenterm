// swift-tools-version: 6.0
import PackageDescription

var testDependencies: [Target.Dependency] = ["FrankenTermKit"]
var targets: [Target] = [
    .systemLibrary(name: "CZlib"),
    .target(name: "FrankenTermKit", dependencies: ["CZlib"])
]
#if os(Linux)
// Independent test oracle only. No OpenSSL edge enters an Apple product.
targets.append(.systemLibrary(name: "CTestOpenSSL", path: "Tests/CTestOpenSSL"))
testDependencies.append("CTestOpenSSL")
#endif
targets.append(.testTarget(name: "FrankenTermKitTests", dependencies: testDependencies))

let package = Package(
    name: "FrankenTermKit",
    platforms: [.iOS("26.0"), .macOS("14.0")],
    products: [.library(name: "FrankenTermKit", targets: ["FrankenTermKit"])],
    targets: targets,
    swiftLanguageModes: [.v6]
)

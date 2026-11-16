// swift-tools-version: 6.0
import PackageDescription

var testDependencies: [Target.Dependency] = ["FrankenTermKit"]
// CryptoExtras supplies maintained RSA primitives on every platform. Existing
// Apple Ed25519, ECDSA, HPKE and archive cryptography continue using CryptoKit.
var coreDependencies: [Target.Dependency] = ["CZlib", "COpenSSHBcrypt", "CQRCodeGen",
    .product(name: "CryptoExtras", package: "swift-crypto")]
let packageDependencies: [Package.Dependency] = [
    .package(url: "https://github.com/apple/swift-crypto.git", exact: "4.5.2")
]
#if os(Linux)
// Exercise the same maintained HPKE API on Linux.
coreDependencies.append(.product(name: "Crypto", package: "swift-crypto"))
#endif
var targets: [Target] = [
    .systemLibrary(name: "CZlib"),
    .target(name: "COpenSSHBcrypt", path: "Sources/COpenSSHBcrypt", publicHeadersPath: "include"),
    .target(name: "CQRCodeGen", path: "Sources/CQRCodeGen", publicHeadersPath: "include"),
    .target(name: "FrankenTermKit", dependencies: coreDependencies)
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
    dependencies: packageDependencies,
    targets: targets,
    swiftLanguageModes: [.v6]
)

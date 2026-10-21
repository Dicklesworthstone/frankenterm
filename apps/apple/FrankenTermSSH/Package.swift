// swift-tools-version: 6.0
import PackageDescription

var dependencies: [Package.Dependency] = [.package(path: "../FrankenTermKit")]
var testDependencies: [Target.Dependency] = ["FrankenTermSSH", "FrankenTermSSHIntegration",
    .product(name: "FrankenTermKit", package: "FrankenTermKit")]
#if os(Linux)
// The kit already pins this maintained implementation. Direct use here is only
// for creating synthetic import fixtures; no extra Apple product dependency.
dependencies.append(.package(url: "https://github.com/apple/swift-crypto.git", exact: "4.5.2"))
testDependencies.append(.product(name: "Crypto", package: "swift-crypto"))
#endif

let package = Package(
    name: "FrankenTermSSH",
    platforms: [.iOS("26.0"), .macOS("14.0")],
    products: [.library(name: "FrankenTermSSHNative", targets: ["CFrankenTermSSH"]),
               .library(name: "FrankenTermSSH", targets: ["FrankenTermSSH"]),
               .library(name: "FrankenTermSSHIntegration", targets: ["FrankenTermSSHIntegration"]),
               .executable(name: "FrankenTermSSHInteropProbe", targets: ["FrankenTermSSHInteropProbe"]),
               .executable(name: "WorkspaceSSHProbe", targets: ["WorkspaceSSHProbe"])],
    dependencies: dependencies,
    targets: [
        .systemLibrary(name: "CLibssh2", pkgConfig: "libssh2",
                       providers: [.brew(["libssh2"]), .apt(["libssh2-1-dev"])]),
        .target(name: "CFrankenTermSSH", dependencies: ["CLibssh2"], publicHeadersPath: "include",
                cSettings: [.define("_DEFAULT_SOURCE", .when(platforms: [.linux]))],
                linkerSettings: [.linkedLibrary("pthread", .when(platforms: [.linux]))]),
        .target(name: "FrankenTermSSH", dependencies: ["CFrankenTermSSH"]),
        .target(name: "FrankenTermSSHIntegration", dependencies: ["FrankenTermSSH",
                .product(name: "FrankenTermKit", package: "FrankenTermKit")]),
        .testTarget(name: "FrankenTermSSHTests", dependencies: testDependencies),
        .testTarget(name: "FrankenTermSSHIntegrationTests", dependencies: ["FrankenTermSSHIntegration", "FrankenTermSSH",
                .product(name: "FrankenTermKit", package: "FrankenTermKit")]),
        // Independent loopback test driver only; not linked into the app.
        .executableTarget(name: "FrankenTermSSHInteropProbe", dependencies: ["FrankenTermSSHIntegration", "FrankenTermSSH",
                .product(name: "FrankenTermKit", package: "FrankenTermKit")],
                path: "Tests/Interop", exclude: ["PumpProbe.swift", "WorkspaceProbe.swift", "test_swift.py", "test_pump.py", "test_workspace.py"],
                sources: ["SessionProbe.swift"]),
        // Published legacy UUID-bound adapter probe, retained independently.
        .executableTarget(name: "TerminalPumpProbe", dependencies: ["FrankenTermSSHIntegration"],
                path: "Tests/Interop", exclude: ["SessionProbe.swift", "WorkspaceProbe.swift", "test_swift.py", "test_pump.py", "test_workspace.py"],
                sources: ["PumpProbe.swift"]),
        // Independently generated keys, protected broker races, durable journal.
        .executableTarget(name: "WorkspaceSSHProbe", dependencies: testDependencies,
                path: "Tests/Interop", exclude: ["SessionProbe.swift", "PumpProbe.swift", "test_swift.py", "test_pump.py", "test_workspace.py"],
                sources: ["WorkspaceProbe.swift"])
    ],
    cLanguageStandard: .c11
)

// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "FrankenTermSSH",
    platforms: [.iOS("26.0"), .macOS("14.0")],
    products: [.library(name: "FrankenTermSSHNative", targets: ["CFrankenTermSSH"]),
               .library(name: "FrankenTermSSH", targets: ["FrankenTermSSH"]),
               .library(name: "FrankenTermSSHIntegration", targets: ["FrankenTermSSHIntegration"]),
               .executable(name: "FrankenTermSSHInteropProbe", targets: ["FrankenTermSSHInteropProbe"])],
    dependencies: [.package(path: "../FrankenTermKit")],
    targets: [
        .systemLibrary(name: "CLibssh2", pkgConfig: "libssh2",
                       providers: [.brew(["libssh2"]), .apt(["libssh2-1-dev"])]),
        .target(name: "CFrankenTermSSH", dependencies: ["CLibssh2"], publicHeadersPath: "include",
                cSettings: [.define("_DEFAULT_SOURCE", .when(platforms: [.linux]))],
                linkerSettings: [.linkedLibrary("pthread", .when(platforms: [.linux]))]),
        .target(name: "FrankenTermSSH", dependencies: ["CFrankenTermSSH"]),
        .target(name: "FrankenTermSSHIntegration", dependencies: ["FrankenTermSSH",
                .product(name: "FrankenTermKit", package: "FrankenTermKit")]),
        .testTarget(name: "FrankenTermSSHTests", dependencies: ["FrankenTermSSH"]),
        .testTarget(name: "FrankenTermSSHIntegrationTests", dependencies: ["FrankenTermSSHIntegration", "FrankenTermSSH",
                .product(name: "FrankenTermKit", package: "FrankenTermKit")]),
        // Independent loopback test driver only; not linked into the app.
        .executableTarget(name: "FrankenTermSSHInteropProbe", dependencies: ["FrankenTermSSHIntegration", "FrankenTermSSH",
                .product(name: "FrankenTermKit", package: "FrankenTermKit")],
                path: "Tests/Interop", exclude: ["PumpProbe.swift", "test_swift.py", "test_pump.py"], sources: ["SessionProbe.swift"]),
        // Published legacy UUID-bound adapter probe, retained independently.
        .executableTarget(name: "TerminalPumpProbe", dependencies: ["FrankenTermSSHIntegration"],
                path: "Tests/Interop", exclude: ["SessionProbe.swift", "test_swift.py", "test_pump.py"], sources: ["PumpProbe.swift"])
    ],
    cLanguageStandard: .c11
)

// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "FrankenTermSSH",
    platforms: [.iOS("26.0"), .macOS("14.0")],
    products: [.library(name: "FrankenTermSSHNative", targets: ["CFrankenTermSSH"]),
               .library(name: "FrankenTermSSH", targets: ["FrankenTermSSH"]),
               .library(name: "FrankenTermSSHIntegration", targets: ["FrankenTermSSHIntegration"])],
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
        // Test-only executable; neither native app links this signing IPC probe.
        .executableTarget(name: "TerminalPumpProbe", dependencies: ["FrankenTermSSHIntegration"],
            path: "Tests/Interop", exclude: ["SessionProbe.swift", "test_swift.py", "test_pump.py"], sources: ["PumpProbe.swift"]),
        .testTarget(name: "FrankenTermSSHTests", dependencies: ["FrankenTermSSH"])
    ],
    cLanguageStandard: .c11
)

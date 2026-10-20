// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "FrankenTermSSH",
    platforms: [.iOS("26.0"), .macOS("14.0")],
    products: [.library(name: "FrankenTermSSHNative", targets: ["CFrankenTermSSH"]),
               .library(name: "FrankenTermSSH", targets: ["FrankenTermSSH"])],
    targets: [
        .systemLibrary(name: "CLibssh2", pkgConfig: "libssh2",
                       providers: [.brew(["libssh2"]), .apt(["libssh2-1-dev"])]),
        .target(name: "CFrankenTermSSH", dependencies: ["CLibssh2"], publicHeadersPath: "include",
                cSettings: [.define("_DEFAULT_SOURCE", .when(platforms: [.linux]))],
                linkerSettings: [.linkedLibrary("pthread", .when(platforms: [.linux]))]),
        .target(name: "FrankenTermSSH", dependencies: ["CFrankenTermSSH"]),
        .testTarget(name: "FrankenTermSSHTests", dependencies: ["FrankenTermSSH"])
    ],
    cLanguageStandard: .c11
)

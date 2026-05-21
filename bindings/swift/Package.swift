// swift-tools-version:5.9
import PackageDescription
import Foundation

// Absolute path to target/release, computed from this manifest's location so
// the linker and runtime rpath work regardless of the current directory.
let pkgDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path
let libDir = "\(pkgDir)/../../target/release"

let package = Package(
    name: "AxiomDB",
    products: [
        .library(name: "AxiomDB", targets: ["AxiomDB"])
    ],
    targets: [
        .target(name: "CAxiomDB"),
        .target(
            name: "AxiomDB",
            dependencies: ["CAxiomDB"],
            linkerSettings: [
                .unsafeFlags([
                    "-L\(libDir)",
                    "-laxiomdb_embedded",
                    "-Xlinker", "-rpath", "-Xlinker", libDir,
                ])
            ]
        ),
        .executableTarget(
            name: "bench",
            dependencies: ["AxiomDB"],
            linkerSettings: [.linkedLibrary("sqlite3")]
        ),
        .testTarget(name: "AxiomDBTests", dependencies: ["AxiomDB"]),
    ]
)

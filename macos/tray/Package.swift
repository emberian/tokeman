// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "TokemanTray",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(name: "TokemanTray", path: "Sources"),
    ]
)

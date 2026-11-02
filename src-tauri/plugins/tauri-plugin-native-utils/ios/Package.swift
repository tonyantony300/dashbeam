// swift-tools-version:5.5

import PackageDescription

let package = Package(
  name: "tauri-plugin-native-utils",
  platforms: [
    .iOS(.v15)
  ],
  products: [
    .library(
      name: "tauri-plugin-native-utils",
      type: .static,
      targets: ["tauri-plugin-native-utils"])
  ],
  dependencies: [
    // Populated by tauri-plugin's build script (see ../build.rs).
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    .target(
      name: "tauri-plugin-native-utils",
      dependencies: [
        .byName(name: "Tauri")
      ],
      path: "Sources")
  ]
)

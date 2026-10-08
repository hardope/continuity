// swift-tools-version:5.9
//
// The macOS settings window for Continuity: a small SwiftUI app bundled
// inside Continuity.app (Contents/Helpers/Continuity Settings.app) that
// talks to the running `continuityd` over its local control socket (see
// core/continuityd/src/control.rs). Build the .app with build-app.sh.

import PackageDescription

let package = Package(
    name: "ContinuitySettings",
    // macOS 12 lays the pages out by hand (see Sources/SettingsUI/Layout.swift);
    // 13 and later get the grouped form, and 26 Liquid Glass.
    platforms: [.macOS(.v12)],
    targets: [
        // Everything but the entry point, so the preview tool can reuse it.
        .target(name: "SettingsUI"),
        .executableTarget(name: "ContinuitySettings", dependencies: ["SettingsUI"]),
        // Development only: renders the screens to PNGs offscreen, with
        // sample data, without opening a window or needing continuityd.
        .executableTarget(name: "SettingsPreview", dependencies: ["SettingsUI"]),
        // Fixtures/status.json is also checked from the Rust side
        // (core/continuityd/src/control.rs), so the two can't drift apart.
        .testTarget(name: "SettingsUITests", dependencies: ["SettingsUI"], resources: [.copy("Fixtures")]),
    ]
)

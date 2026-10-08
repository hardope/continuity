// Development-only views of the settings window, with sample data —
// nothing talks to continuityd:
//
//     swift run SettingsPreview <output dir>   # every page to PNGs, offscreen
//     swift run SettingsPreview --window       # the real window, live on screen
//
// Add --legacy to either for the macOS 12 layout (see Layout.swift).
// Offscreen renders can't draw Liquid Glass (the window server composites
// it), so they show the classic fallback; `--window` shows the real thing.
// In the live window, changes only update the sample data.

import AppKit
import SettingsUI
import SwiftUI

let legacy = CommandLine.arguments.contains("--legacy")

let sample = Status(
    device: ThisDevice(name: "MacBook Air", id: "9f2c4e81b07a5d3361e8a0c2f4b9d7e1", platform: .macOS, version: "0.1.6-beta.9"),
    paused: false,
    devices: [
        PairedDevice(id: "pixel", name: "Pixel 8", platform: .android, connected: true, pairedAtUnix: 1_756_000_000, remoteControlAllowed: true, unlockAllowed: false),
        PairedDevice(id: "office", name: "Office PC", platform: .windows, connected: false, pairedAtUnix: 1_757_500_000, remoteControlAllowed: false, unlockAllowed: false),
        PairedDevice(id: "ubuntu", name: "ubuntu-desk", platform: .linux, connected: true, pairedAtUnix: 1_758_900_000, remoteControlAllowed: false, unlockAllowed: false),
    ],
    nearby: [NearbyDevice(id: "tab", name: "Galaxy Tab S9", platform: .android)],
    receivedFilesDir: FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Downloads/Continuity").path,
    canBeControlled: true,
    canBeUnlocked: false,
    about: About(
        protocolVersion: 2,
        os: "macOS \(ProcessInfo.processInfo.operatingSystemVersion.majorVersion).\(ProcessInfo.processInfo.operatingSystemVersion.minorVersion)",
        startedAtUnix: UInt64(Date().addingTimeInterval(-2 * 3600 - 300).timeIntervalSince1970),
        logFile: FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/app.continuity.continuity/continuityd.log").path,
        configDir: FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/app.continuity.continuity").path
    ),
    activity: Activity(
        connections: 4, clipboardSent: 23, clipboardReceived: 9, filesSent: 3, bytesSent: 18_400_000,
        filesReceived: 1, bytesReceived: 2_100_000, remoteControlSessions: 2, screenLocks: 1
    ),
    permissions: Permissions(screenRecording: false, accessibility: true)
)

/// `--window`: the same scene the real app uses, on screen.
@available(macOS 13.0, *)
struct DemoApp: App {
    @NSApplicationDelegateAdaptor(DemoDelegate.self) private var delegate
    @StateObject private var model = SettingsModel(preview: sample, selection: .device("pixel"))

    var body: some Scene {
        Window("Continuity", id: "settings") {
            RootView()
                .environmentObject(model)
                .environment(\.legacyLayout, legacy)
                .frame(minWidth: 680, minHeight: 460)
        }
        .defaultSize(width: 820, height: 580)
    }
}

final class DemoDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // A bare executable (no .app bundle around it) starts in the
        // background; bring the window forward like a normal app.
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

@MainActor
func render<Page: View>(_ page: Page, size: CGSize, named name: String, into directory: URL, appearance: NSAppearance.Name) throws {
    let view = page.environment(\.glassDisabled, true).environment(\.legacyLayout, legacy).frame(width: size.width, height: size.height)
    let hosting = NSHostingView(rootView: view)
    hosting.frame = CGRect(origin: .zero, size: size)
    let window = NSWindow(contentRect: hosting.frame, styleMask: [.titled, .closable, .resizable, .fullSizeContentView], backing: .buffered, defer: false)
    window.appearance = NSAppearance(named: appearance)
    window.contentView = hosting
    hosting.layoutSubtreeIfNeeded()
    // Let SwiftUI finish its first layout passes.
    RunLoop.main.run(until: Date().addingTimeInterval(0.6))
    guard let bitmap = hosting.bitmapImageRepForCachingDisplay(in: hosting.bounds) else {
        throw CocoaError(.fileWriteUnknown)
    }
    hosting.cacheDisplay(in: hosting.bounds, to: bitmap)
    guard let png = bitmap.representation(using: .png, properties: [:]) else {
        throw CocoaError(.fileWriteUnknown)
    }
    try png.write(to: directory.appendingPathComponent("\(name).png"))
    window.close()
}

if CommandLine.arguments.contains("--window") {
    if #available(macOS 13.0, *) {
        DemoApp.main()
    } else {
        FileHandle.standardError.write(Data("--window needs macOS 13 or newer\n".utf8))
    }
} else {
    let output = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first { !$0.hasPrefix("--") } ?? "preview", isDirectory: true)
    try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
    let app = NSApplication.shared
    app.setActivationPolicy(.prohibited)
    MainActor.assumeIsolated {
        let pages: [(SidebarItem?, String)] = [
            (.thisDevice, "this-device"),
            (.device("pixel"), "device-connected"),
            (.device("office"), "device-offline"),
            (.nearby("tab"), "nearby"),
        ]
        let suffix = legacy ? "-legacy" : ""
        for (appearance, look) in [(NSAppearance.Name.aqua, "light"), (.darkAqua, "dark")] {
            do {
                for (selection, name) in pages {
                    let model = SettingsModel(preview: sample, selection: selection)
                    try render(RootView().environmentObject(model), size: CGSize(width: 820, height: 580), named: "\(name)-\(look)\(suffix)", into: output, appearance: appearance)
                }
                let model = SettingsModel(preview: sample, selection: .thisDevice)
                try render(InfoView(status: sample).environmentObject(model), size: CGSize(width: 560, height: 640), named: "info-\(look)\(suffix)", into: output, appearance: appearance)
            } catch {
                FileHandle.standardError.write(Data("couldn't render: \(error)\n".utf8))
            }
        }
    }
    print("wrote previews to \(output.path)")
}

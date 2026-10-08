// Development-only views of the settings window, with sample data —
// nothing talks to continuityd:
//
//     swift run SettingsPreview <output dir>   # every page to PNGs, offscreen
//     swift run SettingsPreview --window       # the real window, live on screen
//
// Offscreen renders can't draw Liquid Glass (the window server composites
// it), so they show the classic fallback; `--window` shows the real thing.
// In the live window, changes only update the sample data.

import AppKit
import SettingsUI
import SwiftUI

let sample = Status(
    device: ThisDevice(name: "MacBook Air", id: "0f3a", platform: .macOS, version: "0.1.6-beta.6"),
    paused: false,
    devices: [
        PairedDevice(id: "pixel", name: "Pixel 8", platform: .android, connected: true, pairedAtUnix: 1_756_000_000, remoteControlAllowed: true, unlockAllowed: false),
        PairedDevice(id: "office", name: "Office PC", platform: .windows, connected: false, pairedAtUnix: 1_757_500_000, remoteControlAllowed: false, unlockAllowed: false),
        PairedDevice(id: "ubuntu", name: "ubuntu-desk", platform: .linux, connected: true, pairedAtUnix: 1_758_900_000, remoteControlAllowed: false, unlockAllowed: false),
    ],
    nearby: [NearbyDevice(id: "tab", name: "Galaxy Tab S9", platform: .android)],
    receivedFilesDir: FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Downloads/Continuity").path,
    canBeControlled: true,
    canBeUnlocked: false
)

/// `--window`: the same scene the real app uses, on screen.
struct DemoApp: App {
    @NSApplicationDelegateAdaptor(DemoDelegate.self) private var delegate
    @StateObject private var model = SettingsModel(preview: sample, selection: .device("pixel"))

    var body: some Scene {
        Window("Continuity", id: "settings") {
            RootView()
                .environmentObject(model)
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
func render(_ selection: SidebarItem?, named name: String, into directory: URL, appearance: NSAppearance.Name) throws {
    let model = SettingsModel(preview: sample, selection: selection)
    let view = RootView().environmentObject(model).environment(\.glassDisabled, true).frame(width: 820, height: 580)
    let hosting = NSHostingView(rootView: view)
    hosting.frame = CGRect(x: 0, y: 0, width: 820, height: 580)
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
    DemoApp.main()
} else {
    let output = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first ?? "preview", isDirectory: true)
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
        for (selection, name) in pages {
            for (appearance, suffix) in [(NSAppearance.Name.aqua, "light"), (.darkAqua, "dark")] {
                do {
                    try render(selection, named: "\(name)-\(suffix)", into: output, appearance: appearance)
                } catch {
                    FileHandle.standardError.write(Data("couldn't render \(name): \(error)\n".utf8))
                }
            }
        }
    }
    print("wrote previews to \(output.path)")
}

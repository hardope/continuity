import AppKit
import SettingsUI
import SwiftUI

/// Continuity's settings window. Opened from the tray menu's "Settings…",
/// which launches this app from inside Continuity.app; it talks to the
/// running `continuityd` and keeps nothing of its own.
@main
struct ContinuitySettingsApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var model = SettingsModel(profile: profileArgument())

    var body: some Scene {
        Window("Continuity", id: "settings") {
            RootView()
                .environmentObject(model)
                .frame(minWidth: 680, minHeight: 460)
        }
        .defaultSize(width: 820, height: 580)
    }
}

/// `--profile <name>`, which continuityd passes for any profile but the
/// default one (see `CONTINUITY_PROFILE`).
private func profileArgument() -> String {
    let arguments = CommandLine.arguments
    if let flag = arguments.firstIndex(of: "--profile"), flag + 1 < arguments.count {
        return arguments[flag + 1]
    }
    return "default"
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    /// A settings window, not a document app: closing it quits.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

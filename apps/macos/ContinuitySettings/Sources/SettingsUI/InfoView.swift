import AppKit
import SwiftUI

/// The ⓘ panel: what Continuity is and how to use it, the permissions remote
/// control of this Mac needs and how to grant them, what it's done since it
/// started, and where its files are.
public struct InfoView: View {
    @EnvironmentObject private var model: SettingsModel
    @Environment(\.dismiss) private var dismiss
    let status: Status

    public init(status: Status) {
        self.status = status
    }

    public var body: some View {
        VStack(spacing: 0) {
            Page {
                PageSection {
                    AppHeader(version: status.device.version)
                }
                if status.canBeControlled, let permissions = status.permissions {
                    PermissionsSection(permissions: permissions)
                }
                HowToSection(receivedFilesDir: status.receivedFilesDir)
                if let activity = status.activity {
                    ActivitySection(activity: activity, startedAtUnix: status.about?.startedAtUnix)
                }
                PageSection(title: "This Mac") {
                    InfoRow(label: "Name", value: status.device.name)
                    if let about = status.about {
                        InfoRow(label: "System", value: about.os)
                        InfoRow(label: "Continuity", value: "\(status.device.version) · protocol \(about.protocolVersion)")
                    } else {
                        InfoRow(label: "Continuity", value: status.device.version)
                    }
                    HStack(spacing: 8) {
                        Text("Device ID")
                        Spacer(minLength: 8)
                        Text(status.device.id)
                            .font(.system(.body, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .textSelection(.enabled)
                        Button {
                            model.copyToPasteboard(status.device.id)
                        } label: {
                            Image(systemName: "doc.on.doc")
                        }
                        .buttonStyle(.borderless)
                        .help("Copy the device ID")
                    }
                }
                PageSection(title: "Files") {
                    PathRow(label: "Received files", path: status.receivedFilesDir) { model.showReceivedFiles() }
                    if let log = status.about?.logFile {
                        PathRow(label: "Log — attach it to a bug report", path: log) { model.showLogFile() }
                    }
                }
            }
            Divider()
            HStack(spacing: 14) {
                Link("Continuity on GitHub", destination: URL(string: "https://github.com/hardope/continuity")!)
                Link("Report a Problem", destination: URL(string: "https://github.com/hardope/continuity/issues/new")!)
                Text("MIT License")
                    .foregroundStyle(.secondary)
                Spacer(minLength: 8)
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
            .font(.callout)
            .padding(.horizontal, 18)
            .padding(.vertical, 12)
        }
        .frame(width: 560, height: 640)
    }
}

private struct AppHeader: View {
    let version: String

    var body: some View {
        HStack(spacing: 14) {
            Image(nsImage: NSApplication.shared.applicationIconImage)
                .resizable()
                .frame(width: 58, height: 58)
            VStack(alignment: .leading, spacing: 3) {
                Text("Continuity")
                    .font(.title2.weight(.semibold))
                Text("Version \(version)")
                    .foregroundStyle(.secondary)
                Text("Clipboard, files and remote control between your devices — over your own network, with no account.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 4)
    }
}

/// What remote control of this Mac needs from macOS, whether it has it, and
/// how to give it — in the words of the macOS version it's running on.
private struct PermissionsSection: View {
    @EnvironmentObject private var model: SettingsModel
    let permissions: Permissions

    private var macOS: Int { ProcessInfo.processInfo.operatingSystemVersion.majorVersion }
    private var settingsApp: String { macOS >= 13 ? "System Settings" : "System Preferences" }
    private var privacyPage: String { macOS >= 13 ? "Privacy & Security" : "Security & Privacy → Privacy" }

    var body: some View {
        PageSection(title: "Remote Control Permissions") {
            ForEach(Permission.allCases) { permission in
                HStack(spacing: 12) {
                    Image(systemName: permission.symbol)
                        .font(.title3)
                        .foregroundStyle(Color.accentColor)
                        .frame(width: 26)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(permission.title)
                        Text(permission.purpose)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    Spacer(minLength: 8)
                    if permissions.allows(permission) {
                        Label("Allowed", systemImage: "checkmark.circle.fill")
                            .foregroundStyle(.green)
                    } else {
                        Button("Grant…") { model.requestPermission(permission) }
                    }
                }
            }
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                Text("For your phone or another computer to control this Mac, Continuity needs both. Click Grant… and, in \(settingsApp) → \(privacyPage) → \(Permission.screenRecording.settingsName(macOS: macOS)) (then Accessibility), switch on Continuity.")
                Text("macOS applies Screen Recording after Continuity restarts: if it offers to quit and reopen Continuity, let it — otherwise quit it from the menu bar and open it again.")
                if macOS >= 15 {
                    Text("Continuity also needs Local Network access to find your devices; if you turned that down, switch it on under \(privacyPage) → Local Network.")
                }
            }
        }
    }
}

extension Permission {
    var title: String {
        switch self {
        case .screenRecording: "Screen Recording"
        case .accessibility: "Accessibility"
        }
    }

    /// Where System Settings lists it.
    func settingsName(macOS: Int) -> String {
        switch self {
        case .screenRecording: macOS >= 15 ? "Screen & System Audio Recording" : "Screen Recording"
        case .accessibility: "Accessibility"
        }
    }

    var purpose: String {
        switch self {
        case .screenRecording: "Lets the controlling device see this screen. Without it, it sees only the wallpaper."
        case .accessibility: "Lets it use the keyboard and mouse — and makes your phone's play, pause and skip buttons work."
        }
    }

    var symbol: String {
        switch self {
        case .screenRecording: "rectangle.dashed.badge.record"
        case .accessibility: "keyboard"
        }
    }
}

private struct HowToSection: View {
    let receivedFilesDir: String

    var body: some View {
        PageSection(title: "How to Use") {
            Tip(symbol: "link", text: "Pair a device by choosing it under Nearby, then check that both screens show the same code.")
            Tip(symbol: "doc.on.clipboard", text: "Copy on one device and paste on another — the clipboard follows you.")
            Tip(symbol: "paperplane", text: "Send files from a device's page, from Finder with Open With → Continuity, or from your phone's share sheet. They arrive in \(abbreviatingHome(receivedFilesDir)).")
            Tip(symbol: "iphone", text: "From your phone, control what's playing here, lock this Mac, or take control of its screen.")
            Tip(symbol: "pause.circle", text: "Pause syncing from the toolbar or the menu bar whenever you like.")
        }
    }
}

private struct Tip: View {
    let symbol: String
    let text: String

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(.secondary)
                .frame(width: 20)
            Text(text)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct ActivitySection: View {
    let activity: Activity
    let startedAtUnix: UInt64?

    var body: some View {
        PageSection(title: "Activity") {
            InfoRow(label: "Connections", value: "\(activity.connections)")
            InfoRow(label: "Clipboard", value: "\(activity.clipboardSent) sent · \(activity.clipboardReceived) received")
            InfoRow(label: "Files sent", value: files(activity.filesSent, bytes: activity.bytesSent))
            InfoRow(label: "Files received", value: files(activity.filesReceived, bytes: activity.bytesReceived))
            InfoRow(label: "Remote control sessions", value: "\(activity.remoteControlSessions)")
            InfoRow(label: "Locked or unlocked from a phone", value: "\(activity.screenLocks)")
        } footer: {
            if let startedAtUnix {
                Text("Since Continuity started, \(started(startedAtUnix)).")
            }
        }
    }

    private func files(_ count: UInt64, bytes: UInt64) -> String {
        count == 0 ? "None" : "\(count) · \(ByteCountFormatter.string(fromByteCount: Int64(clamping: bytes), countStyle: .file))"
    }

    private func started(_ unix: UInt64) -> String {
        RelativeDateTimeFormatter().localizedString(for: Date(timeIntervalSince1970: TimeInterval(unix)), relativeTo: Date())
    }
}

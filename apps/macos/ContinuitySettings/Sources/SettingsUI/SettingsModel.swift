import AppKit
import Foundation

public enum SidebarItem: Hashable {
    case thisDevice
    case device(String)
    case nearby(String)
}

/// A refused or failed change, shown as an alert.
public struct Problem: Identifiable {
    public let id = UUID()
    public let message: String
}

/// The window's state: the latest status from `continuityd` (kept current
/// by a `watch` connection, which reconnects if Continuity restarts) and
/// every change the window can make.
@MainActor
public final class SettingsModel: ObservableObject {
    public enum Connection: Equatable {
        case connecting
        case connected
        case unavailable(String)
    }

    @Published public private(set) var status: Status?
    @Published public private(set) var connection: Connection = .connecting
    @Published public var selection: SidebarItem? = .thisDevice
    @Published public var problem: Problem?

    private let client: ControlClient?
    private var watcher: LineConnection?
    private var retry: Task<Void, Never>?

    public init(profile: String) {
        client = ControlClient(profile: profile)
        startWatching()
    }

    /// Shows fixed sample data, for previews — talks to nothing.
    public init(preview status: Status, selection: SidebarItem?) {
        client = nil
        self.status = status
        self.selection = selection
        connection = .connected
    }

    public func device(_ id: String) -> PairedDevice? {
        status?.devices.first { $0.id == id }
    }

    public func nearbyDevice(_ id: String) -> NearbyDevice? {
        status?.nearby.first { $0.id == id }
    }

    // MARK: Changes

    public func setPaused(_ paused: Bool) {
        status?.paused = paused
        perform(["op": "set_paused", "paused": paused])
    }

    public func setRemoteControlAllowed(_ device: PairedDevice, _ allowed: Bool) {
        update(device.id) { $0.remoteControlAllowed = allowed }
        perform(["op": "set_remote_control_allowed", "device": device.id, "allowed": allowed])
    }

    public func setUnlockAllowed(_ device: PairedDevice, _ allowed: Bool) {
        update(device.id) { $0.unlockAllowed = allowed }
        perform(["op": "set_unlock_allowed", "device": device.id, "allowed": allowed])
    }

    /// Pairs with a nearby device (both sides then show a code to compare),
    /// or reconnects a paired one.
    public func connect(_ id: String) {
        perform(["op": "connect", "device": id])
    }

    public func disconnect(_ id: String) {
        perform(["op": "disconnect", "device": id])
    }

    public func forget(_ device: PairedDevice) {
        if selection == .device(device.id) {
            selection = .thisDevice
        }
        perform(["op": "forget", "device": device.id])
    }

    public func forgetAll() {
        selection = .thisDevice
        perform(["op": "reset"])
    }

    public func sendFiles(to device: PairedDevice) {
        let panel = NSOpenPanel()
        panel.title = "Send to \(device.name)"
        panel.prompt = "Send"
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        guard panel.runModal() == .OK, !panel.urls.isEmpty else { return }
        perform(["op": "send_files", "device": device.id, "paths": panel.urls.map(\.path)])
    }

    public func showReceivedFiles() {
        guard let dir = status?.receivedFilesDir else { return }
        // Created on the first file received; make sure there's
        // something to show before that.
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        NSWorkspace.shared.open(URL(fileURLWithPath: dir, isDirectory: true))
    }

    /// Starts Continuity itself, when this window finds it isn't running.
    public func openContinuity() {
        // Inside Continuity.app/Contents/Helpers/ when bundled.
        let helpers = Bundle.main.bundleURL.deletingLastPathComponent()
        let mainApp = helpers.deletingLastPathComponent().deletingLastPathComponent()
        let url = mainApp.pathExtension == "app"
            ? mainApp
            : NSWorkspace.shared.urlForApplication(withBundleIdentifier: "app.continuity.desktop")
        guard let url else {
            problem = Problem(message: "Couldn't find Continuity. Open it from your Applications folder.")
            return
        }
        NSWorkspace.shared.openApplication(at: url, configuration: NSWorkspace.OpenConfiguration())
    }

    // MARK: Plumbing

    private func update(_ id: String, _ change: (inout PairedDevice) -> Void) {
        guard let index = status?.devices.firstIndex(where: { $0.id == id }) else { return }
        change(&status!.devices[index])
    }

    /// Sends one change. The window shows it straight away; the next status
    /// from the watch confirms it — or puts it back, if it was refused.
    private func perform(_ fields: [String: Any]) {
        guard let client else { return }
        Task {
            do {
                let response = try await client.send(fields)
                if !response.ok {
                    problem = Problem(message: response.message ?? "Continuity didn't accept that change.")
                }
            } catch {
                problem = Problem(message: error.localizedDescription)
            }
        }
    }

    private func startWatching() {
        guard let client else { return }
        watcher?.close()
        watcher = client.watch(
            onStatus: { [weak self] status in
                Task { @MainActor in self?.received(status) }
            },
            onEnd: { [weak self] _ in
                Task { @MainActor in self?.lostConnection() }
            }
        )
        if watcher == nil {
            lostConnection()
        }
    }

    private func received(_ status: Status) {
        self.status = status
        connection = .connected
        // The selected device may have just been forgotten, or paired.
        switch selection {
        case .device(let id) where device(id) == nil:
            selection = .thisDevice
        case .nearby(let id) where nearbyDevice(id) == nil:
            selection = status.devices.contains { $0.id == id } ? .device(id) : .thisDevice
        default:
            break
        }
    }

    private func lostConnection() {
        watcher = nil
        connection = .unavailable(ControlError.notRunning.localizedDescription)
        retry?.cancel()
        retry = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 2_000_000_000)
            guard !Task.isCancelled else { return }
            self?.startWatching()
        }
    }
}

import SwiftUI

/// The whole window: paired and nearby devices in the sidebar, the selected
/// one's settings beside it — or, while Continuity isn't running, a way to
/// start it.
public struct RootView: View {
    @EnvironmentObject private var model: SettingsModel

    public init() {}

    public var body: some View {
        Group {
            if let status = model.status, model.connection == .connected {
                NavigationSplitView {
                    Sidebar(status: status)
                        .navigationSplitViewColumnWidth(min: 210, ideal: 240, max: 320)
                } detail: {
                    Detail(status: status)
                }
                .toolbar {
                    ToolbarItem(placement: .primaryAction) {
                        Button {
                            model.setPaused(!status.paused)
                        } label: {
                            Label(status.paused ? "Resume Syncing" : "Pause Syncing", systemImage: status.paused ? "play.fill" : "pause.fill")
                        }
                        .help(status.paused ? "Resume syncing with your devices" : "Pause syncing with your devices")
                    }
                }
            } else {
                NotRunningView()
            }
        }
        .alert(
            "Continuity",
            isPresented: Binding(get: { model.problem != nil }, set: { if !$0 { model.problem = nil } }),
            presenting: model.problem
        ) { _ in
            Button("OK", role: .cancel) {}
        } message: { problem in
            Text(problem.message)
        }
    }
}

struct Sidebar: View {
    @EnvironmentObject private var model: SettingsModel
    let status: Status

    var body: some View {
        List(selection: $model.selection) {
            Section {
                Label(status.device.name, systemImage: status.device.platform.symbol)
                    .tag(SidebarItem.thisDevice)
            }
            Section("Paired Devices") {
                if status.devices.isEmpty {
                    Text("None yet")
                        .foregroundStyle(.secondary)
                }
                ForEach(status.devices) { device in
                    DeviceRow(
                        name: device.name,
                        symbol: (device.platform ?? .unknown).symbol,
                        detail: device.connected ? "Connected" : "Not connected",
                        online: device.connected
                    )
                    .tag(SidebarItem.device(device.id))
                }
            }
            if !status.nearby.isEmpty {
                Section("Nearby") {
                    ForEach(status.nearby) { device in
                        DeviceRow(name: device.name, symbol: device.platform.symbol, detail: "Not paired", online: nil)
                            .tag(SidebarItem.nearby(device.id))
                    }
                }
            }
        }
        .listStyle(.sidebar)
    }
}

struct DeviceRow: View {
    let name: String
    let symbol: String
    let detail: String
    /// `nil` hides the status dot (an unpaired device).
    let online: Bool?

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: symbol)
                .frame(width: 20)
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(name)
                    .lineLimit(1)
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            if let online {
                Circle()
                    .fill(online ? Color.green : Color.secondary.opacity(0.35))
                    .frame(width: 7, height: 7)
                    .accessibilityLabel(online ? "Connected" : "Not connected")
            }
        }
    }
}

struct Detail: View {
    @EnvironmentObject private var model: SettingsModel
    let status: Status

    var body: some View {
        switch model.selection {
        case .device(let id):
            if let device = model.device(id) {
                DeviceView(device: device, status: status)
            } else {
                ThisDeviceView(status: status)
            }
        case .nearby(let id):
            if let device = model.nearbyDevice(id) {
                NearbyView(device: device)
            } else {
                ThisDeviceView(status: status)
            }
        case .thisDevice, nil:
            ThisDeviceView(status: status)
        }
    }
}

/// A page's first row: the device's icon on a disc, its name, a line about it.
struct Header: View {
    let symbol: String
    let tint: Color
    let title: String
    let subtitle: String

    var body: some View {
        HStack(spacing: 14) {
            Image(systemName: symbol)
                .font(.system(size: 24))
                .foregroundStyle(tint)
                .frame(width: 54, height: 54)
                .iconBadge(tint: tint)
            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                    .font(.title2.weight(.semibold))
                    .lineLimit(1)
                Text(subtitle)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 4)
    }
}

struct ThisDeviceView: View {
    @EnvironmentObject private var model: SettingsModel
    let status: Status
    @State private var confirmingForgetAll = false

    var body: some View {
        Form {
            Section {
                Header(
                    symbol: status.device.platform.symbol,
                    tint: .accentColor,
                    title: status.device.name,
                    subtitle: "This \(status.device.platform.displayName) · Continuity \(status.device.version)"
                )
            }
            Section {
                Toggle(isOn: Binding(get: { status.paused }, set: { model.setPaused($0) })) {
                    Text("Pause syncing")
                    Text("Stops clipboard sync and new connections until you resume. Devices that are already connected stay connected.")
                }
            }
            Section("Received Files") {
                LabeledContent("Saved to") {
                    Text(abbreviatingHome(status.receivedFilesDir))
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Button("Show in Finder") { model.showReceivedFiles() }
            }
            Section {
                Button("Forget All Devices…", role: .destructive) { confirmingForgetAll = true }
                    .disabled(status.devices.isEmpty)
            } footer: {
                Text("Every device will need to be paired again, on both sides.")
            }
        }
        .formStyle(.grouped)
        .navigationTitle(status.device.name)
        .confirmationDialog(
            "Forget all \(status.devices.count) paired devices?",
            isPresented: $confirmingForgetAll,
            titleVisibility: .visible
        ) {
            Button("Forget All", role: .destructive) { model.forgetAll() }
        } message: {
            Text("They disconnect now, and each one will need to be paired again from scratch.")
        }
    }
}

struct DeviceView: View {
    @EnvironmentObject private var model: SettingsModel
    let device: PairedDevice
    let status: Status
    @State private var confirmingForget = false
    @State private var confirmingRemoteControl = false
    @State private var confirmingUnlock = false

    private var platform: Platform { device.platform ?? .unknown }

    /// "Mac", "PC" or "computer" — what this computer is called in the copy.
    private var here: String {
        switch status.device.platform {
        case .macOS: "Mac"
        case .windows: "PC"
        default: "computer"
        }
    }

    var body: some View {
        Form {
            Section {
                Header(
                    symbol: platform.symbol,
                    tint: device.connected ? .green : .secondary,
                    title: device.name,
                    subtitle: device.connected ? "\(platform.displayName) · connected" : "\(platform.displayName) · not connected"
                )
                HStack(spacing: 10) {
                    Button {
                        model.sendFiles(to: device)
                    } label: {
                        Label("Send Files…", systemImage: "paperplane")
                    }
                    .primaryActionStyle()
                    .disabled(!device.connected)
                    if device.connected {
                        Button("Disconnect") { model.disconnect(device.id) }
                            .secondaryActionStyle()
                    } else {
                        Button("Connect") { model.connect(device.id) }
                            .secondaryActionStyle()
                    }
                }
            }
            if status.canBeControlled {
                Section("Remote Control") {
                    Toggle(isOn: Binding(
                        get: { device.remoteControlAllowed },
                        set: { allowed in
                            if allowed {
                                confirmingRemoteControl = true
                            } else {
                                model.setRemoteControlAllowed(device, false)
                            }
                        }
                    )) {
                        Text("Control this \(here) without asking")
                        Text(device.remoteControlAllowed
                            ? "\(device.name) can see this screen and use its keyboard and mouse whenever it asks."
                            : "This \(here) asks you first each time \(device.name) wants to control it.")
                    }
                }
            }
            if status.canBeUnlocked {
                Section("Remote Unlock") {
                    Toggle(isOn: Binding(
                        get: { device.unlockAllowed },
                        set: { allowed in
                            if allowed {
                                confirmingUnlock = true
                            } else {
                                model.setUnlockAllowed(device, false)
                            }
                        }
                    )) {
                        Text("Unlock this \(here)")
                        Text("You're notified every time it's used. Locking never needs permission.")
                    }
                }
            }
            Section {
                Button("Forget \(device.name)…", role: .destructive) { confirmingForget = true }
            } footer: {
                Text("Paired \(Date(timeIntervalSince1970: TimeInterval(device.pairedAtUnix)).formatted(date: .long, time: .omitted))")
            }
        }
        .formStyle(.grouped)
        .navigationTitle(device.name)
        .confirmationDialog("Let \(device.name) control this \(here) without asking?", isPresented: $confirmingRemoteControl, titleVisibility: .visible) {
            Button("Allow") { model.setRemoteControlAllowed(device, true) }
        } message: {
            Text("Whenever it asks, \(device.name) will see this screen and use its keyboard and mouse — no prompt here first. Anyone with \(device.name) unlocked could do the same.")
        }
        .confirmationDialog("Let \(device.name) unlock this \(here)?", isPresented: $confirmingUnlock, titleVisibility: .visible) {
            Button("Allow") { model.setUnlockAllowed(device, true) }
        } message: {
            Text("\(device.name) will be able to unlock this \(here) without its password. Anyone using \(device.name) while it's unlocked could too.")
        }
        .confirmationDialog("Forget \(device.name)?", isPresented: $confirmingForget, titleVisibility: .visible) {
            Button("Forget", role: .destructive) { model.forget(device) }
        } message: {
            Text("It disconnects now and will need to be paired again from scratch.")
        }
    }
}

struct NearbyView: View {
    @EnvironmentObject private var model: SettingsModel
    let device: NearbyDevice
    @State private var requested = false

    var body: some View {
        Form {
            Section {
                Header(
                    symbol: device.platform.symbol,
                    tint: .accentColor,
                    title: device.name,
                    subtitle: "\(device.platform.displayName) nearby · not paired"
                )
                Button {
                    requested = true
                    model.connect(device.id)
                } label: {
                    Label("Pair…", systemImage: "link")
                }
                .primaryActionStyle()
            } footer: {
                Text(requested
                    ? "Compare the codes on both devices, and confirm on each only if they match."
                    : "Pairing shows a code on both devices. Confirm on each only if the codes match.")
            }
        }
        .formStyle(.grouped)
        .navigationTitle(device.name)
    }
}

struct NotRunningView: View {
    @EnvironmentObject private var model: SettingsModel

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "bolt.horizontal.circle")
                .font(.system(size: 44))
                .foregroundStyle(.secondary)
            if case .unavailable(let reason) = model.connection {
                Text(reason)
                    .font(.title3.weight(.semibold))
                Text("Settings appear here once it's running.")
                    .foregroundStyle(.secondary)
                Button("Open Continuity") { model.openContinuity() }
                    .primaryActionStyle()
            } else {
                Text("Connecting to Continuity…")
                    .font(.title3.weight(.semibold))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(40)
    }
}

/// `/Users/me/Downloads/Continuity` → `~/Downloads/Continuity`.
func abbreviatingHome(_ path: String) -> String {
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    return path.hasPrefix(home) ? "~" + path.dropFirst(home.count) : path
}

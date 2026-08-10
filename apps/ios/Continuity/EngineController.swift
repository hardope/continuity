import Foundation
import SwiftUI

enum ActivityTint {
    case neutral, success, warning
}

struct ActivityEntry: Identifiable {
    let id = UUID()
    let symbol: String
    let text: String
    let tint: ActivityTint
}

/// Owns the running engine and republishes its events as SwiftUI state.
/// `EventListener.onEvent` fires on a Rust/tokio thread, not the main
/// thread, so every update here is dispatched back to main before
/// touching `@Published` state.
@MainActor
final class EngineController: ObservableObject {
    @Published var deviceId: String = "starting..."
    @Published var statusText: String = "Starting..."
    @Published var activity: [ActivityEntry] = []
    @Published var pendingPairing: (peer: FfiDeviceInfo, code: String)?
    @Published var connectedPeer: FfiDeviceInfo?

    private var engine: ContinuityEngine?

    /// iOS has no background execution model that keeps a mesh connection
    /// or a listening socket alive the way a desktop daemon or an Android
    /// foreground service can — see docs/protocol.md. Sync is live only
    /// while this object is around, i.e. while the app is foregrounded.
    func start() {
        guard engine == nil else { return }

        do {
            let identityDer = try SecureIdentity.loadOrCreateIdentityDer()
            deviceId = try deviceIdFor(identityDer: identityDer)

            let appSupport = try FileManager.default.url(
                for: .applicationSupportDirectory,
                in: .userDomainMask,
                appropriateFor: nil,
                create: true
            )
            let receivedDir = appSupport.appendingPathComponent("Continuity", isDirectory: true)
            try FileManager.default.createDirectory(at: receivedDir, withIntermediateDirectories: true)

            let listener = ClosureEventListener { [weak self] event in
                Task { @MainActor in self?.handle(event) }
            }

            engine = try ContinuityEngine.start(
                identityDer: identityDer,
                profile: "default",
                deviceName: SecureIdentity.deviceName(),
                dataDir: appSupport.path,
                receivedFilesDir: receivedDir.path,
                clipboard: IosClipboardProvider(),
                listener: listener
            )
        } catch {
            statusText = "Failed to start: \(error)"
        }
    }

    func confirmPairing(accept: Bool) {
        guard let pending = pendingPairing else { return }
        engine?.confirmPairing(peerId: pending.peer.id, accept: accept)
        pendingPairing = nil
    }

    func sendFile(at url: URL) {
        guard let peer = connectedPeer else { return }
        // Copy into a location the Rust side can open by plain path —
        // `url` may be a security-scoped resource from a file picker.
        let accessed = url.startAccessingSecurityScopedResource()
        defer { if accessed { url.stopAccessingSecurityScopedResource() } }

        do {
            let dest = FileManager.default.temporaryDirectory.appendingPathComponent(url.lastPathComponent)
            if FileManager.default.fileExists(atPath: dest.path) {
                try FileManager.default.removeItem(at: dest)
            }
            try FileManager.default.copyItem(at: url, to: dest)
            engine?.sendFile(peerId: peer.id, path: dest.path)
        } catch {
            append(symbol: "exclamationmark.triangle", text: "Couldn't stage file for sending: \(error)", tint: .warning)
        }
    }

    private func append(symbol: String, text: String, tint: ActivityTint) {
        activity.insert(ActivityEntry(symbol: symbol, text: text, tint: tint), at: 0)
    }

    private func handle(_ event: FfiSyncEvent) {
        switch event {
        case .pairingRequested(let peer, let code):
            pendingPairing = (peer, code)
        case .paired(let peer):
            statusText = "Paired with \(peer.name)"
            append(symbol: "checkmark.shield", text: "Paired with '\(peer.name)'", tint: .success)
        case .pairingDeclined(let peerName):
            append(symbol: "xmark.circle", text: "Pairing with '\(peerName)' declined", tint: .warning)
        case .connected(let peer):
            connectedPeer = peer
            statusText = "Connected: \(peer.name)"
            append(symbol: "link", text: "Connected to '\(peer.name)'", tint: .success)
        case .disconnected(let peerId, let peerName):
            if connectedPeer?.id == peerId {
                connectedPeer = nil
                statusText = "No device connected"
            }
            append(symbol: "link.badge.plus", text: "'\(peerName)' disconnected", tint: .neutral)
        case .clipboardReceived(let fromName):
            append(symbol: "arrow.triangle.2.circlepath", text: "Clipboard synced from '\(fromName)'", tint: .neutral)
        case .fileReceiving(_, let fromName, let fileName, _):
            append(symbol: "arrow.down.doc", text: "Receiving '\(fileName)' from '\(fromName)'...", tint: .neutral)
        case .fileReceived(_, let fileName, _):
            append(symbol: "tray.and.arrow.down", text: "Received '\(fileName)'", tint: .success)
        case .fileSent(_, let fileName, let toName):
            append(symbol: "tray.and.arrow.up", text: "Sent '\(fileName)' to '\(toName)'", tint: .success)
        case .fileTransferFailed(_, let reason):
            append(symbol: "exclamationmark.triangle", text: "Transfer failed: \(reason)", tint: .warning)
        case .error(let message):
            append(symbol: "exclamationmark.triangle", text: message, tint: .warning)
        case .listening, .clipboardBroadcast:
            break
        }
    }
}

/// The generated `EventListener` protocol has no closure-based adapter,
/// so this wraps one.
private final class ClosureEventListener: EventListener {
    private let handler: (FfiSyncEvent) -> Void

    init(_ handler: @escaping (FfiSyncEvent) -> Void) {
        self.handler = handler
    }

    func onEvent(event: FfiSyncEvent) {
        handler(event)
    }
}

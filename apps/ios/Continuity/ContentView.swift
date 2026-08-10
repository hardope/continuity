import SwiftUI
import UniformTypeIdentifiers

extension Color {
    /// The same accent used everywhere else in the project (tray icon,
    /// Android launcher icon, Compose theme).
    static let continuityBlue = Color(red: 0x2F / 255, green: 0x81 / 255, blue: 0xF7 / 255)
}

extension ActivityTint {
    var color: Color {
        switch self {
        case .neutral: return .secondary
        case .success: return .green
        case .warning: return .orange
        }
    }
}

struct ContentView: View {
    @EnvironmentObject var controller: EngineController
    @State private var showingFilePicker = false

    var body: some View {
        NavigationStack {
            List {
                Section {
                    StatusRow(connectedPeer: controller.connectedPeer)
                }

                Section {
                    Button {
                        showingFilePicker = true
                    } label: {
                        Label(
                            controller.connectedPeer.map { "Send file to \($0.name)" } ?? "Send file...",
                            systemImage: "square.and.arrow.up"
                        )
                    }
                    .disabled(controller.connectedPeer == nil)
                } footer: {
                    Label(shortId(controller.deviceId), systemImage: "antenna.radiowaves.left.and.right")
                        .font(.footnote.monospaced())
                        .foregroundStyle(.secondary)
                }

                Section("Activity") {
                    if controller.activity.isEmpty {
                        Text("No activity yet")
                            .foregroundStyle(.secondary)
                            .font(.subheadline)
                    } else {
                        ForEach(controller.activity) { entry in
                            Label {
                                Text(entry.text).font(.subheadline)
                            } icon: {
                                Image(systemName: entry.symbol)
                                    .foregroundStyle(entry.tint.color)
                            }
                        }
                    }
                }
            }
            .navigationTitle("Continuity")
            .tint(.continuityBlue)
        }
        .onAppear { controller.start() }
        .fileImporter(isPresented: $showingFilePicker, allowedContentTypes: [.item]) { result in
            if case .success(let url) = result {
                controller.sendFile(at: url)
            }
        }
        .alert(
            "Pairing request",
            isPresented: Binding(
                get: { controller.pendingPairing != nil },
                set: { if !$0 { controller.pendingPairing = nil } }
            ),
            presenting: controller.pendingPairing
        ) { _ in
            Button("Yes, it matches") { controller.confirmPairing(accept: true) }
            Button("No", role: .cancel) { controller.confirmPairing(accept: false) }
        } message: { pending in
            Text("'\(pending.peer.name)' wants to pair.\n\nConfirmation code: \(pending.code)\n\nDoes this match the code shown on the other device?")
        }
    }

    private func shortId(_ id: String) -> String {
        id.count > 12 ? "This device · \(id.prefix(12))…" : id
    }
}

private struct StatusRow: View {
    let connectedPeer: FfiDeviceInfo?

    var body: some View {
        HStack(spacing: 12) {
            if let peer = connectedPeer {
                Image(systemName: "checkmark.circle.fill")
                    .foregroundStyle(.green)
                    .font(.title2)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Connected").font(.caption).foregroundStyle(.secondary)
                    Text(peer.name).font(.headline)
                }
            } else {
                ProgressView()
                    .controlSize(.small)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Waiting").font(.caption).foregroundStyle(.secondary)
                    Text("No device connected").font(.headline)
                }
            }
            Spacer()
        }
        .padding(.vertical, 4)
    }
}

import AppKit
import SwiftUI

// The building blocks every page is made of. macOS 13 brought the grouped
// settings form (and NavigationSplitView, see RootView); macOS 12 has
// neither, so there the same pages are laid out by hand — sections as
// rounded boxes on a scrolling page — from views that exist on both.

private struct LegacyLayoutKey: EnvironmentKey {
    static let defaultValue = false
}

extension EnvironmentValues {
    /// The macOS 12 layout. Pages set it for their rows when they fall back
    /// to it; setting it from outside previews that layout on a newer Mac.
    public var legacyLayout: Bool {
        get { self[LegacyLayoutKey.self] }
        set { self[LegacyLayoutKey.self] = newValue }
    }
}

/// A scrolling page of `PageSection`s.
struct Page<Content: View>: View {
    @Environment(\.legacyLayout) private var legacyLayout
    @ViewBuilder var content: Content

    var body: some View {
        if #available(macOS 13.0, *), !legacyLayout {
            Form { content }
                .formStyle(.grouped)
        } else {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) { content }
                    .padding(20)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .background(Color(nsColor: .windowBackgroundColor))
            .environment(\.legacyLayout, true)
        }
    }
}

/// A form section, or a titled box with a caption under it.
struct PageSection<Content: View, Footer: View>: View {
    @Environment(\.legacyLayout) private var legacyLayout
    var title: String?
    @ViewBuilder var content: Content
    @ViewBuilder var footer: Footer

    var body: some View {
        if legacyLayout {
            VStack(alignment: .leading, spacing: 6) {
                if let title {
                    Text(title)
                        .font(.headline)
                        .padding(.leading, 4)
                }
                VStack(alignment: .leading, spacing: 12) { content }
                    .padding(12)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(RoundedRectangle(cornerRadius: 8).fill(Color(nsColor: .controlBackgroundColor)))
                    .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color(nsColor: .separatorColor)))
                footer
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 4)
            }
        } else {
            Section {
                content
            } header: {
                if let title {
                    Text(title)
                }
            } footer: {
                footer
            }
        }
    }
}

extension PageSection where Footer == EmptyView {
    init(title: String? = nil, @ViewBuilder content: () -> Content) {
        self.init(title: title, content: content, footer: { EmptyView() })
    }
}

/// A switch with a title and a line of explanation.
struct ToggleRow: View {
    @Environment(\.legacyLayout) private var legacyLayout
    let title: String
    let detail: String
    @Binding var isOn: Bool

    var body: some View {
        if legacyLayout {
            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 8)
                Toggle(title, isOn: $isOn)
                    .labelsHidden()
                    .toggleStyle(.switch)
            }
        } else {
            Toggle(isOn: $isOn) {
                Text(title)
                Text(detail)
            }
        }
    }
}

/// A label on the left, its value on the right.
struct InfoRow: View {
    let label: String
    let value: String

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(label)
            Spacer(minLength: 8)
            Text(value)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)
        }
    }
}

/// A file or folder, with a button that shows it.
struct PathRow: View {
    let label: String
    let path: String
    let show: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text(label)
                Text(abbreviatingHome(path))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
            }
            Spacer(minLength: 8)
            Button("Show in Finder", action: show)
        }
    }
}

/// `/Users/me/Downloads/Continuity` → `~/Downloads/Continuity`.
func abbreviatingHome(_ path: String) -> String {
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    return path.hasPrefix(home) ? "~" + path.dropFirst(home.count) : path
}

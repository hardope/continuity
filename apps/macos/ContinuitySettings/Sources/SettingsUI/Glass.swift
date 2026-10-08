import SwiftUI

// Liquid Glass where it exists, the classic look where it doesn't.
//
// Most of the window gets Liquid Glass without asking: on macOS 26, built
// with the macOS 26 SDK, the sidebar, toolbar and grouped forms adopt it
// on their own, and on earlier macOS the same code renders the familiar
// way. Glass is only applied by hand to controls (buttons, the icon
// badge) — never to content, per Apple's guidance — and only behind both
// checks: the compiler (Swift 6.2 ships with the SDK that has the API) and
// the running macOS.

private struct GlassDisabledKey: EnvironmentKey {
    static let defaultValue = false
}

extension EnvironmentValues {
    /// Forces the classic look even where Liquid Glass is available — for
    /// rendering previews offscreen, where glass (composited by the window
    /// server) can't be drawn.
    public var glassDisabled: Bool {
        get { self[GlassDisabledKey.self] }
        set { self[GlassDisabledKey.self] = newValue }
    }
}

extension View {
    /// The main action on a page.
    func primaryActionStyle() -> some View {
        modifier(ActionStyle(prominent: true))
    }

    /// Any other action next to it.
    func secondaryActionStyle() -> some View {
        modifier(ActionStyle(prominent: false))
    }

    /// The disc behind a device's icon in a page header.
    func iconBadge(tint: Color) -> some View {
        modifier(IconBadge(tint: tint))
    }
}

private struct ActionStyle: ViewModifier {
    let prominent: Bool
    @Environment(\.glassDisabled) private var glassDisabled

    func body(content: Content) -> some View {
        #if compiler(>=6.2)
        if #available(macOS 26.0, *), !glassDisabled {
            if prominent {
                content.buttonStyle(.glassProminent)
            } else {
                content.buttonStyle(.glass)
            }
        } else {
            classic(content)
        }
        #else
        classic(content)
        #endif
    }

    @ViewBuilder
    private func classic(_ content: Content) -> some View {
        if prominent {
            content.buttonStyle(.borderedProminent)
        } else {
            content.buttonStyle(.bordered)
        }
    }
}

private struct IconBadge: ViewModifier {
    let tint: Color
    @Environment(\.glassDisabled) private var glassDisabled

    func body(content: Content) -> some View {
        #if compiler(>=6.2)
        if #available(macOS 26.0, *), !glassDisabled {
            content.glassEffect(.regular.tint(tint.opacity(0.3)), in: Circle())
        } else {
            content.background(tint.opacity(0.16), in: Circle())
        }
        #else
        content.background(tint.opacity(0.16), in: Circle())
        #endif
    }
}

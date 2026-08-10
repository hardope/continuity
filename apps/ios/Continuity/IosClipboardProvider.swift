import UIKit

/// Bridges the engine's clipboard hooks to `UIPasteboard`.
///
/// `setText` (applying a peer's update) is unrestricted and fully wired up
/// — receiving clipboard updates from paired devices works normally.
///
/// `getText` deliberately always returns `nil`, disabling the automatic
/// "watch and broadcast" direction for now. The engine's watcher (shared
/// with every platform, see `continuity-daemon`) polls every 500ms; on iOS
/// each `UIPasteboard` read shows the system's "Continuity pasted from
/// Clipboard" privacy banner, so polling at that rate would show a banner
/// twice a second whenever the app is foregrounded — a real UX bug, not a
/// rough edge. The correct fix is a deliberate, user-initiated "Send
/// Clipboard" action instead of passive polling, which needs a new engine
/// command (`EngineCommand` currently only has `ConfirmPairing` and
/// `SendFile`) — left for a follow-up rather than shipping the broken
/// polling behavior.
final class IosClipboardProvider: ClipboardProvider {
    func getText() -> String? {
        nil
    }

    func setText(text: String) {
        UIPasteboard.general.string = text
    }
}

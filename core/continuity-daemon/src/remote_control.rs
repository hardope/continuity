use continuity_proto::InputEventKind;
use tokio::sync::mpsc;

/// Injects input events and captures the screen on the *controlled* side
/// of a remote-control session. Pluggable like `ClipboardBackend`/
/// `MediaController` — only macOS and Windows have real implementations
/// (`continuityd`'s `MacRemoteControlHost`/`WindowsRemoteControlHost`,
/// compiled in only behind the `remote-control` Cargo feature there, see
/// `core/continuityd/Cargo.toml`); every other shell (Android/iOS, Linux,
/// and any desktop build with that feature disabled for a "lite" release)
/// wires in `NoopRemoteControlHost`.
///
/// Being a paired, trusted peer is not the same thing as being allowed to
/// take over this device's keyboard, mouse, and screen — every real
/// implementation still requires a fresh, explicit accept per session
/// (see `SyncEvent::RemoteControlRequested` / `EngineCommand::
/// RespondToRemoteControlRequest` in `engine.rs`); this trait is purely
/// about *whether the platform is capable at all*, not about consent.
pub trait RemoteControlHost: Send + Sync {
    /// `false` means this host can't be remotely controlled at all — the
    /// engine auto-declines any `RemoteControlRequest` without bothering
    /// the local user with a prompt for a capability that doesn't exist
    /// here (Android/iOS, a Linux desktop, or a "lite" build).
    fn is_available(&self) -> bool {
        true
    }

    /// Injects one input event. Only ever called for an event that
    /// already passed the engine's active-session check (see
    /// `handle_connection_inner`'s `InputEvent` handling) — this trait
    /// doesn't need its own consent logic.
    fn inject(&self, event: InputEventKind);

    /// Starts screen capture for a newly-accepted session, returning a
    /// channel of JPEG-encoded frames. `None` if capture couldn't start
    /// at all (e.g. Screen Recording permission not granted) — the
    /// engine treats that the same as the peer having declined, rather
    /// than silently sending no frames forever.
    fn start_capture(&self) -> Option<mpsc::Receiver<Vec<u8>>>;

    /// Stops any capture in progress. Safe to call even if nothing was
    /// started (e.g. `start_capture` already returned `None`).
    fn stop_capture(&self);
}

pub struct NoopRemoteControlHost;

impl RemoteControlHost for NoopRemoteControlHost {
    fn is_available(&self) -> bool {
        false
    }

    fn inject(&self, _event: InputEventKind) {}

    fn start_capture(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        None
    }

    fn stop_capture(&self) {}
}

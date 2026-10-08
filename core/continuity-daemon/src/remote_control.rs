use continuity_proto::InputEventKind;
use tokio::sync::mpsc;

/// Injects input events and captures the screen on the *controlled* side
/// of a remote-control session. Pluggable like `ClipboardBackend`/
/// `MediaController` — macOS, Windows and Linux have real implementations
/// (`continuityd`'s `MacRemoteControlHost`/`WindowsRemoteControlHost`/
/// `LinuxRemoteControlHost`, compiled in only behind the `remote-control`
/// Cargo feature there, see `core/continuityd/Cargo.toml`); every other
/// shell (Android/iOS, and any desktop build with that feature disabled
/// for a "lite" release) wires in `NoopRemoteControlHost`.
///
/// Being a paired, trusted peer is not the same thing as being allowed to
/// take over this device's keyboard, mouse, and screen — the engine asks
/// the local user before the first session with each peer (see
/// `SyncEvent::RemoteControlRequested` / `EngineCommand::
/// RespondToRemoteControlRequest` in `engine.rs`); this trait is purely
/// about *whether the platform is capable at all*, not about consent.
pub trait RemoteControlHost: Send + Sync {
    /// `false` means this host can't be remotely controlled at all — the
    /// engine auto-declines any `RemoteControlRequest` without bothering
    /// the local user with a prompt for a capability that doesn't exist
    /// here (Android/iOS, or a "lite" build).
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

    /// Why the channel from the latest `start_capture` closed, if the host
    /// knows — the engine asks when that happens and passes the answer on
    /// to the controlling device, instead of the session just ending
    /// without explanation. Matters for hosts whose capture can still fail
    /// *after* `start_capture` returned a channel: on Linux, the desktop
    /// itself asks its user to approve screen sharing, who can decline or
    /// not be there at all.
    fn capture_failure_reason(&self) -> Option<String> {
        None
    }
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

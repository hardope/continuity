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

    /// Whether the frames of the next `start_capture` should show this
    /// computer's pointer. Yes for a phone, which has no pointer of its own
    /// on the picture; no for a desktop viewer, which shows its own where
    /// the mouse is — the one drawn into a picture that updates a few dozen
    /// times a second would only trail behind it. Called just before
    /// `start_capture`; a host that can't leave it out ignores it.
    fn set_pointer_in_frames(&self, _include: bool) {}

    /// Stops any capture in progress. Safe to call even if nothing was
    /// started (e.g. `start_capture` already returned `None`). Only for a
    /// session with the screen; keyboard-and-mouse sharing ends with
    /// `stop_input_only`.
    fn stop_capture(&self);

    /// Gets ready to take input without capturing the screen — a
    /// keyboard-and-mouse sharing session (`RemoteControlRequest { screen:
    /// false }`). macOS and Windows inject with nothing to set up; Linux
    /// opens a portal session without a screen source. `Err` is a reason
    /// the engine ends the request with. Called when the first such session
    /// starts: several computers can share their keyboard and mouse with
    /// this one at once, and with a screen session alongside.
    fn start_input_only(&self) -> Result<(), String> {
        Ok(())
    }

    /// The last keyboard-and-mouse sharing session has ended: lets go of
    /// whatever was left held down, and (Linux) ends the portal session.
    fn stop_input_only(&self) {}

    /// Why input for keyboard-and-mouse sharing can't be injected any more,
    /// when that happened on its own after `start_input_only` succeeded —
    /// on Linux, the desktop's own dialog was declined or never answered,
    /// or remote interaction was stopped from the desktop's controls. The
    /// engine asks every so often while such sessions are up, and ends them
    /// with this reason.
    fn input_only_failure(&self) -> Option<String> {
        None
    }

    /// This screen's size in pixels, for a controller forwarding its own
    /// mouse (`InputEventKind::MouseMoveRelative` is in these pixels, and
    /// the controller needs to know where the edges are). `None` if the
    /// host can't say.
    fn screen_size(&self) -> Option<(u32, u32)> {
        None
    }

    /// Where the pointer is pressed against an edge of the screen right
    /// now, and how far along it (`0.0..=1.0` from the top or the left) —
    /// asked after each `MouseMoveRelative` the engine injects, so the
    /// controlling computer can take its cursor back when it reaches the
    /// far side. `None` when it isn't at an edge, or the host can't tell.
    fn pointer_at_edge(&self) -> Option<(continuity_proto::ScreenEdge, f64)> {
        None
    }

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

/// Why a lock/unlock attempt didn't happen — kept to the two cases a
/// requester can actually do something different about, so they map
/// one-to-one onto `continuity_proto::ScreenLockOutcome`'s non-success
/// variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenLockError {
    /// This device has no supported way to do it at all (no session
    /// manager to ask, or a platform with no public API for it).
    Unsupported(String),
    /// Supported in principle, but this attempt failed.
    Failed(String),
}

/// Locks or unlocks this device's own screen on request from a paired peer
/// (`Message::ScreenLockRequest`). Pluggable like `MediaController` — only
/// Linux has a real implementation (`continuityd`'s `LinuxScreenLock`,
/// which asks systemd-logind, the same thing `loginctl lock-session` /
/// `unlock-session` do). Every other shell wires in
/// `NoopScreenLockController`, and the engine answers `Unsupported` for it
/// without bothering anyone.
///
/// Like `RemoteControlHost`, this trait is only about *capability*.
/// Whether a given peer is allowed to unlock is decided by the engine
/// against the trust store before this is ever called.
///
/// Both methods may block (D-Bus round trips, plus waiting to see whether
/// the lock screen actually went away) — the engine always calls them
/// from `spawn_blocking`, never directly on an async task.
pub trait ScreenLockController: Send + Sync {
    /// `false` means this device can't be locked/unlocked remotely at all.
    fn is_available(&self) -> bool {
        true
    }

    fn lock(&self) -> Result<(), ScreenLockError>;

    fn unlock(&self) -> Result<(), ScreenLockError>;
}

pub struct NoopScreenLockController;

impl ScreenLockController for NoopScreenLockController {
    fn is_available(&self) -> bool {
        false
    }

    fn lock(&self) -> Result<(), ScreenLockError> {
        Err(ScreenLockError::Unsupported("not supported on this platform".to_string()))
    }

    fn unlock(&self) -> Result<(), ScreenLockError> {
        Err(ScreenLockError::Unsupported("not supported on this platform".to_string()))
    }
}

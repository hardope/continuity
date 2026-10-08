//! Windows remote lock: `LockWorkStation`, the same thing ⊞ Win+L does —
//! a documented API any desktop app can call for its own session.
//!
//! **Unlock isn't offered.** Windows has no API for an app to dismiss the
//! lock screen. The only supported route is a credential provider, a
//! sign-in-screen plugin that would need the user's password stored for it
//! and an administrator install (Continuity installs per-user). `unlock`
//! answers `Unsupported`, and `can_unlock` says so up front.
//!
//! **Not verified on a real Windows machine** — compile-checked in CI
//! only, like the rest of the Windows-specific code.

use continuity_daemon::{ScreenLockController, ScreenLockError};
use windows::Win32::System::Shutdown::LockWorkStation;

pub struct WindowsScreenLock;

impl ScreenLockController for WindowsScreenLock {
    fn can_unlock(&self) -> bool {
        false
    }

    fn lock(&self) -> Result<(), ScreenLockError> {
        // Returns once locking has started — the lock itself is
        // asynchronous. It fails only for a process outside the interactive
        // desktop (a service, say), which continuityd never is.
        unsafe { LockWorkStation() }.map_err(|e| ScreenLockError::Failed(format!("Windows didn't lock the screen: {e}")))
    }

    fn unlock(&self) -> Result<(), ScreenLockError> {
        Err(ScreenLockError::Unsupported("Windows doesn't let apps unlock the screen".to_string()))
    }
}

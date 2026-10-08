//! macOS remote lock: locks this Mac's screen when a paired device asks.
//!
//! **Unlock isn't offered.** macOS has no supported way for an app to
//! dismiss its lock screen (Apple Watch unlock is private to Apple), and
//! the workaround some apps use — storing the login password and typing
//! it into the lock screen with synthetic keystrokes — breaks across macOS
//! updates and can't help after a restart with FileVault. `unlock` answers
//! `Unsupported`, and `can_unlock` says so up front.
//!
//! Locking calls `SACLockScreenImmediate` from the private `login.framework`
//! — what lock-screen utilities have used for years, with the same effect
//! as the Apple menu's Lock Screen. It's looked up at runtime (`dlopen`/
//! `dlsym`, like `media_mac.rs` does for MediaRemote), so a macOS without
//! it fails cleanly instead of at launch. The fallback is pressing that
//! menu item's own shortcut, ⌃⌘Q, as synthetic key events — which needs
//! the Accessibility permission media control already asks for.
//!
//! Either way, the result is confirmed from the session's own
//! `CGSSessionScreenIsLocked` flag (public, via `CGSessionCopyCurrentDictionary`)
//! rather than assumed from the call returning.

use continuity_daemon::{ScreenLockController, ScreenLockError};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventTapLocation, CGSessionCopyCurrentDictionary};
use std::ffi::{c_void, CStr};
use std::time::{Duration, Instant};

/// How long the screen gets to report itself locked after asking.
const LOCK_CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);

/// `kVK_ANSI_Q` (HIToolbox's Events.h).
const KEY_Q: u16 = 0x0C;

type LockScreenFn = unsafe extern "C" fn() -> i32;

pub struct MacScreenLock;

impl ScreenLockController for MacScreenLock {
    fn can_unlock(&self) -> bool {
        false
    }

    fn lock(&self) -> Result<(), ScreenLockError> {
        if screen_is_locked() {
            return Ok(());
        }
        match lock_screen_function() {
            Some(lock_screen) => {
                let status = unsafe { lock_screen() };
                tracing::debug!("SACLockScreenImmediate returned {status}");
            }
            None => {
                tracing::info!("SACLockScreenImmediate isn't available; locking with ⌃⌘Q instead");
                crate::media_mac::ensure_accessibility_trust();
                press_lock_shortcut();
            }
        }
        let deadline = Instant::now() + LOCK_CONFIRM_TIMEOUT;
        while Instant::now() < deadline {
            if screen_is_locked() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(ScreenLockError::Failed(
            "the screen didn't lock (check that Continuity is allowed under System Settings > Privacy & Security > Accessibility)".to_string(),
        ))
    }

    fn unlock(&self) -> Result<(), ScreenLockError> {
        Err(ScreenLockError::Unsupported("macOS doesn't let apps unlock the screen".to_string()))
    }
}

/// `SACLockScreenImmediate` from the private `login.framework`, if this
/// macOS still has it.
fn lock_screen_function() -> Option<LockScreenFn> {
    unsafe {
        let path = CStr::from_bytes_with_nul(b"/System/Library/PrivateFrameworks/login.framework/login\0").unwrap();
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY);
        if handle.is_null() {
            return None;
        }
        let name = CStr::from_bytes_with_nul(b"SACLockScreenImmediate\0").unwrap();
        let symbol = libc::dlsym(handle, name.as_ptr());
        (!symbol.is_null()).then(|| std::mem::transmute::<*mut c_void, LockScreenFn>(symbol))
    }
}

/// The Apple menu's Lock Screen shortcut, ⌃⌘Q, as synthetic key events.
fn press_lock_shortcut() {
    for key_down in [true, false] {
        let Some(event) = CGEvent::new_keyboard_event(None, KEY_Q, key_down) else {
            tracing::debug!("couldn't create the ⌃⌘Q key event");
            return;
        };
        CGEvent::set_flags(Some(&event), CGEventFlags::MaskControl | CGEventFlags::MaskCommand);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
}

/// Whether this login session's screen is locked right now: the
/// `CGSSessionScreenIsLocked` entry of the session dictionary, which is
/// only present (and true) while it is.
fn screen_is_locked() -> bool {
    let Some(session) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    // The same typed view `media_mac.rs` takes of MediaRemote's dictionary.
    let session = unsafe { &*(CFRetained::as_ptr(&session).as_ptr() as *const CFDictionary<CFString, CFType>) };
    let Some(value) = session.get(&CFString::from_str("CGSSessionScreenIsLocked")) else {
        return false;
    };
    if let Some(flag) = value.downcast_ref::<CFBoolean>() {
        flag.as_bool()
    } else {
        value.downcast_ref::<CFNumber>().and_then(|n| n.as_i64()).is_some_and(|n| n != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolves the private function without calling it — calling it would
    /// lock the screen of whoever runs the tests.
    #[test]
    fn the_lock_function_is_present_on_this_macos() {
        assert!(lock_screen_function().is_some(), "SACLockScreenImmediate is missing; locking would fall back to ⌃⌘Q");
    }

    /// Reads the session's lock state without changing it. A process with
    /// no login session (some CI runners) has nothing to read; that's not
    /// what this checks, so it's skipped there.
    #[test]
    fn the_session_lock_state_is_readable() {
        if CGSessionCopyCurrentDictionary().is_none() {
            eprintln!("no login session here — nothing to read");
            return;
        }
        let _ = screen_is_locked();
    }

    /// Actually locks this Mac's screen — run by hand:
    /// `cargo test -p continuityd -- --ignored locks_this_mac`
    #[test]
    #[ignore]
    fn locks_this_mac() {
        MacScreenLock.lock().expect("locks");
        assert!(screen_is_locked());
    }
}

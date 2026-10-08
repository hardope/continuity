//! The macOS privacy permissions Continuity needs, for the settings window to
//! show and help grant (see control.rs): Screen Recording, for what a
//! controlling device sees (remote_control_mac.rs), and Accessibility, for
//! its keyboard and mouse and for a phone's play/pause/next (media_mac.rs).
//!
//! macOS only lists an app under those settings once the app has asked for
//! them, so the window's "Grant…" asks first — the system's own prompt, the
//! first time — and then opens System Settings at the page where it's
//! switched on.

use crate::control::{Permission, Permissions};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef) -> u8;
}

/// Cheap enough to poll: the settings window shows a permission switched on
/// within a second or so of it happening.
pub fn current() -> Permissions {
    Permissions { screen_recording: CGPreflightScreenCaptureAccess(), accessibility: unsafe { AXIsProcessTrusted() } != 0 }
}

pub fn request(permission: Permission) {
    let (granted, page) = match permission {
        Permission::ScreenRecording => (CGRequestScreenCaptureAccess(), "Privacy_ScreenCapture"),
        Permission::Accessibility => (prompt_for_accessibility(), "Privacy_Accessibility"),
    };
    if granted {
        return;
    }
    if let Err(e) = std::process::Command::new("/usr/bin/open").arg(format!("x-apple.systempreferences:com.apple.preference.security?{page}")).spawn() {
        tracing::warn!("couldn't open System Settings: {e}");
    }
}

/// The prompting form of the check — the only one that adds Continuity to
/// the Accessibility list.
fn prompt_for_accessibility() -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    let options = CFDictionary::from_CFType_pairs(&[(CFString::new("AXTrustedCheckOptionPrompt"), CFBoolean::true_value())]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0 }
}

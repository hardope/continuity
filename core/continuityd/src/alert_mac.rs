//! Questions on macOS (pairing, remote control) as an alert window, through
//! CoreFoundation's `CFUserNotification`.
//!
//! **Real bug fixed here**: from 0.1.8 these were notifications with buttons
//! (`notifications_mac`), so that they wouldn't freeze the tray's event loop
//! the way a modal dialog on its thread did. But whether a notification is
//! seen at all depends on settings Continuity can't read or change. On a Mac
//! with Continuity's notification style set to None, or with a Focus on,
//! macOS accepted every pairing request and filed it in Notification Center
//! without a banner. The system log showed `alertStyle:"none"` and
//! `interruptionSuppression: delay delivery`. Nothing asked anyone, and the
//! engine declined each request after three minutes. Posting never failed,
//! so the dialog fallback never ran. A pairing code has to be compared on
//! screen, so asking can't depend on notification settings.
//!
//! `CFUserNotification` is built for this: the system's
//! UserNotificationCenter agent draws the alert in a window of its own, in
//! front, whatever the Focus or notification settings. Only the calling
//! thread waits for the answer, so the tray, the viewer and the window's
//! requests carry on while it's up. It doesn't need a bundle, so a
//! development build asks the same way.

use objc2_core_foundation::{
    kCFUserNotificationDefaultResponse, kCFUserNotificationNoteAlertLevel, CFOptionFlags, CFRetained, CFString,
    CFUserNotification, CFURL,
};
use std::time::Duration;

/// Asks a yes/no question in an alert window and blocks the calling thread
/// until it's answered or `timeout` passes. Returns `Some(true)` only for a
/// click on `yes`, and `Some(false)` for `no` or a timeout. Returns `None`
/// if the alert couldn't be shown at all, so the caller should ask another
/// way.
pub fn ask(title: &str, body: &str, yes: &str, no: &str, timeout: Duration) -> Option<bool> {
    let icon = app_icon();
    let mut response: CFOptionFlags = 0;
    // SAFETY: every argument is a valid CF object or None (each is
    // optional), and `response` outlives the call.
    let error = unsafe {
        CFUserNotification::display_alert(
            timeout.as_secs_f64(),
            kCFUserNotificationNoteAlertLevel,
            icon.as_deref(),
            None,
            None,
            Some(&CFString::from_str(title)),
            Some(&CFString::from_str(body)),
            // The default button is the right-hand one, highlighted. The
            // alternate sits to its left.
            Some(&CFString::from_str(yes)),
            Some(&CFString::from_str(no)),
            None,
            &mut response,
        )
    };
    if error != 0 {
        tracing::warn!("couldn't show the alert (CFUserNotification error {error})");
        return None;
    }
    // The low two bits say which button was clicked. A timeout comes back
    // as the cancel response.
    Some(response & 0b11 == kCFUserNotificationDefaultResponse)
}

/// The bundle's icon for the alert to show. `None` for an unbundled build,
/// which gets the generic note icon instead.
fn app_icon() -> Option<CFRetained<CFURL>> {
    let exe = std::env::current_exe().ok()?;
    // Continuity.app/Contents/MacOS/continuityd -> Contents/Resources.
    let icon = exe.parent()?.parent()?.join("Resources").join("AppIcon.icns");
    if icon.is_file() {
        CFURL::from_file_path(&icon)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual probe: puts a real alert on screen, so it's `#[ignore]`d.
    /// Leave it alone: it should close itself and count as no.
    /// `cargo test -p continuityd -- --ignored an_unanswered_alert_counts_as_no`
    #[test]
    #[ignore]
    fn an_unanswered_alert_counts_as_no() {
        let answer = ask(
            "Continuity — test alert",
            "This is a test. Don't click anything: it closes itself in 5 seconds.",
            "Yes",
            "No",
            Duration::from_secs(5),
        );
        assert_eq!(answer, Some(false));
    }
}

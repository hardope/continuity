//! Notifications on macOS, through Apple's UserNotifications framework
//! (`UNUserNotificationCenter`, via mac-usernotifications).
//!
//! **Real bug fixed here**: these used to go through notify-rust, which on
//! macOS posts with the older `NSUserNotificationCenter`. An app using that
//! never asks to notify, and macOS now treats such an app as not allowed:
//! every notification was accepted and filed away without a banner (the
//! system log has each one with `authorizationStatus: Denied`), and
//! Continuity never appeared under System Settings → Notifications. In
//! testing, no Continuity notification showed at all on either Mac.
//! UserNotifications asks once (`request_permission`), lists Continuity in
//! System Settings, and shows banners even while Continuity is the active
//! app (a remote-control viewer in front, say).
//!
//! notify-rust has this backend too (its `preview-macos-un` feature), but
//! its blocking calls either turn the main run loop over by hand when called
//! on the main thread — from inside a tao event handler — or refuse to run
//! from another thread at a moment the main thread is busy, which is exactly
//! when a notification gets posted from the event loop. Here every call runs
//! on a thread of its own and only waits for its own answer; the one thing
//! that needs the main run loop, the user's response, arrives through tao's.
//!
//! An unbundled development build has no bundle identifier, which
//! UserNotifications needs (it would crash without one): every call checks
//! first and only logs.

use futures_lite::future;
use mac_usernotifications::{Action, Notification, NotificationResponse};
use std::time::Duration;

/// How often a notification that's waiting for a click checks whether it's
/// still in Notification Center. A notification cleared with "Clear All"
/// never says so, and without this its thread would wait forever.
const STILL_THERE_POLL: Duration = Duration::from_secs(10);

/// Asks macOS to let Continuity notify. The first time, macOS shows its own
/// prompt — on macOS 27 a banner, with Allow under its Options button;
/// closing it or clicking the banner itself counts as Don't Allow. After
/// that it answers at once, with whatever's set in System Settings →
/// Notifications → Continuity.
pub fn request_permission() {
    std::thread::spawn(|| match future::block_on(mac_usernotifications::request_auth()) {
        Ok(true) => tracing::debug!("notifications are allowed"),
        // Also what a refused request comes back as (macOS couldn't find
        // the app, say), so not necessarily someone's choice.
        Ok(false) => tracing::info!("notifications aren't allowed for Continuity (System Settings → Notifications → Continuity)"),
        Err(e) => tracing::debug!("couldn't ask to notify (expected for an unbundled binary): {e}"),
    });
}

/// Posts a notification and returns at once.
pub fn show(title: &str, body: &str) {
    let notification = Notification::new().title(title).message(body);
    std::thread::spawn(move || {
        if let Err(e) = future::block_on(mac_usernotifications::send(notification)) {
            tracing::debug!("notification not shown (expected for an unbundled binary): {e}");
        }
    });
}

/// Posts a notification with buttons (`(id, label)` pairs) and returns at
/// once. `on_action` gets the id of the button clicked, or `"default"` for
/// a click on the notification itself; it isn't called when it's dismissed
/// or cleared.
pub fn show_with_actions(title: &str, body: &str, actions: &[(&str, &str)], on_action: impl FnOnce(&str) + Send + 'static) {
    let mut notification = Notification::new().title(title).message(body);
    for (id, label) in actions {
        notification = notification.action(Action::button(*id, *label));
    }
    std::thread::spawn(move || {
        let response = future::block_on(async {
            let handle = notification.send().await?;
            let id = handle.notification_id().to_string();
            Ok::<_, mac_usernotifications::Error>(future::or(async { handle.response().await.ok() }, gone_from_notification_center(id)).await)
        });
        match response {
            Ok(Some(response)) => {
                if let Some(action) = clicked(&response) {
                    on_action(action);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::debug!("notification not shown (expected for an unbundled binary): {e}"),
        }
    });
}

/// What was clicked, if anything: a button's id, or `"default"` for the
/// notification itself.
fn clicked(response: &NotificationResponse) -> Option<&str> {
    if response.is_dismiss_action() || response.is_timed_out() {
        None
    } else if response.is_default_action() {
        Some("default")
    } else {
        Some(&response.action_identifier)
    }
}

/// Resolves (with `None`, nothing clicked) once notification `id` is no
/// longer in Notification Center.
async fn gone_from_notification_center(id: String) -> Option<NotificationResponse> {
    loop {
        futures_timer::Delay::new(STILL_THERE_POLL).await;
        if !mac_usernotifications::get_delivered_notification_ids().await.contains(&id) {
            return None;
        }
    }
}

//! The xdg-desktop-portal half of Linux remote control: asks the desktop
//! for a combined `RemoteDesktop` + `ScreenCast` session (input injection
//! plus one monitor's PipeWire video stream), then carries that session's
//! input events. Under Wayland this is the only sanctioned way for an app
//! to see the screen or inject input (XTest doesn't work there at all), and
//! it's what GNOME and KDE Plasma both implement.
//!
//! Every portal method that can involve the user answers in two steps: the
//! call returns a `Request` object path straight away, and the real result
//! arrives later as that object's `Response` signal. Setup steps answer
//! instantly — the signal can beat the call's own reply — so the listener
//! has to exist *before* the call, which means predicting the request's
//! path. That's only possible because the caller picks the path's last
//! element by passing `handle_token`.
//!
//! **Real bug fixed here:** the first version predicted a path from a
//! token it never actually passed, so the portal picked its own and
//! answered somewhere nobody was listening. The very first call
//! (`CreateSession`) waited forever — and so did the controlling phone's
//! "connecting" spinner, since its side of the session had already been
//! accepted. It also built the session's path without the sender element.
//! Paths here follow the documented scheme (and `ashpd`, the most widely
//! used Rust portal client): `/org/freedesktop/portal/desktop/{request,
//! session}/SENDER/TOKEN`, SENDER being this connection's unique name
//! without the `:` and with `.` replaced by `_`.
//!
//! Kept free of PipeWire so it builds, and can be exercised against a mock
//! portal over a real D-Bus connection, anywhere zbus does.

use futures_util::StreamExt;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::watch;
use zbus::zvariant::{self, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREEN_CAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

// Bit values and modes as the portal specification numbers them.
const DEVICE_KEYBOARD: u32 = 1;
const DEVICE_POINTER: u32 = 2;
const SOURCE_MONITOR: u32 = 1;
const CURSOR_MODE_EMBEDDED: u32 = 2;
const PERSIST_UNTIL_REVOKED: u32 = 2;

/// Setup steps that never involve a person (creating the session, picking
/// devices and sources) answer within milliseconds; this only bounds a
/// portal that has stopped responding.
const SETUP_TIMEOUT: Duration = Duration::from_secs(20);

/// `Start` is the step that shows the person at this computer the system's
/// "allow remote control?" dialog. Long enough to walk over and answer it,
/// short enough that the controlling device isn't left waiting on an empty
/// room indefinitely.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// No usable portal: none running, or none implementing `RemoteDesktop`
    /// (wlroots' portal, for one, only does `ScreenCast`).
    Unavailable,
    /// The person at this computer declined, or closed the dialog.
    Declined,
    /// Nobody answered in time.
    TimedOut,
    /// The session was ended from the controlling side before setup finished.
    Cancelled,
    /// Anything else the portal reported.
    Failed,
}

#[derive(Debug)]
pub struct PortalError {
    pub kind: ErrorKind,
    /// The portal step that failed — for the log, and for deciding whether
    /// a retry without session persistence is worth it.
    pub step: &'static str,
    /// Worded for the person on the controlling device, who sees it as
    /// "remote control ended: <reason>".
    pub reason: String,
}

impl PortalError {
    fn new(kind: ErrorKind, step: &'static str, reason: impl Into<String>) -> Self {
        Self { kind, step, reason: reason.into() }
    }
}

impl std::fmt::Display for PortalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({:?} at {})", self.reason, self.kind, self.step)
    }
}

fn failed(step: &'static str, error: impl std::fmt::Display) -> PortalError {
    PortalError::new(ErrorKind::Failed, step, format!("the desktop's screen-sharing service failed at {step}: {error}"))
}

/// A started session: what input injection talks to, and what gets closed
/// when it's over.
pub struct PortalSession {
    connection: Connection,
    handle: OwnedObjectPath,
    /// The PipeWire node of the monitor stream the person picked.
    pub node_id: u32,
    /// That stream's size in the compositor's logical coordinate space, if
    /// the portal reported one.
    pub logical_size: Option<(i32, i32)>,
}

pub struct Negotiated {
    pub session: PortalSession,
    /// A PipeWire remote that only exposes this session's stream.
    pub pipewire_fd: OwnedFd,
    /// What to pass as `restore_token` next time. A token, once used, is
    /// spent — so when persistence was requested and this is `None`, any
    /// stored token should be discarded.
    pub restore_token: Option<String>,
    /// Whether this session asked to persist at all. When it didn't (an
    /// older portal), a stored token wasn't used and should be left alone.
    pub persistence_requested: bool,
}

/// Runs the whole setup on the session bus. On success the person at this
/// computer has approved (or a stored approval was restored) and the
/// stream is ready to connect to.
pub async fn negotiate(restore_token: Option<&str>, stop: &watch::Receiver<bool>) -> Result<Negotiated, PortalError> {
    let connection = Connection::session()
        .await
        .map_err(|e| PortalError::new(ErrorKind::Unavailable, "connect", format!("couldn't reach this desktop's session bus: {e}")))?;
    negotiate_on(connection, restore_token, stop, APPROVAL_TIMEOUT).await
}

/// `negotiate` on an already-open bus connection, with the approval
/// timeout as a parameter — split out so a test can run it against a mock
/// portal without waiting a minute and a half.
pub async fn negotiate_on(
    connection: Connection,
    restore_token: Option<&str>,
    stop: &watch::Receiver<bool>,
    approval_timeout: Duration,
) -> Result<Negotiated, PortalError> {
    // Doubles as the check for whether there's a usable portal at all — a
    // desktop without a RemoteDesktop implementation fails right here, and
    // that's worth saying plainly instead of as a raw D-Bus error.
    let version = property_u32(&connection, REMOTE_DESKTOP_IFACE, "version").await.map_err(|e| {
        tracing::warn!("no RemoteDesktop portal: {e}");
        PortalError::new(
            ErrorKind::Unavailable,
            "RemoteDesktop",
            "this desktop doesn't let apps control it remotely (it needs xdg-desktop-portal with the GNOME or KDE Plasma backend)",
        )
    })?;
    // RemoteDesktop v2 can persist a session: once the person here has
    // approved, later sessions can be restored without asking again — the
    // same "approve once" model macOS's and Windows' permissions have.
    // Continuity's own per-device consent still runs before any of this.
    let persist = version >= 2;
    // Embedded = the compositor draws the real cursor into the frames.
    // GNOME and KDE both support it; without it the cursor is just absent.
    let embedded_cursor = property_u32(&connection, SCREEN_CAST_IFACE, "AvailableCursorModes")
        .await
        .map(|modes| modes & CURSOR_MODE_EMBEDDED != 0)
        .unwrap_or(true);
    tracing::debug!("RemoteDesktop portal v{version}, persistence {persist}, embedded cursor {embedded_cursor}");

    match start_session(&connection, persist, restore_token, embedded_cursor, stop, approval_timeout).await {
        // Asking to persist is new in v2, and a portal could refuse a
        // session for it (a restore token it no longer recognizes is
        // supposed to be ignored, but that's the portal's call). Not worth
        // failing over — the session works fine without it.
        Err(e) if persist && e.step == "SelectDevices" && e.kind == ErrorKind::Failed => {
            tracing::info!("portal rejected a persistent session ({e}); retrying without persistence");
            start_session(&connection, false, None, embedded_cursor, stop, approval_timeout).await
        }
        other => other,
    }
}

async fn start_session(
    connection: &Connection,
    persist: bool,
    restore_token: Option<&str>,
    embedded_cursor: bool,
    stop: &watch::Receiver<bool>,
    approval_timeout: Duration,
) -> Result<Negotiated, PortalError> {
    let session_token = new_token("session");
    let token = new_token("create");
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("session_handle_token", Value::from(session_token.as_str()));
    let created: HashMap<String, OwnedValue> =
        request(connection, REMOTE_DESKTOP_IFACE, "CreateSession", &token, &(options,), SETUP_TIMEOUT, stop).await?;

    let handle = match session_handle(&created) {
        Some(handle) => handle,
        None => {
            let expected = format!("{PORTAL_PATH}/session/{}/{session_token}", sender_path_element(connection, "CreateSession")?);
            tracing::warn!("CreateSession's response had no usable session_handle; assuming {expected}");
            OwnedObjectPath::try_from(expected).map_err(|e| failed("CreateSession", e))?
        }
    };

    let result = configure_and_start(connection, &handle, persist, restore_token, embedded_cursor, stop, approval_timeout).await;
    if result.is_err() {
        // Also takes down anything still on screen for this session.
        close_object(connection, handle.as_str(), SESSION_IFACE).await;
    }
    result
}

async fn configure_and_start(
    connection: &Connection,
    session: &OwnedObjectPath,
    persist: bool,
    restore_token: Option<&str>,
    embedded_cursor: bool,
    stop: &watch::Receiver<bool>,
    approval_timeout: Duration,
) -> Result<Negotiated, PortalError> {
    let token = new_token("devices");
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("types", Value::from(DEVICE_KEYBOARD | DEVICE_POINTER));
    if persist {
        options.insert("persist_mode", Value::from(PERSIST_UNTIL_REVOKED));
        if let Some(restore_token) = restore_token {
            options.insert("restore_token", Value::from(restore_token));
        }
    }
    let _: HashMap<String, OwnedValue> =
        request(connection, REMOTE_DESKTOP_IFACE, "SelectDevices", &token, &(session, options), SETUP_TIMEOUT, stop).await?;

    let token = new_token("sources");
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("types", Value::from(SOURCE_MONITOR));
    options.insert("multiple", Value::from(false));
    if embedded_cursor {
        options.insert("cursor_mode", Value::from(CURSOR_MODE_EMBEDDED));
    }
    let _: HashMap<String, OwnedValue> =
        request(connection, SCREEN_CAST_IFACE, "SelectSources", &token, &(session, options), SETUP_TIMEOUT, stop).await?;

    // The step that shows the person here the system's own dialog (unless
    // `restore_token` restored an earlier approval).
    let token = new_token("start");
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    let started: StartResults =
        request(connection, REMOTE_DESKTOP_IFACE, "Start", &token, &(session, "", options), approval_timeout, stop).await?;

    let devices = started.devices.unwrap_or(0);
    if devices & (DEVICE_KEYBOARD | DEVICE_POINTER) != DEVICE_KEYBOARD | DEVICE_POINTER {
        tracing::warn!("portal granted input devices {devices:#x} rather than keyboard + pointer; some input won't work");
    }
    let StartedStream(node_id, properties) = started
        .streams
        .unwrap_or_default()
        .into_iter()
        .next()
        .ok_or_else(|| PortalError::new(ErrorKind::Failed, "Start", "the desktop approved the session but didn't share a screen"))?;

    let options: HashMap<&str, Value> = HashMap::new();
    let reply = connection
        .call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(SCREEN_CAST_IFACE), "OpenPipeWireRemote", &(session, options))
        .await
        .map_err(|e| failed("OpenPipeWireRemote", e))?;
    let pipewire_fd: zvariant::OwnedFd = reply.body().deserialize().map_err(|e| failed("OpenPipeWireRemote", e))?;

    tracing::debug!("portal session {session} started: node {node_id}, logical size {:?}", properties.size);
    Ok(Negotiated {
        session: PortalSession { connection: connection.clone(), handle: session.clone(), node_id, logical_size: properties.size },
        pipewire_fd: pipewire_fd.into(),
        restore_token: started.restore_token,
        persistence_requested: persist,
    })
}

/// Calls a portal method that answers through a `Request` object, and
/// waits for that object's `Response` — see the module docs for why the
/// listener has to exist before the call.
async fn request<B, R>(
    connection: &Connection,
    interface: &str,
    method: &'static str,
    handle_token: &str,
    body: &B,
    timeout: Duration,
    stop: &watch::Receiver<bool>,
) -> Result<R, PortalError>
where
    B: serde::Serialize + zvariant::DynamicType,
    R: for<'de> serde::Deserialize<'de> + zvariant::Type,
{
    let expected = format!("{PORTAL_PATH}/request/{}/{handle_token}", sender_path_element(connection, method)?);
    let mut responses = response_stream(connection, &expected).await.map_err(|e| failed(method, e))?;

    let reply = connection
        .call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(interface), method, body)
        .await
        .map_err(|e| failed(method, e))?;
    let request_path: OwnedObjectPath = reply.body().deserialize().map_err(|e| failed(method, e))?;
    if request_path.as_str() != expected {
        // Portals have used the caller's handle_token since
        // xdg-desktop-portal 0.9 (2018); an older one picks its own path.
        // Listen there instead — a response it already sent before this
        // subscription is lost, but the timeout below still bounds that.
        tracing::warn!("portal answered {method} at {request_path} instead of {expected}");
        responses = response_stream(connection, request_path.as_str()).await.map_err(|e| failed(method, e))?;
    }

    let mut stop = stop.clone();
    let response = tokio::select! {
        response = responses.next() => response,
        _ = tokio::time::sleep(timeout) => {
            // Takes the dialog (if this step showed one) back off the
            // screen — otherwise it would sit there asking about a session
            // nobody is waiting for any more.
            close_object(connection, request_path.as_str(), REQUEST_IFACE).await;
            let reason = if method == "Start" {
                format!("nobody approved screen sharing on this computer within {} seconds", timeout.as_secs())
            } else {
                format!("the desktop's screen-sharing service stopped responding (at {method})")
            };
            return Err(PortalError::new(ErrorKind::TimedOut, method, reason));
        }
        _ = stopped(&mut stop) => {
            close_object(connection, request_path.as_str(), REQUEST_IFACE).await;
            return Err(PortalError::new(ErrorKind::Cancelled, method, "the session was ended before it started"));
        }
    };
    let message = match response {
        Some(Ok(message)) => message,
        Some(Err(e)) => return Err(failed(method, e)),
        None => return Err(failed(method, "the bus connection closed")),
    };

    // The response code first, on its own: `R` only describes a successful
    // response's results, which a refusal needn't match.
    let body = message.body();
    let (code, _): (u32, HashMap<String, OwnedValue>) = body.deserialize().map_err(|e| failed(method, e))?;
    match code {
        0 => body.deserialize::<(u32, R)>().map(|(_, results)| results).map_err(|e| failed(method, e)),
        1 => Err(PortalError::new(ErrorKind::Declined, method, "screen sharing was declined on this computer")),
        _ => Err(PortalError::new(ErrorKind::Failed, method, format!("the desktop's screen-sharing service refused the request (at {method})"))),
    }
}

async fn response_stream(connection: &Connection, path: &str) -> zbus::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(REQUEST_IFACE)?
        .member("Response")?
        .path(path)?
        .build();
    MessageStream::for_match_rule(rule, connection, Some(1)).await
}

/// Resolves once `stop` turns true. Never resolves if nothing can turn it
/// true any more.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// `CreateSession`'s `session_handle` result. xdg-desktop-portal sends it
/// typed `s` for historical reasons even though it's an object path;
/// accept either, as `ashpd` does, in case that's ever corrected.
fn session_handle(results: &HashMap<String, OwnedValue>) -> Option<OwnedObjectPath> {
    match &**results.get("session_handle")? {
        Value::Str(path) => OwnedObjectPath::try_from(path.as_str()).ok(),
        Value::ObjectPath(path) => Some(path.clone().into()),
        _ => None,
    }
}

fn sender_path_element(connection: &Connection, step: &'static str) -> Result<String, PortalError> {
    let name = connection.unique_name().ok_or_else(|| failed(step, "this bus connection has no unique name"))?;
    Ok(name.trim_start_matches(':').replace('.', "_"))
}

/// A token for a request or session path element (`[A-Za-z0-9_]` only),
/// unique within this process.
fn new_token(step: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("continuity_{step}_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

async fn property_u32(connection: &Connection, interface: &str, name: &str) -> zbus::Result<u32> {
    let reply = connection.call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(PROPERTIES_IFACE), "Get", &(interface, name)).await?;
    let value: OwnedValue = reply.body().deserialize()?;
    Ok(u32::try_from(value)?)
}

async fn close_object(connection: &Connection, path: &str, interface: &str) {
    if let Err(e) = connection.call_method(Some(PORTAL_DEST), path, Some(interface), "Close", &()).await {
        tracing::debug!("couldn't close {interface} at {path}: {e}");
    }
}

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "dict")]
struct StartResults {
    devices: Option<u32>,
    streams: Option<Vec<StartedStream>>,
    restore_token: Option<String>,
}

/// One entry of `Start`'s `streams` result: `(node_id, properties)`.
#[derive(serde::Deserialize, zvariant::Type, Debug)]
struct StartedStream(u32, StreamProperties);

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "dict")]
struct StreamProperties {
    size: Option<(i32, i32)>,
}

impl PortalSession {
    /// `keycode` is a Linux evdev code (`KEY_*` from input-event-codes.h).
    pub async fn notify_keyboard_keycode(&self, keycode: i32, pressed: bool) -> zbus::Result<()> {
        self.notify("NotifyKeyboardKeycode", &(&self.handle, no_options(), keycode, u32::from(pressed))).await
    }

    /// `(x, y)` in the stream's own coordinate space — see
    /// `LinuxRemoteControlHost`'s input forwarding for which size that is.
    pub async fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> zbus::Result<()> {
        self.notify("NotifyPointerMotionAbsolute", &(&self.handle, no_options(), self.node_id, x, y)).await
    }

    /// `button` is a Linux evdev code (`BTN_LEFT` etc.).
    pub async fn notify_pointer_button(&self, button: i32, pressed: bool) -> zbus::Result<()> {
        self.notify("NotifyPointerButton", &(&self.handle, no_options(), button, u32::from(pressed))).await
    }

    /// Smooth-scroll deltas, positive meaning down/right (libinput's
    /// convention).
    pub async fn notify_pointer_axis(&self, dx: f64, dy: f64) -> zbus::Result<()> {
        let mut options = no_options();
        // Each event is a whole scroll — no kinetic continuation after it.
        options.insert("finish", Value::from(true));
        self.notify("NotifyPointerAxis", &(&self.handle, options, dx, dy)).await
    }

    async fn notify<B>(&self, method: &str, body: &B) -> zbus::Result<()>
    where
        B: serde::Serialize + zvariant::DynamicType,
    {
        self.connection.call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(REMOTE_DESKTOP_IFACE), method, body).await.map(drop)
    }

    /// Ends the session — also what clears the desktop's own "screen is
    /// being shared" indicator.
    pub async fn close(&self) {
        close_object(&self.connection, self.handle.as_str(), SESSION_IFACE).await;
    }

    /// Resolves when the portal closes the session itself — which is what
    /// happens when sharing is stopped from this computer's own controls.
    /// Never resolves if that can't be watched.
    pub async fn closed(&self) {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(SESSION_IFACE)
            .and_then(|rule| rule.member("Closed"))
            .and_then(|rule| rule.path(self.handle.as_str()))
            .map(|rule| rule.build());
        let stream = match rule {
            Ok(rule) => MessageStream::for_match_rule(rule, &self.connection, Some(1)).await,
            Err(e) => Err(e),
        };
        match stream {
            Ok(mut stream) => {
                stream.next().await;
            }
            Err(e) => {
                tracing::debug!("can't watch for the portal closing session {}: {e}", self.handle);
                std::future::pending::<()>().await;
            }
        }
    }
}

fn no_options() -> HashMap<&'static str, Value<'static>> {
    HashMap::new()
}

#[cfg(test)]
#[path = "portal_tests.rs"]
mod tests;

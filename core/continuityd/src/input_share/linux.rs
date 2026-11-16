//! Keyboard-and-mouse sharing's Linux backend: the InputCapture portal,
//! which is how a Wayland app may capture input at all (GNOME 45 and later,
//! KDE Plasma 6.1 and later). Nothing here sees the cursor: Continuity asks
//! the compositor for pointer barriers along the placed edges of the screen
//! zones, and when the cursor crosses one the compositor hides it, says so
//! (`Activated`) and hands over the captured mouse and keyboard as a libei
//! stream (`ConnectToEIS`, read with reis). `Release` gives input back, with
//! where the cursor should reappear.
//!
//! Setup: an input-capture session — `CreateSession2` and `Start` on a v2
//! portal, with a restore token kept next to the trust store so the
//! desktop's "allow input capture?" dialog is answered once; the deprecated
//! `CreateSession` on a v1 one — then the EIS connection, the zones
//! (`GetZones`), the barriers (`SetPointerBarriers`) and `Enable`. Zones
//! changing (a monitor plugged in) and the session being disabled set the
//! barriers up again. Everything runs on a thread of its own with its own
//! runtime, like the remote-desktop host's portal session.
//!
//! **Compile-checked only** on CI: there's no Wayland session where this is
//! written. The portal calls follow the interface XML; libei through reis
//! 0.7's receiver API, as its own `receive` example drives it.

use super::{Bounds, LocalInput, Point, Shared, WHEEL_NOTCH};
use crate::remote_control_linux::portal::{
    self, close_object, new_token, property_u32, request, session_handle, ErrorKind, PortalError, PERSIST_UNTIL_REVOKED, PORTAL_DEST,
    PORTAL_PATH, SESSION_IFACE, SETUP_TIMEOUT,
};
use crate::remote_viewer::key_from_native_code;
use continuity_proto::{MouseButton as ProtoMouseButton, Platform, ScreenEdge};
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use zbus::zvariant::{self, OwnedObjectPath, OwnedValue, Structure, Value};
use zbus::{Connection, MatchRule, MessageStream};

const INPUT_CAPTURE_IFACE: &str = "org.freedesktop.portal.InputCapture";

// Capability bits as the portal numbers them.
const CAPABILITY_KEYBOARD: u32 = 1;
const CAPABILITY_POINTER: u32 = 2;

// Evdev button codes (linux/input-event-codes.h), as libei reports them.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// What the core asks of the portal task.
enum Command {
    /// The edges to put barriers on (placed, with a session up).
    Edges(Vec<ScreenEdge>),
    /// Give input back, the cursor reappearing at this point.
    Release(Point),
}

pub(crate) struct Backend {
    restore_token_path: Option<PathBuf>,
    /// The running portal task's commands, once started.
    commands: Mutex<Option<mpsc::UnboundedSender<Command>>>,
    /// The latest edges, for a task started after they were set.
    edges: Arc<Mutex<Vec<ScreenEdge>>>,
    /// The union of the zones — what the core measures edges against.
    screen: Arc<Mutex<Option<Bounds>>>,
}

impl Backend {
    pub(crate) fn new(profile: &str) -> Self {
        let restore_token_path = directories::ProjectDirs::from("app", "continuity", "continuity")
            .map(|dirs| dirs.config_dir().join(format!("input-capture-restore-token.{profile}")));
        Self { restore_token_path, commands: Mutex::new(None), edges: Arc::default(), screen: Arc::default() }
    }

    /// Starts the portal task on a thread of its own. Setup is asynchronous
    /// and can wait on a person, so this only fails if the thread can't be
    /// made; the task reports anything later through `backend_failed`.
    pub(crate) fn start(&self, shared: &Arc<Shared>) -> Result<(), String> {
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (shared, edges, screen, token_path) = (shared.clone(), self.edges.clone(), self.screen.clone(), self.restore_token_path.clone());
        std::thread::Builder::new()
            .name("input-capture".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(e) => return shared.backend_failed(&format!("couldn't start: {e}")),
                };
                if let Err(why) = runtime.block_on(run(&shared, commands_rx, edges, screen, token_path)) {
                    shared.backend_failed(&why);
                }
            })
            .map_err(|e| format!("couldn't start its thread: {e}"))?;
        *self.commands.lock().unwrap() = Some(commands_tx);
        Ok(())
    }

    pub(crate) fn placements_changed(&self, edges: &[ScreenEdge]) {
        *self.edges.lock().unwrap() = edges.to_vec();
        self.send(Command::Edges(edges.to_vec()));
    }

    pub(crate) fn screen(&self) -> Option<Bounds> {
        *self.screen.lock().unwrap()
    }

    /// Nothing to do: the compositor hides the cursor while it captures.
    pub(crate) fn grabbed(&self, _left_at: Point) {}

    pub(crate) fn released(&self, at: Point) {
        self.send(Command::Release(at));
    }

    fn send(&self, command: Command) {
        if let Some(commands) = self.commands.lock().unwrap().as_ref() {
            let _ = commands.send(command);
        }
    }
}

/// Words a portal failure for the person here — `portal::request`'s own
/// reasons are about screen sharing, which this isn't.
fn reason(error: PortalError) -> String {
    match error.kind {
        ErrorKind::Declined => "input capture was declined on this computer".to_string(),
        ErrorKind::TimedOut if error.step == "Start" => "nobody allowed input capture on this computer in time".to_string(),
        _ => format!("the desktop's input-capture portal failed at {}: {}", error.step, error.reason),
    }
}

async fn run(
    shared: &Shared,
    mut commands: mpsc::UnboundedReceiver<Command>,
    edges: Arc<Mutex<Vec<ScreenEdge>>>,
    screen: Arc<Mutex<Option<Bounds>>>,
    token_path: Option<PathBuf>,
) -> Result<(), String> {
    let connection = Connection::session().await.map_err(|e| format!("couldn't reach this desktop's session bus: {e}"))?;
    let version = property_u32(&connection, INPUT_CAPTURE_IFACE, "version").await.map_err(|e| {
        tracing::debug!("no InputCapture portal: {e}");
        "this desktop can't share its keyboard and mouse (it needs the input-capture portal: GNOME 45 or KDE Plasma 6.1 or newer, on Wayland)"
            .to_string()
    })?;
    // Never set: the session is only ever ended by the process going away.
    let (_stop_tx, stop) = watch::channel(false);
    let session = if version >= 2 {
        start_session(&connection, token_path.as_ref(), &stop).await?
    } else {
        create_session_v1(&connection, &stop).await?
    };
    let result = capture(shared, &connection, &session, &mut commands, edges, screen, &stop).await;
    close_object(&connection, session.as_str(), SESSION_IFACE).await;
    result
}

/// v2: `CreateSession2`, then `Start`, which shows the desktop's own dialog
/// unless a stored approval is restored.
async fn start_session(connection: &Connection, token_path: Option<&PathBuf>, stop: &watch::Receiver<bool>) -> Result<OwnedObjectPath, String> {
    let session_token = new_token("capture_session");
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("session_handle_token", Value::from(session_token.as_str()));
    let reply = connection
        .call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(INPUT_CAPTURE_IFACE), "CreateSession2", &(options,))
        .await
        .map_err(|e| format!("the desktop's input-capture portal refused a session: {e}"))?;
    let results: HashMap<String, OwnedValue> = reply.body().deserialize().map_err(|e| format!("CreateSession2 answered oddly: {e}"))?;
    let session = session_handle(&results).ok_or("CreateSession2 gave no session")?;

    let restore_token = token_path.and_then(|path| std::fs::read_to_string(path).ok()).map(|token| token.trim().to_string());
    let token = new_token("capture_start");
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("capabilities", Value::from(CAPABILITY_KEYBOARD | CAPABILITY_POINTER));
    options.insert("persist_mode", Value::from(PERSIST_UNTIL_REVOKED));
    if let Some(restore_token) = restore_token.as_deref().filter(|token| !token.is_empty()) {
        options.insert("restore_token", Value::from(restore_token));
    }
    let started: StartResults = request(connection, INPUT_CAPTURE_IFACE, "Start", &token, &(&session, "", options), portal::APPROVAL_TIMEOUT, stop)
        .await
        .map_err(reason)?;
    if started.capabilities.unwrap_or(0) & CAPABILITY_POINTER == 0 {
        tracing::warn!("input capture was started without the pointer; crossing an edge won't work");
    }
    // A restore token is single use: keep the new one, or forget the old.
    if let Some(path) = token_path {
        match started.restore_token {
            Some(token) => {
                if let Err(e) = crate::share::write_private_file(path, token.as_bytes()) {
                    tracing::debug!("couldn't keep the input-capture restore token: {e}");
                }
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    Ok(session)
}

/// v1: the deprecated `CreateSession`, which starts the session at once.
async fn create_session_v1(connection: &Connection, stop: &watch::Receiver<bool>) -> Result<OwnedObjectPath, String> {
    let session_token = new_token("capture_session");
    let token = new_token("capture_create");
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("session_handle_token", Value::from(session_token.as_str()));
    options.insert("capabilities", Value::from(CAPABILITY_KEYBOARD | CAPABILITY_POINTER));
    let results: HashMap<String, OwnedValue> =
        request(connection, INPUT_CAPTURE_IFACE, "CreateSession", &token, &("", options), portal::APPROVAL_TIMEOUT, stop).await.map_err(reason)?;
    session_handle(&results).ok_or_else(|| "CreateSession gave no session".to_string())
}

/// The session's life: the EIS connection, the barriers, then the
/// portal's signals, the core's commands and the captured input, until one
/// of them ends it.
async fn capture(
    shared: &Shared,
    connection: &Connection,
    session: &OwnedObjectPath,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    edges: Arc<Mutex<Vec<ScreenEdge>>>,
    screen: Arc<Mutex<Option<Bounds>>>,
    stop: &watch::Receiver<bool>,
) -> Result<(), String> {
    // Subscribed before anything can be activated.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(INPUT_CAPTURE_IFACE)
        .and_then(|rule| rule.path(PORTAL_PATH))
        .map(|rule| rule.build())
        .map_err(|e| format!("couldn't watch the input-capture portal: {e}"))?;
    let mut signals =
        MessageStream::for_match_rule(rule, connection, Some(64)).await.map_err(|e| format!("couldn't watch the input-capture portal: {e}"))?;

    let options: HashMap<&str, Value> = HashMap::new();
    let reply = connection
        .call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(INPUT_CAPTURE_IFACE), "ConnectToEIS", &(session, options))
        .await
        .map_err(|e| format!("couldn't connect to the desktop's input stream: {e}"))?;
    let fd: zvariant::OwnedFd = reply.body().deserialize().map_err(|e| format!("ConnectToEIS answered oddly: {e}"))?;
    let context = ei::Context::new(UnixStream::from(std::os::fd::OwnedFd::from(fd))).map_err(|e| format!("couldn't open the input stream: {e}"))?;
    let (_ei_connection, mut events) = context
        .handshake_tokio("continuity", ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| format!("the input stream's handshake failed: {e}"))?;

    let mut current_edges = edges.lock().unwrap().clone();
    let mut barriers = set_up_barriers(connection, session, &current_edges, &screen, stop).await?;
    let mut activation: Option<u32> = None;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => return Ok(()),
                Some(Command::Edges(new)) => {
                    current_edges = new;
                    barriers = set_up_barriers(connection, session, &current_edges, &screen, stop).await?;
                }
                Some(Command::Release(at)) => {
                    if let Some(id) = activation.take() {
                        release(connection, session, id, at).await;
                    }
                }
            },
            message = signals.next() => {
                let Some(Ok(message)) = message else { return Err("the input-capture portal went away".to_string()) };
                let member = message.header().member().map(|member| member.as_str().to_string()).unwrap_or_default();
                let body = message.body();
                let Ok((path, _)) = body.deserialize::<(OwnedObjectPath, HashMap<String, OwnedValue>)>() else { continue };
                if path != *session {
                    continue;
                }
                match member.as_str() {
                    "Activated" => {
                        let Ok((_, activated)) = body.deserialize::<(OwnedObjectPath, Activation)>() else { continue };
                        let Some(id) = activated.activation_id else { continue };
                        activation = Some(id);
                        let bounds = *screen.lock().unwrap();
                        let (cx, cy) = activated.cursor_position.unwrap_or_default();
                        let at = Point { x: cx, y: cy };
                        let edge = activated.barrier_id.and_then(|barrier| barriers.get(&barrier).copied());
                        let taken = match (edge, bounds) {
                            (Some(edge), Some(bounds)) => shared.crossed(edge, position_along(bounds, edge, at), at),
                            _ => false,
                        };
                        if !taken {
                            // Nothing (any more) to take the mouse to: back at once.
                            let back = match (edge, bounds) {
                                (Some(edge), Some(bounds)) => super::point_on_edge(bounds, edge, position_along(bounds, edge, at)),
                                _ => at,
                            };
                            activation = None;
                            release(connection, session, id, back).await;
                        }
                    }
                    "Deactivated" => {
                        activation = None;
                        shared.lost();
                    }
                    "Disabled" => {
                        activation = None;
                        shared.lost();
                        enable(connection, session).await?;
                    }
                    "ZonesChanged" => {
                        barriers = set_up_barriers(connection, session, &current_edges, &screen, stop).await?;
                    }
                    _ => {}
                }
            }
            event = events.next() => match event {
                None => return Err("the desktop closed the input stream".to_string()),
                Some(Err(e)) => return Err(format!("the input stream failed: {e}")),
                Some(Ok(event)) => handle_event(shared, &context, event),
            },
        }
    }
}

/// Reads the zones, puts barriers on the placed edges and enables capture.
/// Returns which edge each barrier id stands for.
async fn set_up_barriers(
    connection: &Connection,
    session: &OwnedObjectPath,
    edges: &[ScreenEdge],
    screen: &Mutex<Option<Bounds>>,
    stop: &watch::Receiver<bool>,
) -> Result<HashMap<u32, ScreenEdge>, String> {
    let token = new_token("capture_zones");
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    let zones: ZonesResults = request(connection, INPUT_CAPTURE_IFACE, "GetZones", &token, &(session, options), SETUP_TIMEOUT, stop).await.map_err(reason)?;
    let list = zones.zones.unwrap_or_default();
    *screen.lock().unwrap() = union(&list);

    let barriers = barriers_for(&list, edges);
    let dicts: Vec<HashMap<&str, Value>> = barriers
        .iter()
        .map(|&(id, _, position)| {
            let mut barrier: HashMap<&str, Value> = HashMap::new();
            barrier.insert("barrier_id", Value::from(id));
            barrier.insert("position", Value::from(Structure::from(position)));
            barrier
        })
        .collect();
    let token = new_token("capture_barriers");
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    let set: BarrierResults = request(
        connection,
        INPUT_CAPTURE_IFACE,
        "SetPointerBarriers",
        &token,
        &(session, options, dicts, zones.zone_set.unwrap_or(0)),
        SETUP_TIMEOUT,
        stop,
    )
    .await
    .map_err(reason)?;
    let failed = set.failed_barriers.unwrap_or_default();
    if !failed.is_empty() {
        tracing::warn!("the desktop refused input-capture barriers {failed:?}; those edges won't cross");
    }
    enable(connection, session).await?;
    tracing::debug!("input capture armed: {} barrier(s) on {edges:?}", barriers.len().saturating_sub(failed.len()));
    Ok(barriers.into_iter().filter(|(id, _, _)| !failed.contains(id)).map(|(id, edge, _)| (id, edge)).collect())
}

async fn enable(connection: &Connection, session: &OwnedObjectPath) -> Result<(), String> {
    let options: HashMap<&str, Value> = HashMap::new();
    connection
        .call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(INPUT_CAPTURE_IFACE), "Enable", &(session, options))
        .await
        .map(drop)
        .map_err(|e| format!("couldn't enable input capture: {e}"))
}

async fn release(connection: &Connection, session: &OwnedObjectPath, activation_id: u32, at: Point) {
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("activation_id", Value::from(activation_id));
    options.insert("cursor_position", Value::from(Structure::from((at.x, at.y))));
    if let Err(e) = connection.call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(INPUT_CAPTURE_IFACE), "Release", &(session, options)).await {
        tracing::debug!("couldn't release input capture: {e}");
    }
}

/// One libei event while capturing.
fn handle_event(shared: &Shared, context: &ei::Context, event: EiEvent) {
    let input = match event {
        EiEvent::SeatAdded(added) => {
            added.seat.bind_capabilities(DeviceCapability::Pointer | DeviceCapability::Keyboard | DeviceCapability::Button | DeviceCapability::Scroll);
            let _ = context.flush();
            return;
        }
        EiEvent::PointerMotion(motion) => LocalInput::Moved { at: Point { x: 0.0, y: 0.0 }, delta: (f64::from(motion.dx), f64::from(motion.dy)) },
        EiEvent::Button(button) => {
            let button_kind = match button.button {
                BTN_LEFT => ProtoMouseButton::Left,
                BTN_RIGHT => ProtoMouseButton::Right,
                BTN_MIDDLE => ProtoMouseButton::Middle,
                _ => return,
            };
            LocalInput::Button { button: button_kind, down: button.state == ei::button::ButtonState::Press }
        }
        // libei's convention is libinput's, positive scrolling down and
        // right; the viewer's is the other way round.
        EiEvent::ScrollDelta(scroll) => LocalInput::Scroll { dx: -f64::from(scroll.dx), dy: -f64::from(scroll.dy) },
        EiEvent::ScrollDiscrete(scroll) => LocalInput::Scroll {
            dx: -f64::from(scroll.discrete_dx) / 120.0 * WHEEL_NOTCH,
            dy: -f64::from(scroll.discrete_dy) / 120.0 * WHEEL_NOTCH,
        },
        EiEvent::KeyboardKey(key) => LocalInput::Key {
            key: u16::try_from(key.key).ok().and_then(|code| key_from_native_code(code, Platform::Linux)),
            down: key.state == ei::keyboard::KeyState::Press,
        },
        _ => return,
    };
    // Only while the mouse is over there — never a crossing check: these
    // events carry no position (the compositor decides crossings).
    shared.forward(input);
}

/// How far along `edge` of `bounds` the cursor at `at` is.
fn position_along(bounds: Bounds, edge: ScreenEdge, at: Point) -> f64 {
    match edge {
        ScreenEdge::Left | ScreenEdge::Right => ((at.y - bounds.y) / bounds.height.max(1.0)).clamp(0.0, 1.0),
        ScreenEdge::Top | ScreenEdge::Bottom => ((at.x - bounds.x) / bounds.width.max(1.0)).clamp(0.0, 1.0),
    }
}

/// The bounding box of the zones (`(width, height, x, y)` each).
fn union(zones: &[(u32, u32, i32, i32)]) -> Option<Bounds> {
    let left = zones.iter().map(|z| z.2).min()?;
    let top = zones.iter().map(|z| z.3).min()?;
    let right = zones.iter().map(|z| z.2 + z.0 as i32).max()?;
    let bottom = zones.iter().map(|z| z.3 + z.1 as i32).max()?;
    Some(Bounds { x: f64::from(left), y: f64::from(top), width: f64::from(right - left), height: f64::from(bottom - top) })
}

/// A barrier for every placed edge of every zone on the outside of the
/// zones' bounding box, as `(id, edge, (x1, y1, x2, y2))`. Positions follow
/// the portal's convention: a barrier sits on the top or left edge of its
/// pixels and its ends are inclusive, so a right-edge barrier is at `x +
/// width` and runs from `y` to `y + height - 1`.
fn barriers_for(zones: &[(u32, u32, i32, i32)], edges: &[ScreenEdge]) -> Vec<(u32, ScreenEdge, (i32, i32, i32, i32))> {
    let Some(outer) = union(zones) else { return Vec::new() };
    let (left, top) = (outer.x as i32, outer.y as i32);
    let (right, bottom) = (left + outer.width as i32, top + outer.height as i32);
    let mut barriers = Vec::new();
    for &(width, height, x, y) in zones {
        let (w, h) = (width as i32, height as i32);
        for &edge in edges {
            let position = match edge {
                ScreenEdge::Left if x == left => (x, y, x, y + h - 1),
                ScreenEdge::Right if x + w == right => (x + w, y, x + w, y + h - 1),
                ScreenEdge::Top if y == top => (x, y, x + w - 1, y),
                ScreenEdge::Bottom if y + h == bottom => (x, y + h, x + w - 1, y + h),
                _ => continue,
            };
            barriers.push((barriers.len() as u32 + 1, edge, position));
        }
    }
    barriers
}

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "dict")]
struct StartResults {
    capabilities: Option<u32>,
    restore_token: Option<String>,
}

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "dict")]
struct ZonesResults {
    zones: Option<Vec<(u32, u32, i32, i32)>>,
    zone_set: Option<u32>,
}

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "dict")]
struct BarrierResults {
    failed_barriers: Option<Vec<u32>>,
}

#[derive(zvariant::DeserializeDict, zvariant::Type, Debug, Default)]
#[zvariant(signature = "dict")]
struct Activation {
    activation_id: Option<u32>,
    cursor_position: Option<(f64, f64)>,
    barrier_id: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barriers_go_on_the_outside_of_the_zones() {
        // Two 1920x1080 monitors side by side, as in the portal's docs.
        let zones = [(1920, 1080, 0, 0), (1920, 1080, 1920, 0)];
        let right = barriers_for(&zones, &[ScreenEdge::Right]);
        assert_eq!(right, vec![(1, ScreenEdge::Right, (3840, 0, 3840, 1079))], "only the right screen's right edge");
        let left = barriers_for(&zones, &[ScreenEdge::Left]);
        assert_eq!(left, vec![(1, ScreenEdge::Left, (0, 0, 0, 1079))]);
        let top = barriers_for(&zones, &[ScreenEdge::Top]);
        assert_eq!(top, vec![(1, ScreenEdge::Top, (0, 0, 1919, 0)), (2, ScreenEdge::Top, (1920, 0, 3839, 0))], "both screens' tops");
        let bottom = barriers_for(&zones, &[ScreenEdge::Bottom]);
        assert_eq!(bottom[0].2, (0, 1080, 1919, 1080));
        assert!(barriers_for(&[], &[ScreenEdge::Right]).is_empty());
        assert!(barriers_for(&zones, &[]).is_empty());
    }

    #[test]
    fn the_zones_union_is_their_bounding_box() {
        assert_eq!(union(&[(1920, 1080, 0, 0), (1280, 1024, 1920, -200)]), Some(Bounds { x: 0.0, y: -200.0, width: 3200.0, height: 1280.0 }));
        assert_eq!(union(&[]), None);
    }

    #[test]
    fn the_position_along_an_edge_is_measured_on_the_whole_screen() {
        let bounds = Bounds { x: 0.0, y: 0.0, width: 3840.0, height: 1080.0 };
        assert_eq!(position_along(bounds, ScreenEdge::Right, Point { x: 3870.0, y: 540.0 }), 0.5, "past the edge, as Activated reports it");
        assert_eq!(position_along(bounds, ScreenEdge::Top, Point { x: 960.0, y: -12.0 }), 0.25);
        assert_eq!(position_along(bounds, ScreenEdge::Left, Point { x: -5.0, y: 2000.0 }), 1.0, "clamped");
    }
}

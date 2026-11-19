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
//! `CreateSession` on a v1 one — then the EIS connection, and the barriers
//! armed (`arm`). Everything runs on a thread of its own with its own
//! runtime, like the remote-desktop host's portal session.
//!
//! **Arming the barriers** is `Disable`, `GetZones`, `SetPointerBarriers`,
//! `Enable` — and only when what's armed has to change: the edges with a
//! session up changed, the zones changed (a monitor plugged in), or the
//! desktop disabled the session. Never while the mouse is over there: a
//! change then waits until it's back. **Real bug fixed here**: the barriers
//! used to be set again on an enabled session, at every session coming up
//! and at every crossing. GNOME only takes barriers on a session that isn't
//! enabled: it dropped the armed ones, refused the new ones, and refused
//! the `Enable` after them — which stopped sharing altogether. The very
//! first crossing also suspended the capture it had just started.
//!
//! A session the desktop closes, or whose input stream goes, is replaced
//! by a new one (the restore token keeps the dialog away), with a growing
//! pause when they keep ending straight away.
//!
//! **Compile-checked only** on CI: there's no Wayland session where this is
//! written. The portal calls follow the interface XML and GNOME's
//! implementation (xdg-desktop-portal-gnome, mutter); libei through reis
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
use std::time::{Duration, Instant};
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

/// A session that ends sooner than this after starting counts towards
/// giving up (`MAX_QUICK_ENDS`).
const QUICK_END: Duration = Duration::from_secs(10);
/// Sessions ending straight away this many times in a row: the backend
/// gives up, says why, and is started again at the next placement change
/// or connection.
const MAX_QUICK_ENDS: u32 = 5;

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
    /// The latest edges, for a session started after they were set.
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

    /// Passes the edges on only when they changed: the core reports them at
    /// every crossing and every session coming and going, and arming the
    /// barriers again isn't free (see the module docs).
    pub(crate) fn placements_changed(&self, edges: &[ScreenEdge]) {
        {
            let mut current = self.edges.lock().unwrap();
            if *current == edges {
                return;
            }
            *current = edges.to_vec();
        }
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

/// Why one input-capture session ended.
enum Ended {
    /// The backend was dropped.
    Stopped,
    /// The session went, but a new one should do (the desktop closed it,
    /// its input stream went).
    Again(String),
    /// Something a new session wouldn't fix.
    Failed(String),
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
    tracing::info!("InputCapture portal v{version}");
    // Never set: a session is only ever ended by the process going away.
    let (_stop_tx, stop) = watch::channel(false);
    let mut quick_ends = 0;
    loop {
        let session = if version >= 2 {
            start_session(&connection, token_path.as_ref(), &stop).await?
        } else {
            create_session_v1(&connection, &stop).await?
        };
        let started = Instant::now();
        let ended = capture(shared, &connection, &session, &mut commands, &edges, &screen, &stop).await;
        close_object(&connection, session.as_str(), SESSION_IFACE).await;
        // The mouse can't be over there without a session.
        shared.lost();
        match ended {
            Ended::Stopped => return Ok(()),
            Ended::Failed(why) => return Err(why),
            Ended::Again(why) => {
                quick_ends = if started.elapsed() < QUICK_END { quick_ends + 1 } else { 0 };
                if quick_ends >= MAX_QUICK_ENDS {
                    return Err(format!("the desktop's input capture keeps stopping ({why})"));
                }
                let pause = Duration::from_secs(1 << quick_ends);
                tracing::info!("input-capture session ended ({why}); starting a new one in {}s", pause.as_secs());
                tokio::time::sleep(pause).await;
            }
        }
    }
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

/// A stream of the signals that matter to `session`: the portal's
/// InputCapture ones (all on the portal object, telling sessions apart by
/// their first argument), and the session itself being closed.
async fn watch_signals(connection: &Connection, session: &OwnedObjectPath) -> Result<futures_util::stream::SelectAll<MessageStream>, String> {
    let failed = |e: zbus::Error| format!("couldn't watch the input-capture portal: {e}");
    let capture = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(INPUT_CAPTURE_IFACE)
        .and_then(|rule| rule.path(PORTAL_PATH))
        .map(|rule| rule.build())
        .map_err(failed)?;
    let closed = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(SESSION_IFACE)
        .and_then(|rule| rule.member("Closed"))
        .and_then(|rule| rule.path(session.as_str()))
        .map(|rule| rule.build())
        .map_err(failed)?;
    let capture = MessageStream::for_match_rule(capture, connection, Some(64)).await.map_err(failed)?;
    let closed = MessageStream::for_match_rule(closed, connection, Some(1)).await.map_err(failed)?;
    Ok(futures_util::stream::select_all([capture, closed]))
}

/// The session's life: the EIS connection, the barriers, then the
/// portal's signals, the core's commands and the captured input, until one
/// of them ends it.
async fn capture(
    shared: &Shared,
    connection: &Connection,
    session: &OwnedObjectPath,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    edges: &Mutex<Vec<ScreenEdge>>,
    screen: &Mutex<Option<Bounds>>,
    stop: &watch::Receiver<bool>,
) -> Ended {
    // Subscribed before anything can be activated.
    let mut signals = match watch_signals(connection, session).await {
        Ok(signals) => signals,
        Err(why) => return Ended::Failed(why),
    };

    let options: HashMap<&str, Value> = HashMap::new();
    let reply = match connection.call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(INPUT_CAPTURE_IFACE), "ConnectToEIS", &(session, options)).await {
        Ok(reply) => reply,
        Err(e) => return Ended::Again(format!("couldn't connect to the desktop's input stream: {e}")),
    };
    let fd: zvariant::OwnedFd = match reply.body().deserialize() {
        Ok(fd) => fd,
        Err(e) => return Ended::Failed(format!("ConnectToEIS answered oddly: {e}")),
    };
    let context = match ei::Context::new(UnixStream::from(std::os::fd::OwnedFd::from(fd))) {
        Ok(context) => context,
        Err(e) => return Ended::Again(format!("couldn't open the input stream: {e}")),
    };
    let (_ei_connection, mut events) = match context.handshake_tokio("continuity", ei::handshake::ContextType::Receiver).await {
        Ok(handshake) => handshake,
        Err(e) => return Ended::Again(format!("the input stream's handshake failed: {e}")),
    };

    // What's armed now, and what to arm once the mouse is back here.
    let mut armed = edges.lock().unwrap().clone();
    let mut barriers = match arm(connection, session, &armed, screen, stop, false).await {
        Ok(barriers) => barriers,
        Err(why) => return Ended::Again(why),
    };
    let mut waiting: Option<Vec<ScreenEdge>> = None;
    let mut activation: Option<u32> = None;
    let mut scroll = FrameScroll::default();
    loop {
        // Arms what changed while the mouse was over there, now it's back.
        if activation.is_none() {
            if let Some(next) = waiting.take() {
                if next != armed {
                    armed = next;
                    barriers = match arm(connection, session, &armed, screen, stop, true).await {
                        Ok(barriers) => barriers,
                        Err(why) => return Ended::Again(why),
                    };
                }
            }
        }
        tokio::select! {
            command = commands.recv() => match command {
                None => return Ended::Stopped,
                Some(Command::Edges(next)) => waiting = Some(next),
                Some(Command::Release(at)) => {
                    if let Some(id) = activation.take() {
                        release(connection, session, id, at).await;
                    }
                }
            },
            message = signals.next() => {
                let Some(Ok(message)) = message else { return Ended::Again("the input-capture portal went away".to_string()) };
                let header = message.header();
                let member = header.member().map(|member| member.as_str().to_string()).unwrap_or_default();
                if header.interface().is_some_and(|interface| interface.as_str() == SESSION_IFACE) {
                    // Only this session's `Closed` is subscribed to.
                    return Ended::Again("the desktop closed the input-capture session".to_string());
                }
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
                        let at = activated.cursor_position.map(|(x, y)| Point { x, y });
                        let edge = activated_edge(activated.barrier_id, &barriers, at, bounds, &armed);
                        tracing::debug!("input capture activated ({id}) at {at:?}, barrier {:?} = {edge:?}", activated.barrier_id);
                        let taken = match (edge, bounds) {
                            (Some(edge), Some(bounds)) => {
                                let at = at.unwrap_or_else(|| super::point_on_edge(bounds, edge, 0.5));
                                shared.crossed(edge, position_along(bounds, edge, at), at)
                            }
                            _ => false,
                        };
                        if !taken {
                            // Nothing (any more) to take the mouse to, or the
                            // pointer was being driven from over there: back
                            // at once, a few pixels in.
                            let back = match (edge, bounds, at) {
                                (Some(edge), Some(bounds), Some(at)) => super::point_on_edge(bounds, edge, position_along(bounds, edge, at)),
                                (Some(edge), Some(bounds), None) => super::point_on_edge(bounds, edge, 0.5),
                                _ => at.unwrap_or(Point { x: 0.0, y: 0.0 }),
                            };
                            activation = None;
                            release(connection, session, id, back).await;
                        }
                    }
                    "Deactivated" => {
                        activation = None;
                        shared.lost();
                    }
                    // The desktop switched capture off (GNOME, for one,
                    // does when the monitors change, and says
                    // `ZonesChanged`): armed again from the start.
                    "Disabled" | "ZonesChanged" => {
                        tracing::debug!("input capture: {member}; arming the barriers again");
                        activation = None;
                        shared.lost();
                        barriers = match arm(connection, session, &armed, screen, stop, true).await {
                            Ok(barriers) => barriers,
                            Err(why) => return Ended::Again(why),
                        };
                    }
                    _ => {}
                }
            }
            event = events.next() => match event {
                None => return Ended::Again("the desktop closed the input stream".to_string()),
                Some(Err(e)) => return Ended::Again(format!("the input stream failed: {e}")),
                Some(Ok(EiEvent::Disconnected(_))) => return Ended::Again("the desktop disconnected the input stream".to_string()),
                Some(Ok(event)) => handle_event(shared, &context, &mut scroll, event),
            },
        }
    }
}

/// Arms the barriers: reads the zones, puts barriers on `edges` and enables
/// capture — `Disable` first when the session may be enabled already,
/// since barriers can only be set on one that isn't (GNOME refuses them
/// otherwise; the portal's own documentation says setting them suspends
/// it). Returns which edge each barrier id stands for.
async fn arm(
    connection: &Connection,
    session: &OwnedObjectPath,
    edges: &[ScreenEdge],
    screen: &Mutex<Option<Bounds>>,
    stop: &watch::Receiver<bool>,
    was_enabled: bool,
) -> Result<HashMap<u32, ScreenEdge>, String> {
    if was_enabled {
        // Not enabled any more (GNOME disables it itself when the zones
        // change) is fine.
        if let Err(e) = call(connection, "Disable", session).await {
            tracing::debug!("input capture: Disable: {e}");
        }
    }
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
        tracing::warn!("the desktop refused input-capture barriers {failed:?} of {barriers:?}; those edges won't cross");
    }
    call(connection, "Enable", session).await.map_err(|e| format!("couldn't enable input capture: {e}"))?;
    tracing::info!("input capture armed: {} barrier(s) on {edges:?}, zones {list:?}", barriers.len().saturating_sub(failed.len()));
    Ok(barriers.into_iter().filter(|(id, _, _)| !failed.contains(id)).map(|(id, edge, _)| (id, edge)).collect())
}

/// One of the session methods that take only options (`Enable`, `Disable`).
async fn call(connection: &Connection, method: &str, session: &OwnedObjectPath) -> zbus::Result<()> {
    let options: HashMap<&str, Value> = HashMap::new();
    connection.call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(INPUT_CAPTURE_IFACE), method, &(session, options)).await.map(drop)
}

async fn release(connection: &Connection, session: &OwnedObjectPath, activation_id: u32, at: Point) {
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("activation_id", Value::from(activation_id));
    options.insert("cursor_position", Value::from(Structure::from((at.x, at.y))));
    if let Err(e) = connection.call_method(Some(PORTAL_DEST), PORTAL_PATH, Some(INPUT_CAPTURE_IFACE), "Release", &(session, options)).await {
        tracing::debug!("couldn't release input capture: {e}");
    }
}

/// Which placed edge an activation is at: its barrier's, or — a desktop
/// that doesn't say which barrier (KDE Plasma), or names none — the armed
/// edge the cursor is pressed against.
fn activated_edge(barrier_id: Option<u32>, barriers: &HashMap<u32, ScreenEdge>, at: Option<Point>, bounds: Option<Bounds>, armed: &[ScreenEdge]) -> Option<ScreenEdge> {
    if let Some(edge) = barrier_id.filter(|id| *id != 0).and_then(|id| barriers.get(&id).copied()) {
        return Some(edge);
    }
    let (at, bounds) = (at?, bounds?);
    super::edge_of(at, bounds).map(|(edge, _)| edge).filter(|edge| armed.contains(edge))
}

/// Scrolling within one libei frame. A wheel can come as both a discrete
/// and a smooth scroll for the same turn; the discrete one wins, so a notch
/// scrolls the same as from any other computer and isn't counted twice.
#[derive(Default, Debug, PartialEq)]
struct FrameScroll {
    smooth: (f64, f64),
    discrete: (i32, i32),
}

impl FrameScroll {
    /// What the frame's scrolling forwards as, in the viewer's convention
    /// (pixels, positive up and left — libinput's is down and right).
    fn take(&mut self) -> Option<LocalInput> {
        let scroll = std::mem::take(self);
        if scroll.discrete != (0, 0) {
            let notches = |value: i32| -f64::from(value) / 120.0 * WHEEL_NOTCH;
            Some(LocalInput::Scroll { dx: notches(scroll.discrete.0), dy: notches(scroll.discrete.1) })
        } else if scroll.smooth != (0.0, 0.0) {
            Some(LocalInput::Scroll { dx: -scroll.smooth.0, dy: -scroll.smooth.1 })
        } else {
            None
        }
    }
}

/// One libei event while capturing.
fn handle_event(shared: &Shared, context: &ei::Context, scroll: &mut FrameScroll, event: EiEvent) {
    let input = match event {
        EiEvent::SeatAdded(added) => {
            added.seat.bind_capabilities(DeviceCapability::Pointer | DeviceCapability::Keyboard | DeviceCapability::Button | DeviceCapability::Scroll);
            let _ = context.flush();
            return;
        }
        EiEvent::DeviceAdded(_) | EiEvent::DeviceRemoved(_) | EiEvent::DeviceResumed(_) | EiEvent::DevicePaused(_) => {
            tracing::debug!("input capture: {event:?}");
            return;
        }
        EiEvent::ScrollDelta(delta) => {
            scroll.smooth.0 += f64::from(delta.dx);
            scroll.smooth.1 += f64::from(delta.dy);
            return;
        }
        EiEvent::ScrollDiscrete(discrete) => {
            scroll.discrete.0 += discrete.discrete_dx;
            scroll.discrete.1 += discrete.discrete_dy;
            return;
        }
        EiEvent::Frame(_) => match scroll.take() {
            Some(input) => input,
            None => return,
        },
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
/// width` and runs from `y` to `y + height - 1` (mutter checks exactly
/// that: `x == layout.x + layout.width`).
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

    #[test]
    fn an_activation_without_a_barrier_id_is_placed_by_the_cursor() {
        let bounds = Some(Bounds { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 });
        let barriers = HashMap::from([(1, ScreenEdge::Right)]);
        let past_right = Some(Point { x: 1921.0, y: 300.0 });
        assert_eq!(activated_edge(Some(1), &barriers, past_right, bounds, &[ScreenEdge::Right]), Some(ScreenEdge::Right));
        assert_eq!(activated_edge(None, &barriers, past_right, bounds, &[ScreenEdge::Right]), Some(ScreenEdge::Right), "KDE Plasma names no barrier");
        assert_eq!(activated_edge(Some(0), &barriers, past_right, bounds, &[ScreenEdge::Right]), Some(ScreenEdge::Right), "0: couldn't tell");
        assert_eq!(activated_edge(None, &barriers, Some(Point { x: -1.0, y: 300.0 }), bounds, &[ScreenEdge::Right]), None, "not an armed edge");
        assert_eq!(activated_edge(None, &barriers, None, bounds, &[ScreenEdge::Right]), None);
    }

    #[test]
    fn a_frames_wheel_turn_counts_once() {
        let mut scroll = FrameScroll { smooth: (0.0, 15.0), discrete: (0, 120) };
        assert_eq!(scroll.take(), Some(LocalInput::Scroll { dx: 0.0, dy: -WHEEL_NOTCH }), "the notch, not the notch and its smooth copy");
        assert_eq!(scroll, FrameScroll::default(), "taken");
        let mut scroll = FrameScroll { smooth: (-3.5, 2.0), discrete: (0, 0) };
        assert_eq!(scroll.take(), Some(LocalInput::Scroll { dx: 3.5, dy: -2.0 }), "a touchpad's smooth scroll");
        assert_eq!(FrameScroll::default().take(), None);
    }
}

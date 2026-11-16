//! Keyboard-and-mouse sharing, the controlling side: this computer's own
//! mouse and keyboard, forwarded to another computer while the cursor is
//! "over there" — one keyboard and mouse driving computers side by side,
//! with no picture of the other screen. The other computer injects what
//! it's sent exactly as it does for the remote-control viewer; the session
//! is one without the screen (`RemoteControlRequest { screen: false }`, see
//! `continuity-daemon`).
//!
//! This module is the part every platform shares: which computer is placed
//! at which edge (`ControlState::set_shared_edge`), keeping an idle
//! input-only session open with each one that's connected, deciding when
//! the cursor has crossed over and come back, forwarding while it's over
//! there, and the escape chord. How this computer's input is watched and
//! held is each platform's own (`mac.rs`, `windows.rs`, `linux.rs`), behind
//! the same small `Backend` surface:
//!
//! - **macOS**: a session-level `CGEventTap`; the cursor is hidden and
//!   unlinked from the mouse while it's over there.
//! - **Windows**: low-level mouse and keyboard hooks; the cursor waits in the
//!   middle of the screen, so every movement reads as a delta.
//! - **Linux**: the InputCapture portal — the compositor draws pointer
//!   barriers on the placed edges, hides the cursor and hands over the
//!   captured input through libei, which is the only way a Wayland app may
//!   capture input at all.
//!
//! **Crossing.** On macOS and Windows the backend reports every move with
//! where the cursor is; a move outward through a placed edge with a session
//! up crosses. On Linux the compositor decides and says so (`crossed`). The
//! other computer's pointer is then put at the matching point of its
//! opposite edge — placed exactly when it said its screen size, else moved
//! there with moves longer than any screen (`PIN`) — and from then on every
//! mouse and keyboard event is forwarded: moves as deltas, buttons, the
//! wheel, keys through the viewer's layout translation (Command and Control
//! trade places between a Mac and a PC). When the other computer reports its
//! pointer against the edge facing this screen (`PointerReachedEdge`), the
//! cursor comes back at the matching point of this screen's edge, a few
//! pixels in.
//!
//! **Getting back no matter what.** Double-tapping Control brings the mouse
//! and keyboard back on this computer alone, before anything is sent, so it
//! works when the other computer has frozen or the network has gone. A
//! session ending, the peer disconnecting and the system taking input back
//! all release too, each first letting go of whatever was held down over
//! there.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use mac as platform;
#[cfg(target_os = "windows")]
use self::windows as platform;

use crate::remote_viewer::{KeyTranslator, KeyboardForwarder};
use continuity_daemon::{EngineCommand, RemoteControlRole, SyncEvent};
use continuity_proto::{InputEventKind, MouseButton as ProtoMouseButton, Platform, ScreenEdge, ScreenSize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::keyboard::KeyCode;
use tokio::sync::mpsc::UnboundedSender;

/// Two Control presses this close together, with nothing else in between,
/// bring the mouse and keyboard back to this computer.
const ESCAPE_DOUBLE_TAP: Duration = Duration::from_millis(400);

/// How far in from the edge the cursor is put when it comes back — past
/// the edge check's one pixel, so standing there isn't already crossing.
const RETURN_INSET: f64 = 4.0;

/// After the cursor comes back, how long a move at the edge is left alone
/// — the moves of the gesture that brought it back are still arriving.
const RECROSS_HOLDOFF: Duration = Duration::from_millis(300);

/// A move at least this long pins the other computer's pointer to an edge
/// wherever it was (see `arrival_moves`); the Linux host recognizes it.
const PIN: f64 = 1_000_000.0;

/// What one wheel notch is forwarded as — the viewer's own figure, so a
/// notch scrolls the same however the other computer is driven.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) const WHEEL_NOTCH: f64 = 24.0;

/// Told when the mouse crosses over or comes back, or a session it was over
/// at ends (for the window's status). Never from `set_placements`, which
/// the event loop calls in response to this.
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// A point in this computer's screen coordinates, as its backend reports
/// them (points on macOS, pixels on Windows, logical pixels on Linux).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Point {
    pub x: f64,
    pub y: f64,
}

/// The screen whose edges are watched, in the same coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One input event on this computer, as a backend reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum LocalInput {
    /// The mouse moved to `at`, by `delta`.
    Moved { at: Point, delta: (f64, f64) },
    Button { button: ProtoMouseButton, down: bool },
    /// In the viewer's convention: pixels, positive scrolls up and left.
    Scroll { dx: f64, dy: f64 },
    /// `None` for a key `KEY_CODES` doesn't know — still swallowed while
    /// the mouse is over there, just not forwarded.
    Key { key: Option<KeyCode>, down: bool },
}

/// What's known about a computer placed at an edge, once its session is up.
#[derive(Clone)]
struct Target {
    name: String,
    platform: Platform,
    /// The other screen's size, when it said — without it the pointer is
    /// put at an edge with moves longer than any screen (`arrival_moves`).
    screen_size: Option<ScreenSize>,
}

/// The mouse and keyboard are over there.
struct Grab {
    peer_id: String,
    keyboard: KeyboardForwarder,
    buttons: Vec<ProtoMouseButton>,
    /// Control keys down right now, so a held one's auto-repeat (Windows
    /// repeats them) isn't taken for a second tap.
    controls_held: Vec<KeyCode>,
    /// Where the cursor comes back to on escape: by the edge it left
    /// through, a few pixels in.
    left_at: Point,
    last_control_tap: Option<Instant>,
}

#[derive(Default)]
struct State {
    /// Which edge each paired computer is placed at.
    placements: HashMap<String, ScreenEdge>,
    /// Connected peers' platforms, for the key translation.
    platforms: HashMap<String, Platform>,
    /// Peers with an input-only session up.
    sessions: HashMap<String, Target>,
    grab: Option<Grab>,
    /// Whether the backend is watching input (started once there's
    /// something to watch for, and the permission for it).
    running: bool,
    /// When the cursor last came back (see `RECROSS_HOLDOFF`).
    released_at: Option<Instant>,
    /// Whether the person here has been told the backend couldn't start.
    warned: bool,
}

/// What the backends hold on to, and call back into.
pub(crate) struct Shared {
    commands: UnboundedSender<EngineCommand>,
    state: Mutex<State>,
    on_change: OnChange,
    backend: platform::Backend,
}

pub struct InputShare {
    shared: Arc<Shared>,
}

impl InputShare {
    /// `profile` scopes anything the backend keeps between runs (the Linux
    /// portal's restore token).
    pub fn new(commands: UnboundedSender<EngineCommand>, on_change: OnChange, profile: &str) -> Self {
        let backend = platform::Backend::new(profile);
        Self { shared: Arc::new(Shared { commands, state: Mutex::new(State::default()), on_change, backend }) }
    }

    /// The peer this computer's mouse and keyboard are over at right now.
    pub fn sharing_with(&self) -> Option<String> {
        self.shared.state.lock().unwrap().grab.as_ref().map(|g| g.peer_id.clone())
    }

    /// Puts `placements` into effect: a computer newly placed and connected
    /// gets a session asked for; one no longer placed has its session
    /// ended, and the mouse back if it was over there.
    pub fn set_placements(&self, placements: HashMap<String, ScreenEdge>) {
        let shared = &self.shared;
        let (request, end) = {
            let mut state = shared.state.lock().unwrap();
            let request: Vec<String> = placements
                .keys()
                .filter(|id| !state.placements.contains_key(*id) && state.platforms.contains_key(*id) && !state.sessions.contains_key(*id))
                .cloned()
                .collect();
            let end: Vec<String> = state.sessions.keys().filter(|id| !placements.contains_key(*id)).cloned().collect();
            if state.grab.as_ref().is_some_and(|g| end.contains(&g.peer_id)) {
                shared.release(&mut state, None);
            }
            state.placements = placements.clone();
            (request, end)
        };
        for peer_id in request {
            let _ = shared.commands.send(EngineCommand::RequestRemoteControl { peer_crypto_id: peer_id, screen: false });
        }
        for peer_id in end {
            let _ = shared.commands.send(EngineCommand::EndRemoteControlSession { peer_crypto_id: peer_id });
        }
        self.ensure_running();
        shared.backend.placements_changed(&shared.watched_edges());
        // Deliberately no `on_change` here: the event loop calls this *from*
        // its handler for that very change, so one would feed the other
        // forever. **Real bug fixed here**: it did, and the first build with
        // this feature spun its event loop at full CPU from the moment it
        // started, the tray dead. `on_change` is for the mouse crossing over
        // and coming back, which nothing else reports.
    }

    /// Follows the engine: sessions coming and going, the pointer over
    /// there reaching the edge that faces this screen.
    pub fn observe(&self, event: &SyncEvent) {
        let shared = &self.shared;
        let mut state = shared.state.lock().unwrap();
        match event {
            SyncEvent::Connected { peer } => {
                state.platforms.insert(peer.id.clone(), peer.platform);
                if state.placements.contains_key(&peer.id) && !state.sessions.contains_key(&peer.id) {
                    let _ = shared.commands.send(EngineCommand::RequestRemoteControl { peer_crypto_id: peer.id.clone(), screen: false });
                }
                // The permission may have been granted since the backend
                // last tried; a connection is a good moment to try again.
                drop(state);
                self.ensure_running();
            }
            SyncEvent::Disconnected { peer_id, .. } | SyncEvent::WasRevoked { peer_id, .. } | SyncEvent::RevokedByPeer { peer_id, .. } => {
                state.platforms.remove(peer_id);
                if state.sessions.remove(peer_id).is_some() {
                    if state.grab.as_ref().is_some_and(|g| &g.peer_id == peer_id) {
                        shared.release(&mut state, None);
                    }
                    drop(state);
                    shared.backend.placements_changed(&shared.watched_edges());
                    (shared.on_change)();
                }
            }
            SyncEvent::WasReset => {
                state.platforms.clear();
                state.sessions.clear();
                if state.grab.is_some() {
                    shared.release(&mut state, None);
                }
                drop(state);
                shared.backend.placements_changed(&shared.watched_edges());
            }
            SyncEvent::RemoteControlSessionStarted { peer_id, peer_name, role, screen: false, screen_size, .. }
                if *role == RemoteControlRole::Controlling =>
            {
                let platform = state.platforms.get(peer_id).copied().unwrap_or(Platform::MacOs);
                state.sessions.insert(peer_id.clone(), Target { name: peer_name.clone(), platform, screen_size: *screen_size });
                tracing::info!("sharing the keyboard and mouse with '{peer_name}' when the cursor crosses to it");
                drop(state);
                shared.backend.placements_changed(&shared.watched_edges());
            }
            SyncEvent::RemoteControlSessionEnded { peer_id, .. } => {
                if state.sessions.remove(peer_id).is_some() {
                    if state.grab.as_ref().is_some_and(|g| &g.peer_id == peer_id) {
                        shared.release(&mut state, None);
                    }
                    drop(state);
                    shared.backend.placements_changed(&shared.watched_edges());
                    (shared.on_change)();
                }
            }
            SyncEvent::RemoteControlDeclined { peer_id, peer_name } => {
                if state.placements.contains_key(peer_id) {
                    tracing::info!("'{peer_name}' declined to share its keyboard and mouse; placed at an edge, it will be asked again when it reconnects");
                }
            }
            SyncEvent::PointerReachedEdge { peer_id, edge, position } => {
                let coming_back =
                    state.grab.as_ref().is_some_and(|g| &g.peer_id == peer_id) && state.placements.get(peer_id).map(|placed| placed.opposite()) == Some(*edge);
                if coming_back {
                    let placed = state.placements[peer_id];
                    let at = shared.backend.screen().map(|bounds| point_on_edge(bounds, placed, *position));
                    shared.release(&mut state, at);
                    drop(state);
                    (shared.on_change)();
                }
            }
            _ => {}
        }
    }

    /// Starts the backend if it isn't running and there's a placement to
    /// watch for. When it can't (a permission missing, no portal), says so
    /// once and tries again at the next placement change or connection.
    fn ensure_running(&self) {
        {
            let state = self.shared.state.lock().unwrap();
            if state.running || state.placements.is_empty() {
                return;
            }
        }
        match self.shared.backend.start(&self.shared) {
            Ok(()) => self.shared.state.lock().unwrap().running = true,
            Err(why) => self.shared.backend_failed(&why),
        }
    }
}

impl Shared {
    /// The backend couldn't start, or stopped: logged, said once, and tried
    /// again at the next placement change or connection.
    pub(crate) fn backend_failed(&self, why: &str) {
        tracing::warn!("keyboard-and-mouse sharing can't watch this computer's input: {why}");
        let mut state = self.state.lock().unwrap();
        state.running = false;
        if state.grab.is_some() {
            self.release(&mut state, None);
        }
        if !state.warned {
            state.warned = true;
            drop(state);
            crate::notify(&format!("Keyboard and mouse sharing can't start: {why}"));
        }
    }

    /// The placed edges with a session up — the ones worth watching.
    fn watched_edges(&self) -> Vec<ScreenEdge> {
        let state = self.state.lock().unwrap();
        state.placements.iter().filter(|(peer_id, _)| state.sessions.contains_key(*peer_id)).map(|(_, edge)| *edge).collect()
    }

    /// Whether the mouse and keyboard are over there right now.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn is_grabbed(&self) -> bool {
        self.state.lock().unwrap().grab.is_some()
    }

    /// One event from the backend. Returns whether to swallow it: every
    /// event while the mouse is over there, none while it's here.
    pub(crate) fn on_input(&self, input: LocalInput) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.grab.is_none() {
            if let LocalInput::Moved { at, delta } = input {
                if let Some((peer_id, edge)) = crossing(&state, self.backend.screen(), at, delta) {
                    let position = self.backend.screen().and_then(|bounds| edge_of(at, bounds)).map_or(0.5, |(_, position)| position);
                    self.grab(&mut state, peer_id, edge, position, at);
                    drop(state);
                    (self.on_change)();
                }
            }
            return false;
        }
        self.forward_grabbed(state, input)
    }

    /// One event to forward if the mouse is over there, and nothing
    /// otherwise — never a crossing check. For a backend whose events
    /// carry no position (Linux: the compositor decides crossings).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn forward(&self, input: LocalInput) {
        let state = self.state.lock().unwrap();
        if state.grab.is_some() {
            self.forward_grabbed(state, input);
        }
    }

    /// `on_input` while the mouse is over there.
    fn forward_grabbed(&self, mut state: std::sync::MutexGuard<'_, State>, input: LocalInput) -> bool {
        let commands = &self.commands;
        let Some(grabbed) = state.grab.as_mut() else { return false };
        let peer_id = grabbed.peer_id.clone();
        let send = |event: InputEventKind| {
            let _ = commands.send(EngineCommand::SendInputEvent { peer_crypto_id: peer_id.clone(), event });
        };
        match input {
            LocalInput::Moved { delta: (dx, dy), .. } => {
                if dx != 0.0 || dy != 0.0 {
                    send(InputEventKind::MouseMoveRelative { dx, dy });
                }
            }
            LocalInput::Button { button, down } => {
                if down {
                    if !grabbed.buttons.contains(&button) {
                        grabbed.buttons.push(button);
                    }
                } else {
                    grabbed.buttons.retain(|&held| held != button);
                }
                send(InputEventKind::MouseButton { button, down });
            }
            LocalInput::Scroll { dx, dy } => {
                if dx != 0.0 || dy != 0.0 {
                    send(InputEventKind::MouseScroll { delta_x: dx, delta_y: dy });
                }
            }
            LocalInput::Key { key: None, .. } => {
                grabbed.last_control_tap = None;
            }
            LocalInput::Key { key: Some(key), down } => {
                if matches!(key, KeyCode::ControlLeft | KeyCode::ControlRight) {
                    let repeat = down && grabbed.controls_held.contains(&key);
                    if down && !repeat {
                        grabbed.controls_held.push(key);
                        // The escape chord: Control twice in a row.
                        let now = Instant::now();
                        if grabbed.last_control_tap.is_some_and(|last| now.duration_since(last) <= ESCAPE_DOUBLE_TAP) {
                            let back_to = grabbed.left_at;
                            self.release(&mut state, Some(back_to));
                            drop(state);
                            (self.on_change)();
                            return true;
                        }
                        grabbed.last_control_tap = Some(now);
                    } else if !down {
                        grabbed.controls_held.retain(|&held| held != key);
                    }
                } else {
                    grabbed.last_control_tap = None;
                }
                if let Some(forwarded) = grabbed.keyboard.key(key, down) {
                    send(forwarded);
                }
            }
        }
        true
    }

    /// The system decided the cursor crossed `edge` at `position` along it
    /// (Linux: the compositor's pointer barrier). Returns whether the mouse
    /// is now over there — `false` when nothing is placed there with a
    /// session up, and the backend should give input back.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn crossed(&self, edge: ScreenEdge, position: f64, at: Point) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.grab.is_some() {
            return true;
        }
        let Some(peer_id) = placed_with_session(&state, edge) else { return false };
        self.grab(&mut state, peer_id, edge, position, at);
        drop(state);
        (self.on_change)();
        true
    }

    /// The system took input back on its own (Linux: the compositor ended
    /// the capture). The backend has nothing more to undo.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn lost(&self) {
        let mut state = self.state.lock().unwrap();
        if state.grab.is_some() {
            self.release(&mut state, None);
            drop(state);
            (self.on_change)();
        }
    }

    /// Takes the mouse and keyboard over to `peer_id`, the cursor having
    /// left through `edge` at `position` along it.
    fn grab(&self, state: &mut State, peer_id: String, edge: ScreenEdge, position: f64, at: Point) {
        let Some(target) = state.sessions.get(&peer_id).cloned() else { return };
        // A computer that can place its pointer (it said how big its screen
        // is) gets the exact point; one that can only move it (Linux,
        // input-only) gets moves that pin it to the arriving edge.
        let events = match target.screen_size {
            Some(_) => {
                let (x, y) = arrival(edge, position);
                vec![InputEventKind::MouseMove { x, y }]
            }
            None => arrival_moves(edge, position, None),
        };
        for event in events {
            let _ = self.commands.send(EngineCommand::SendInputEvent { peer_crypto_id: peer_id.clone(), event });
        }
        let left_at = self.backend.screen().map_or(at, |bounds| point_on_edge(bounds, edge, position));
        self.backend.grabbed(left_at);
        tracing::info!("keyboard and mouse now go to '{}'", target.name);
        state.grab = Some(Grab {
            peer_id,
            keyboard: KeyboardForwarder::new(KeyTranslator::new(cfg!(target_os = "macos"), target.platform)),
            buttons: Vec::new(),
            controls_held: Vec::new(),
            left_at,
            last_control_tap: None,
        });
    }

    /// Brings the mouse and keyboard back, letting go of whatever was held
    /// down over there first. `at`: where the cursor appears on this screen
    /// (where it left, without one).
    fn release(&self, state: &mut State, at: Option<Point>) {
        let Some(mut grabbed) = state.grab.take() else { return };
        for &button in grabbed.buttons.iter().rev() {
            let _ = self.commands.send(EngineCommand::SendInputEvent {
                peer_crypto_id: grabbed.peer_id.clone(),
                event: InputEventKind::MouseButton { button, down: false },
            });
        }
        for event in grabbed.keyboard.release_all() {
            let _ = self.commands.send(EngineCommand::SendInputEvent { peer_crypto_id: grabbed.peer_id.clone(), event });
        }
        self.backend.released(at.unwrap_or(grabbed.left_at));
        state.released_at = Some(Instant::now());
        tracing::info!("keyboard and mouse are back on this computer");
    }
}

/// The peer placed at `edge` with a session up, if any.
fn placed_with_session(state: &State, edge: ScreenEdge) -> Option<String> {
    state.placements.iter().find(|(peer_id, placed)| **placed == edge && state.sessions.contains_key(*peer_id)).map(|(peer_id, _)| peer_id.clone())
}

/// The peer the cursor at `at` has just crossed onto, if any, and through
/// which edge: at a placed edge of `screen`, moving *outward* through it
/// (`delta`, the move that got it there), with a session up for the
/// computer placed there — and not within `RECROSS_HOLDOFF` of coming back.
/// **Real bug fixed here**: the cursor came back one pixel inside the edge,
/// which the edge check counts as at it, so the first movement after
/// returning crossed straight over again — the mouse was trapped on the
/// other computer, and only the escape chord brought it home.
fn crossing(state: &State, screen: Option<Bounds>, at: Point, delta: (f64, f64)) -> Option<(String, ScreenEdge)> {
    if state.released_at.is_some_and(|released| released.elapsed() < RECROSS_HOLDOFF) {
        return None;
    }
    let (edge, _) = edge_of(at, screen?)?;
    let outward = match edge {
        ScreenEdge::Left => delta.0 < 0.0,
        ScreenEdge::Right => delta.0 > 0.0,
        ScreenEdge::Top => delta.1 < 0.0,
        ScreenEdge::Bottom => delta.1 > 0.0,
    };
    if !outward {
        return None;
    }
    placed_with_session(state, edge).map(|peer_id| (peer_id, edge))
}

/// Which edge of `bounds` `point` is pressed against, if any, and how far
/// along it (0.0 at the top or left). Within a pixel counts, and so does
/// beyond: a backend can report where the cursor *would* have gone.
pub(crate) fn edge_of(point: Point, bounds: Bounds) -> Option<(ScreenEdge, f64)> {
    if bounds.width <= 1.0 || bounds.height <= 1.0 {
        return None;
    }
    let (left, top) = (bounds.x, bounds.y);
    let (right, bottom) = (left + bounds.width - 1.0, top + bounds.height - 1.0);
    let along_y = ((point.y - top) / (bottom - top)).clamp(0.0, 1.0);
    let along_x = ((point.x - left) / (right - left)).clamp(0.0, 1.0);
    if point.x <= left + 1.0 {
        Some((ScreenEdge::Left, along_y))
    } else if point.x >= right - 1.0 {
        Some((ScreenEdge::Right, along_y))
    } else if point.y <= top + 1.0 {
        Some((ScreenEdge::Top, along_x))
    } else if point.y >= bottom - 1.0 {
        Some((ScreenEdge::Bottom, along_x))
    } else {
        None
    }
}

/// The point `position` (0.0..=1.0) of the way along `edge` of `bounds`,
/// `RETURN_INSET` in — clear of the edge check, so a cursor put there isn't
/// at the edge until it's moved there again.
pub(crate) fn point_on_edge(bounds: Bounds, edge: ScreenEdge, position: f64) -> Point {
    let (left, top) = (bounds.x, bounds.y);
    let (right, bottom) = (left + bounds.width - 1.0, top + bounds.height - 1.0);
    let position = position.clamp(0.0, 1.0);
    match edge {
        ScreenEdge::Left => Point { x: left + RETURN_INSET, y: top + position * (bottom - top) },
        ScreenEdge::Right => Point { x: right - RETURN_INSET, y: top + position * (bottom - top) },
        ScreenEdge::Top => Point { x: left + position * (right - left), y: top + RETURN_INSET },
        ScreenEdge::Bottom => Point { x: left + position * (right - left), y: bottom - RETURN_INSET },
    }
}

/// Where the other computer's pointer appears for a cursor leaving through
/// `edge` at `position`: the matching point of the opposite edge, as the
/// normalized position `MouseMove` takes.
fn arrival(edge: ScreenEdge, position: f64) -> (f64, f64) {
    let position = position.clamp(0.0, 1.0);
    match edge {
        ScreenEdge::Left => (1.0, position),
        ScreenEdge::Right => (0.0, position),
        ScreenEdge::Top => (position, 1.0),
        ScreenEdge::Bottom => (position, 0.0),
    }
}

/// The moves that put the other computer's pointer where `arrival` says
/// when it can only be moved, not placed: a pin to the top (or left), then
/// a pin to the arriving edge — last, so a host that tracks pins (Linux)
/// takes that one as the edge the pointer is at — then along the edge by
/// `position` of `screen`, or nothing more with the size unknown.
fn arrival_moves(edge: ScreenEdge, position: f64, screen: Option<ScreenSize>) -> Vec<InputEventKind> {
    let position = position.clamp(0.0, 1.0);
    let (secondary, arriving) = match edge {
        ScreenEdge::Left => ((0.0, -PIN), (PIN, 0.0)),
        ScreenEdge::Right => ((0.0, -PIN), (-PIN, 0.0)),
        ScreenEdge::Top => ((-PIN, 0.0), (0.0, PIN)),
        ScreenEdge::Bottom => ((-PIN, 0.0), (0.0, -PIN)),
    };
    let mut moves = vec![
        InputEventKind::MouseMoveRelative { dx: secondary.0, dy: secondary.1 },
        InputEventKind::MouseMoveRelative { dx: arriving.0, dy: arriving.1 },
    ];
    if let Some(size) = screen {
        let along = match edge {
            ScreenEdge::Left | ScreenEdge::Right => InputEventKind::MouseMoveRelative { dx: 0.0, dy: position * f64::from(size.height) },
            ScreenEdge::Top | ScreenEdge::Bottom => InputEventKind::MouseMoveRelative { dx: position * f64::from(size.width), dy: 0.0 },
        };
        moves.push(along);
    }
    moves
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Bounds = Bounds { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 };

    #[test]
    fn a_cursor_leaving_one_edge_arrives_at_the_opposite_one() {
        assert_eq!(arrival(ScreenEdge::Right, 0.25), (0.0, 0.25));
        assert_eq!(arrival(ScreenEdge::Left, 0.75), (1.0, 0.75));
        assert_eq!(arrival(ScreenEdge::Top, 0.1), (0.1, 1.0));
        assert_eq!(arrival(ScreenEdge::Bottom, 0.9), (0.9, 0.0));
        assert_eq!(arrival(ScreenEdge::Right, 7.0), (0.0, 1.0), "clamped");
    }

    #[test]
    fn arrival_by_moves_pins_to_the_edge_last_then_goes_along_it() {
        let size = Some(ScreenSize { width: 1920, height: 1080 });
        // Leaving through the right edge: arrive at the other's left.
        let moves = arrival_moves(ScreenEdge::Right, 0.5, size);
        assert!(matches!(moves[0], InputEventKind::MouseMoveRelative { dx, dy } if dx == 0.0 && dy <= -PIN), "top first");
        assert!(matches!(moves[1], InputEventKind::MouseMoveRelative { dx, dy } if dx <= -PIN && dy == 0.0), "then the left edge");
        assert!(matches!(moves[2], InputEventKind::MouseMoveRelative { dx, dy } if dx == 0.0 && dy == 540.0));
        let moves = arrival_moves(ScreenEdge::Top, 0.25, size);
        assert!(matches!(moves[1], InputEventKind::MouseMoveRelative { dx, dy } if dx == 0.0 && dy >= PIN), "the bottom edge");
        assert!(matches!(moves[2], InputEventKind::MouseMoveRelative { dx, dy } if dx == 480.0 && dy == 0.0));
        assert_eq!(arrival_moves(ScreenEdge::Left, 0.5, None).len(), 2, "no size: the pins alone");
    }

    #[test]
    fn the_pointer_is_at_an_edge_only_when_pressed_against_one() {
        assert_eq!(edge_of(Point { x: 960.0, y: 540.0 }, SCREEN), None);
        assert_eq!(edge_of(Point { x: 0.0, y: 270.0 }, SCREEN), Some((ScreenEdge::Left, 270.0 / 1079.0)));
        assert_eq!(edge_of(Point { x: 1919.0, y: 1079.0 }, SCREEN).map(|(e, _)| e), Some(ScreenEdge::Right), "a corner counts as a side");
        assert_eq!(edge_of(Point { x: 500.0, y: 0.5 }, SCREEN).map(|(e, _)| e), Some(ScreenEdge::Top));
        assert_eq!(edge_of(Point { x: 500.0, y: 1078.5 }, SCREEN).map(|(e, _)| e), Some(ScreenEdge::Bottom));
        assert_eq!(edge_of(Point { x: 1950.0, y: 10.0 }, SCREEN).map(|(e, _)| e), Some(ScreenEdge::Right), "past the edge counts");
        let second = Bounds { x: 1920.0, y: 0.0, width: 1000.0, height: 1000.0 };
        assert_eq!(edge_of(Point { x: 1920.0, y: 500.0 }, second).map(|(e, _)| e), Some(ScreenEdge::Left));
        assert_eq!(edge_of(Point { x: 2400.0, y: 500.0 }, second), None);
    }

    #[test]
    fn the_cursor_comes_back_clear_of_the_edge() {
        let p = point_on_edge(SCREEN, ScreenEdge::Right, 0.5);
        assert_eq!((p.x, p.y), (1915.0, 539.5));
        let p = point_on_edge(SCREEN, ScreenEdge::Top, 0.0);
        assert_eq!((p.x, p.y), (0.0, 4.0));
        // Where it lands is *not* at the edge, or it would cross straight back.
        for edge in [ScreenEdge::Left, ScreenEdge::Right, ScreenEdge::Top, ScreenEdge::Bottom] {
            assert_eq!(edge_of(point_on_edge(SCREEN, edge, 0.3), SCREEN), None, "{edge:?}");
        }
    }

    #[test]
    fn crossing_needs_a_placed_edge_a_session_and_an_outward_move() {
        let mut state = State::default();
        state.placements.insert("pc".into(), ScreenEdge::Right);
        let right = Point { x: 1919.0, y: 10.0 };
        let outward = (3.0, 0.0);
        assert_eq!(crossing(&state, Some(SCREEN), right, outward), None, "placed, but no session yet");
        state.sessions.insert("pc".into(), Target { name: "PC".into(), platform: Platform::Windows, screen_size: None });
        assert_eq!(crossing(&state, Some(SCREEN), right, outward), Some(("pc".to_string(), ScreenEdge::Right)));
        assert_eq!(crossing(&state, Some(SCREEN), right, (-3.0, 0.0)), None, "moving away from the edge isn't crossing");
        assert_eq!(crossing(&state, Some(SCREEN), right, (0.0, 5.0)), None, "nor moving along it");
        assert_eq!(crossing(&state, Some(SCREEN), Point { x: 960.0, y: 540.0 }, outward), None);
        assert_eq!(crossing(&state, Some(SCREEN), Point { x: 0.0, y: 10.0 }, (-3.0, 0.0)), None, "nothing placed there");
        assert_eq!(crossing(&state, None, right, outward), None, "no screen known");
        state.released_at = Some(Instant::now());
        assert_eq!(crossing(&state, Some(SCREEN), right, outward), None, "just came back: not again yet");
    }
}

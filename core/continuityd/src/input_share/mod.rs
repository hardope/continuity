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
//! - **Windows**: low-level mouse and keyboard hooks; the cursor stays where
//!   it left, and each movement is measured from there.
//! - **Linux**: the InputCapture portal — the compositor draws pointer
//!   barriers on the placed edges, hides the cursor and hands over the
//!   captured input through libei, which is the only way a Wayland app may
//!   capture input at all.
//!
//! **Crossing.** On macOS and Windows the backend reports every move with
//! where the cursor is; a move outward through a placed edge with a session
//! up crosses. On Linux the compositor decides and says so (`crossed`). The
//! other computer's pointer is then put at the matching point of its
//! opposite edge, a few pixels in — placed exactly when it said its screen
//! size, else moved there with moves longer than any screen (`PIN`) — and
//! from then on every mouse and keyboard event is forwarded: moves as
//! deltas, buttons, the wheel, keys through the viewer's layout translation
//! (Command and Control trade places between a Mac and a PC). When the
//! other computer reports its pointer against the edge facing this screen
//! (`PointerReachedEdge`), the cursor comes back at the matching point of
//! this screen's edge, a few pixels in.
//!
//! **Never while being driven.** Two computers placed at each other's edges
//! each control the other. Input another computer is injecting here
//! (`note_remote_input`) never crosses over: the backends leave injected
//! events alone where they can tell them apart (macOS, Windows), and no
//! crossing is taken while such input is arriving (Linux, where the
//! compositor's barrier can't tell). **Real bug fixed here**: on a Mac
//! being driven from Windows, the Mac's own event tap took the moves it was
//! injecting for Windows as the Mac's mouse — the pointer, driven to the
//! Mac's edge facing Windows, "crossed" back, both computers' input went
//! round in a loop, and the cursor vanished on one of them.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tao::keyboard::KeyCode;
use tokio::sync::mpsc::UnboundedSender;

/// Two Control presses this close together, with nothing else in between,
/// bring the mouse and keyboard back to this computer.
const ESCAPE_DOUBLE_TAP: Duration = Duration::from_millis(400);

/// How far in from the edge the cursor is put when it comes back, and the
/// other computer's pointer when it arrives — past the edge checks' one
/// pixel, so standing there isn't already crossing.
const RETURN_INSET: f64 = 4.0;

/// After the cursor comes back, how long a move at the edge is left alone
/// — the moves of the gesture that brought it back are still arriving.
const RECROSS_HOLDOFF: Duration = Duration::from_millis(300);

/// How long after input another computer injected here the cursor can't
/// cross over (see the module docs).
const REMOTE_INPUT_QUIET: Duration = Duration::from_millis(400);

/// After a session ended with nothing wrong (the connection blinked, a
/// screen session took its place), how long before asking for a new one —
/// if the computer is still placed and connected by then.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(2);

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

/// When input another computer sent was last injected here, in
/// milliseconds since `clock()` started (0: never).
static REMOTE_INPUT_AT: AtomicU64 = AtomicU64::new(0);

fn clock() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Called by this computer's remote-control host for every event another
/// computer has it inject: while those arrive, the cursor here is being
/// driven from over there, and never crosses over on its own account.
pub(crate) fn note_remote_input() {
    REMOTE_INPUT_AT.store(clock(), Ordering::Relaxed);
}

fn driven_from_elsewhere() -> bool {
    let at = REMOTE_INPUT_AT.load(Ordering::Relaxed);
    at != 0 && clock().saturating_sub(at) < REMOTE_INPUT_QUIET.as_millis() as u64
}

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
    /// The engine's session, so the end of some other session with the
    /// same computer (it controlling this one, say) isn't taken for this
    /// one's.
    session_id: String,
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
    /// Keys and buttons that were already down here when the mouse went
    /// over: they stay this computer's until let go, so the release isn't
    /// swallowed and left stuck down here.
    keys_here: Vec<KeyCode>,
    buttons_here: Vec<ProtoMouseButton>,
    /// The fractions of a pixel not yet forwarded, of moves and of
    /// scrolling: a host injects whole pixels, and a slow mouse (libei's
    /// are fractional) would otherwise not move it at all.
    move_carry: (f64, f64),
    scroll_carry: (f64, f64),
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
    /// Screen sessions this computer has open, by peer: sharing's session
    /// with that computer gives way to one, and is asked for again once it
    /// ends.
    screen_sessions: HashMap<String, String>,
    /// Keys and buttons down here while the mouse is here — see
    /// `Grab::keys_here`.
    keys_here: Vec<KeyCode>,
    buttons_here: Vec<ProtoMouseButton>,
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
                state.screen_sessions.remove(peer_id);
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
                state.screen_sessions.clear();
                if state.grab.is_some() {
                    shared.release(&mut state, None);
                }
                drop(state);
                shared.backend.placements_changed(&shared.watched_edges());
            }
            SyncEvent::RemoteControlSessionStarted { peer_id, session_id, role: RemoteControlRole::Controlling, screen: true, .. } => {
                state.screen_sessions.insert(peer_id.clone(), session_id.clone());
            }
            SyncEvent::RemoteControlSessionStarted { peer_id, peer_name, session_id, role: RemoteControlRole::Controlling, screen: false, screen_size } => {
                let platform = state.platforms.get(peer_id).copied().unwrap_or(Platform::MacOs);
                let target = Target { session_id: session_id.clone(), name: peer_name.clone(), platform, screen_size: *screen_size };
                state.sessions.insert(peer_id.clone(), target);
                tracing::info!("sharing the keyboard and mouse with '{peer_name}' when the cursor crosses to it (its screen {screen_size:?})");
                drop(state);
                shared.backend.placements_changed(&shared.watched_edges());
            }
            SyncEvent::RemoteControlSessionEnded { peer_id, peer_name, session_id, reason } => {
                if state.screen_sessions.get(peer_id) == Some(session_id) {
                    // A screen session took sharing's place; sharing comes
                    // back now it's over.
                    state.screen_sessions.remove(peer_id);
                    drop(state);
                    shared.ask_again_later(peer_id.clone());
                    return;
                }
                if !state.sessions.get(peer_id).is_some_and(|target| &target.session_id == session_id) {
                    return;
                }
                state.sessions.remove(peer_id);
                if state.grab.as_ref().is_some_and(|g| &g.peer_id == peer_id) {
                    shared.release(&mut state, None);
                }
                drop(state);
                shared.backend.placements_changed(&shared.watched_edges());
                (shared.on_change)();
                match reason {
                    // Nothing wrong: asked for again, if it's still placed
                    // and connected by then.
                    None => shared.ask_again_later(peer_id.clone()),
                    Some(reason) => tracing::info!("keyboard-and-mouse sharing with '{peer_name}' ended: {reason}"),
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

    /// Asks `peer_id` for a session again after `ASK_AGAIN_AFTER`, if by
    /// then it's still placed and connected, with no session — sharing's or
    /// a screen one — up or on its way. (The engine drops a request while
    /// one is pending.)
    fn ask_again_later(self: &Arc<Self>, peer_id: String) {
        let shared = self.clone();
        let spawned = std::thread::Builder::new().name("input-share-ask".to_string()).spawn(move || {
            std::thread::sleep(ASK_AGAIN_AFTER);
            let state = shared.state.lock().unwrap();
            let wanted = state.placements.contains_key(&peer_id)
                && state.platforms.contains_key(&peer_id)
                && !state.sessions.contains_key(&peer_id)
                && !state.screen_sessions.contains_key(&peer_id);
            drop(state);
            if wanted {
                let _ = shared.commands.send(EngineCommand::RequestRemoteControl { peer_crypto_id: peer_id, screen: false });
            }
        });
        if let Err(e) = spawned {
            tracing::debug!("couldn't schedule asking for keyboard-and-mouse sharing again: {e}");
        }
    }

    /// The placed edges with a session up — the ones worth watching.
    fn watched_edges(&self) -> Vec<ScreenEdge> {
        let state = self.state.lock().unwrap();
        let mut edges: Vec<ScreenEdge> =
            state.placements.iter().filter(|(peer_id, _)| state.sessions.contains_key(*peer_id)).map(|(_, edge)| *edge).collect();
        edges.sort_by_key(|edge| *edge as u8);
        edges.dedup();
        edges
    }

    /// Whether the mouse and keyboard are over there right now.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn is_grabbed(&self) -> bool {
        self.state.lock().unwrap().grab.is_some()
    }

    /// One event from the backend. Returns whether to swallow it: every
    /// event while the mouse is over there, including the move that took it
    /// there, and none while it's here.
    pub(crate) fn on_input(&self, input: LocalInput) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.grab.is_none() {
            match input {
                LocalInput::Moved { at, delta } => {
                    if let Some((peer_id, edge)) = crossing(&state, self.backend.screen(), at, delta, driven_from_elsewhere()) {
                        let position = self.backend.screen().and_then(|bounds| edge_of(at, bounds)).map_or(0.5, |(_, position)| position);
                        self.grab(&mut state, peer_id, edge, position, at);
                        let taken = state.grab.is_some();
                        drop(state);
                        (self.on_change)();
                        return taken;
                    }
                }
                LocalInput::Key { key: Some(key), down } => held(&mut state.keys_here, key, down),
                LocalInput::Button { button, down } => held(&mut state.buttons_here, button, down),
                LocalInput::Key { key: None, .. } | LocalInput::Scroll { .. } => {}
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
                let (dx, dy) = (whole(&mut grabbed.move_carry.0, dx), whole(&mut grabbed.move_carry.1, dy));
                if dx != 0.0 || dy != 0.0 {
                    send(InputEventKind::MouseMoveRelative { dx, dy });
                }
            }
            LocalInput::Button { button, down } => {
                if grabbed.buttons_here.contains(&button) {
                    // Down since before the mouse went over: this
                    // computer's to let go of.
                    if !down {
                        grabbed.buttons_here.retain(|&held| held != button);
                    }
                    return false;
                }
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
                let (dx, dy) = (whole(&mut grabbed.scroll_carry.0, dx), whole(&mut grabbed.scroll_carry.1, dy));
                if dx != 0.0 || dy != 0.0 {
                    send(InputEventKind::MouseScroll { delta_x: dx, delta_y: dy });
                }
            }
            LocalInput::Key { key: None, .. } => {
                grabbed.last_control_tap = None;
            }
            LocalInput::Key { key: Some(key), down } => {
                if grabbed.keys_here.contains(&key) {
                    // Its auto-repeat and its release both stay here.
                    if !down {
                        grabbed.keys_here.retain(|&held| held != key);
                    }
                    return false;
                }
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
    /// session up, or the pointer is being driven from another computer,
    /// and the backend should give input back.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn crossed(&self, edge: ScreenEdge, position: f64, at: Point) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.grab.is_some() {
            return true;
        }
        if driven_from_elsewhere() {
            tracing::debug!("not crossing at the {edge:?} edge: the pointer here is being driven from another computer");
            return false;
        }
        let Some(peer_id) = placed_with_session(&state, edge) else { return false };
        self.grab(&mut state, peer_id, edge, position, at);
        let taken = state.grab.is_some();
        drop(state);
        (self.on_change)();
        taken
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
        // is) gets the exact point; one that can only move it gets moves
        // that pin it to the arriving edge.
        let events = match target.screen_size {
            Some(size) => {
                let (x, y) = arrival(edge, position, Some(size));
                vec![InputEventKind::MouseMove { x, y }]
            }
            None => arrival_moves(edge, position, None),
        };
        for event in events {
            let _ = self.commands.send(EngineCommand::SendInputEvent { peer_crypto_id: peer_id.clone(), event });
        }
        let left_at = self.backend.screen().map_or(at, |bounds| point_on_edge(bounds, edge, position));
        self.backend.grabbed(left_at);
        tracing::info!("keyboard and mouse now go to '{}' (left through the {edge:?} edge at {position:.2})", target.name);
        state.grab = Some(Grab {
            peer_id,
            keyboard: KeyboardForwarder::new(KeyTranslator::new(cfg!(target_os = "macos"), target.platform)),
            buttons: Vec::new(),
            controls_held: Vec::new(),
            keys_here: std::mem::take(&mut state.keys_here),
            buttons_here: std::mem::take(&mut state.buttons_here),
            move_carry: (0.0, 0.0),
            scroll_carry: (0.0, 0.0),
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
        // Still down here from before: still this computer's.
        state.keys_here = grabbed.keys_here;
        state.buttons_here = grabbed.buttons_here;
        self.backend.released(at.unwrap_or(grabbed.left_at));
        state.released_at = Some(Instant::now());
        tracing::info!("keyboard and mouse are back on this computer");
    }
}

/// Keeps `down_now` to what's down: `item` added on a press, removed on a
/// release.
fn held<T: PartialEq>(down_now: &mut Vec<T>, item: T, down: bool) {
    if down {
        if !down_now.contains(&item) {
            down_now.push(item);
        }
    } else {
        down_now.retain(|other| *other != item);
    }
}

/// The whole pixels of `delta` plus what `carry` held back last time,
/// keeping the fraction left over in `carry` for the next one.
fn whole(carry: &mut f64, delta: f64) -> f64 {
    let total = *carry + delta;
    let whole = total.trunc();
    *carry = total - whole;
    whole
}

/// The peer placed at `edge` with a session up, if any.
fn placed_with_session(state: &State, edge: ScreenEdge) -> Option<String> {
    state.placements.iter().find(|(peer_id, placed)| **placed == edge && state.sessions.contains_key(*peer_id)).map(|(peer_id, _)| peer_id.clone())
}

/// The peer the cursor at `at` has just crossed onto, if any, and through
/// which edge: at a placed edge of `screen`, moving *outward* through it
/// (`delta`, the move that got it there), with a session up for the
/// computer placed there — and not within `RECROSS_HOLDOFF` of coming back,
/// nor while another computer is driving the pointer here (`driven`).
/// **Real bug fixed here**: the cursor came back one pixel inside the edge,
/// which the edge check counts as at it, so the first movement after
/// returning crossed straight over again — the mouse was trapped on the
/// other computer, and only the escape chord brought it home.
fn crossing(state: &State, screen: Option<Bounds>, at: Point, delta: (f64, f64), driven: bool) -> Option<(String, ScreenEdge)> {
    if driven || state.released_at.is_some_and(|released| released.elapsed() < RECROSS_HOLDOFF) {
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
/// normalized position `MouseMove` takes — `RETURN_INSET` pixels in when
/// the other screen's size is known, so a first move along the edge, or a
/// little back, isn't already the way home.
fn arrival(edge: ScreenEdge, position: f64, size: Option<ScreenSize>) -> (f64, f64) {
    let position = position.clamp(0.0, 1.0);
    let inset = |extent: u32| if extent > 1 { (RETURN_INSET / f64::from(extent - 1)).min(0.5) } else { 0.0 };
    let (inset_x, inset_y) = size.map_or((0.0, 0.0), |size| (inset(size.width), inset(size.height)));
    match edge {
        ScreenEdge::Left => (1.0 - inset_x, position),
        ScreenEdge::Right => (inset_x, position),
        ScreenEdge::Top => (position, 1.0 - inset_y),
        ScreenEdge::Bottom => (position, inset_y),
    }
}

/// The moves that put the other computer's pointer where `arrival` says
/// when it can only be moved, not placed: a pin to the bottom (or the
/// right), then a pin to the arriving edge — last, so a host that tracks
/// pins (Linux) takes that one as the edge the pointer is at — then along
/// the edge by `position` of `screen`, or nothing more with the size
/// unknown. Never through the top-left corner: a pin into it opens GNOME's
/// Activities overview (its hot corner). **Real bug fixed here**: the pins
/// went to the top first, so a cursor crossing onto a Linux computer from
/// its left landed in that corner and opened the overview.
fn arrival_moves(edge: ScreenEdge, position: f64, screen: Option<ScreenSize>) -> Vec<InputEventKind> {
    let position = position.clamp(0.0, 1.0);
    let (secondary, arriving) = match edge {
        ScreenEdge::Left => ((0.0, PIN), (PIN, 0.0)),
        ScreenEdge::Right => ((0.0, PIN), (-PIN, 0.0)),
        ScreenEdge::Top => ((PIN, 0.0), (0.0, PIN)),
        ScreenEdge::Bottom => ((PIN, 0.0), (0.0, -PIN)),
    };
    let mut moves = vec![
        InputEventKind::MouseMoveRelative { dx: secondary.0, dy: secondary.1 },
        InputEventKind::MouseMoveRelative { dx: arriving.0, dy: arriving.1 },
    ];
    if let Some(size) = screen {
        // From the bottom (or right) end of the edge.
        let along = match edge {
            ScreenEdge::Left | ScreenEdge::Right => InputEventKind::MouseMoveRelative { dx: 0.0, dy: -(1.0 - position) * f64::from(size.height) },
            ScreenEdge::Top | ScreenEdge::Bottom => InputEventKind::MouseMoveRelative { dx: -(1.0 - position) * f64::from(size.width), dy: 0.0 },
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
        assert_eq!(arrival(ScreenEdge::Right, 0.25, None), (0.0, 0.25));
        assert_eq!(arrival(ScreenEdge::Left, 0.75, None), (1.0, 0.75));
        assert_eq!(arrival(ScreenEdge::Top, 0.1, None), (0.1, 1.0));
        assert_eq!(arrival(ScreenEdge::Bottom, 0.9, None), (0.9, 0.0));
        assert_eq!(arrival(ScreenEdge::Right, 7.0, None), (0.0, 1.0), "clamped");
    }

    #[test]
    fn with_the_size_known_it_arrives_a_few_pixels_in() {
        let size = Some(ScreenSize { width: 1441, height: 901 });
        let (x, y) = arrival(ScreenEdge::Right, 0.5, size);
        assert_eq!((x * 1440.0, y), (4.0, 0.5), "4 px from the left edge");
        let (x, _) = arrival(ScreenEdge::Left, 0.5, size);
        assert_eq!(((1.0 - x) * 1440.0).round(), 4.0, "4 px from the right edge");
        let (_, y) = arrival(ScreenEdge::Bottom, 0.5, size);
        assert_eq!(y * 900.0, 4.0);
        // ...which a host's own edge check doesn't count as at the edge.
        let other = Bounds { x: 0.0, y: 0.0, width: 1441.0, height: 901.0 };
        let (x, y) = arrival(ScreenEdge::Right, 0.5, size);
        assert_eq!(edge_of(Point { x: x * 1440.0, y: y * 900.0 }, other), None);
    }

    #[test]
    fn arrival_by_moves_pins_to_the_edge_last_then_goes_along_it() {
        let size = Some(ScreenSize { width: 1920, height: 1080 });
        // Leaving through the right edge: arrive at the other's left.
        let moves = arrival_moves(ScreenEdge::Right, 0.25, size);
        assert!(matches!(moves[0], InputEventKind::MouseMoveRelative { dx, dy } if dx == 0.0 && dy >= PIN), "the bottom first");
        assert!(matches!(moves[1], InputEventKind::MouseMoveRelative { dx, dy } if dx <= -PIN && dy == 0.0), "then the left edge");
        assert!(matches!(moves[2], InputEventKind::MouseMoveRelative { dx, dy } if dx == 0.0 && dy == -810.0), "a quarter of the way down");
        let moves = arrival_moves(ScreenEdge::Top, 0.25, size);
        assert!(matches!(moves[0], InputEventKind::MouseMoveRelative { dx, dy } if dx >= PIN && dy == 0.0), "the right first");
        assert!(matches!(moves[1], InputEventKind::MouseMoveRelative { dx, dy } if dx == 0.0 && dy >= PIN), "then the bottom edge");
        assert!(matches!(moves[2], InputEventKind::MouseMoveRelative { dx, dy } if dx == -1440.0 && dy == 0.0));
        assert_eq!(arrival_moves(ScreenEdge::Left, 0.5, None).len(), 2, "no size: the pins alone");
        // None of them heads for the top-left corner.
        for edge in [ScreenEdge::Left, ScreenEdge::Right, ScreenEdge::Top, ScreenEdge::Bottom] {
            let InputEventKind::MouseMoveRelative { dx, dy } = arrival_moves(edge, 0.5, None)[0] else { unreachable!() };
            assert!(dx >= 0.0 && dy >= 0.0, "{edge:?}");
        }
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

    fn target(session_id: &str) -> Target {
        Target { session_id: session_id.into(), name: "PC".into(), platform: Platform::Windows, screen_size: None }
    }

    #[test]
    fn crossing_needs_a_placed_edge_a_session_and_an_outward_move() {
        let mut state = State::default();
        state.placements.insert("pc".into(), ScreenEdge::Right);
        let right = Point { x: 1919.0, y: 10.0 };
        let outward = (3.0, 0.0);
        assert_eq!(crossing(&state, Some(SCREEN), right, outward, false), None, "placed, but no session yet");
        state.sessions.insert("pc".into(), target("s1"));
        assert_eq!(crossing(&state, Some(SCREEN), right, outward, false), Some(("pc".to_string(), ScreenEdge::Right)));
        assert_eq!(crossing(&state, Some(SCREEN), right, outward, true), None, "never while driven from another computer");
        assert_eq!(crossing(&state, Some(SCREEN), right, (-3.0, 0.0), false), None, "moving away from the edge isn't crossing");
        assert_eq!(crossing(&state, Some(SCREEN), right, (0.0, 5.0), false), None, "nor moving along it");
        assert_eq!(crossing(&state, Some(SCREEN), Point { x: 960.0, y: 540.0 }, outward, false), None);
        assert_eq!(crossing(&state, Some(SCREEN), Point { x: 0.0, y: 10.0 }, (-3.0, 0.0), false), None, "nothing placed there");
        assert_eq!(crossing(&state, None, right, outward, false), None, "no screen known");
        state.released_at = Some(Instant::now());
        assert_eq!(crossing(&state, Some(SCREEN), right, outward, false), None, "just came back: not again yet");
    }

    #[test]
    fn injected_input_counts_as_driven_for_a_moment() {
        note_remote_input();
        assert!(driven_from_elsewhere());
        REMOTE_INPUT_AT.store(clock().saturating_sub(REMOTE_INPUT_QUIET.as_millis() as u64 + 1), Ordering::Relaxed);
        assert!(!driven_from_elsewhere(), "quiet long enough");
    }

    #[test]
    fn fractions_of_a_pixel_add_up_instead_of_vanishing() {
        let mut carry = 0.0;
        let sent: Vec<f64> = [0.4, 0.4, 0.4, -0.3, 2.75].iter().map(|&d| whole(&mut carry, d)).collect();
        assert_eq!(sent, vec![0.0, 0.0, 1.0, 0.0, 2.0]);
        assert!((carry - 0.65).abs() < 1e-9);
        let mut carry = 0.0;
        assert_eq!(whole(&mut carry, -1.5), -1.0);
        assert_eq!(whole(&mut carry, -0.5), -1.0, "negative fractions carry too");
    }

    #[test]
    fn held_tracks_whats_down() {
        let mut keys = Vec::new();
        held(&mut keys, KeyCode::ShiftLeft, true);
        held(&mut keys, KeyCode::ShiftLeft, true);
        held(&mut keys, KeyCode::KeyA, true);
        assert_eq!(keys, vec![KeyCode::ShiftLeft, KeyCode::KeyA], "a repeat isn't a second key");
        held(&mut keys, KeyCode::ShiftLeft, false);
        assert_eq!(keys, vec![KeyCode::KeyA]);
    }
}

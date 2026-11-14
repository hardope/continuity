//! Keyboard-and-mouse sharing, the controlling side on macOS: this Mac's own
//! mouse and keyboard, forwarded to another computer while the cursor is
//! "over there" — the way one keyboard and mouse drive two computers side
//! by side, with no picture of the other screen. The other computer injects
//! what it's sent exactly as it does for the remote-control viewer; the
//! session is one without the screen (`RemoteControlRequest { screen:
//! false }`, see `continuity-daemon`).
//!
//! **Placement.** The Continuity window puts a paired computer at one edge
//! of this screen (`ControlState::set_shared_edge`). While it's connected,
//! a session is kept open with it (requested on connection, with the other
//! computer's usual consent), idle until the cursor crosses that edge.
//!
//! **Crossing.** A session-level event tap sees every mouse move. When the
//! cursor reaches a placed edge, the other computer's pointer is put at the
//! matching point of its opposite edge (one absolute `MouseMove`), this
//! Mac's cursor is hidden and unlinked from the mouse, and from then on the
//! tap swallows every mouse and keyboard event and forwards it: moves as
//! deltas (`MouseMoveRelative`), buttons, the scroll wheel, keys through the
//! viewer's own layout translation (Command and Control trade places
//! towards a PC). When the other computer reports its pointer against the
//! edge facing this screen (`PointerReachedEdge`), the cursor is put back at
//! the matching point of this screen's edge, shown, and relinked.
//!
//! **Getting back no matter what.** Double-tapping Control releases the
//! mouse and keyboard on this Mac alone, before anything is sent, so it
//! works when the other computer has frozen or the network has gone. A
//! session ending, the peer disconnecting, and the tap being disabled by
//! the system all release too, and whatever was held down over there is let
//! go of first.
//!
//! An active tap on keyboard events needs the Accessibility permission the
//! remote-control host already asks for. Only this Mac's main display is
//! watched; a cursor on a second display never crosses onto another
//! computer.

use crate::remote_control_mac::edge_of;
use crate::remote_viewer::{key_from_mac_code, KeyTranslator, KeyboardForwarder};
use continuity_daemon::{EngineCommand, RemoteControlRole, SyncEvent};
use continuity_proto::{InputEventKind, MouseButton as ProtoMouseButton, Platform, ScreenEdge, ScreenSize};
use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRetained, CFRunLoop, CGPoint, CGRect};
use tao::keyboard::KeyCode;
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGDisplayBounds, CGDisplayHideCursor, CGDisplayShowCursor, CGEvent, CGEventField,
    CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMainDisplayID,
    CGWarpMouseCursorPosition,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Two Control presses this close together, with nothing else in between,
/// bring the mouse and keyboard back to this Mac.
const ESCAPE_DOUBLE_TAP: Duration = Duration::from_millis(400);

/// Told when the sharing state changes (for the window's status).
pub type OnChange = Arc<dyn Fn() + Send + Sync>;

/// What's known about a computer placed at an edge, once its session is up.
#[derive(Clone)]
struct Target {
    name: String,
    platform: Platform,
    /// The other screen's size, when it said — without it the pointer is
    /// put at an edge with moves longer than any screen (see `arrival`).
    screen_size: Option<ScreenSize>,
}

/// The mouse and keyboard are over there.
struct Grab {
    peer_id: String,
    keyboard: KeyboardForwarder,
    buttons: Vec<ProtoMouseButton>,
    /// Where the cursor left this screen, to come back to on escape.
    left_at: CGPoint,
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
    /// Whether the event tap is running (made once there's something to
    /// watch for, and the permission for it).
    tap_running: bool,
}

struct Shared {
    commands: UnboundedSender<EngineCommand>,
    state: Mutex<State>,
    on_change: OnChange,
}

pub struct InputShare {
    shared: Arc<Shared>,
}

thread_local! {
    /// The tap thread's own port, to re-enable it when the system disables
    /// it (a callback that took too long).
    static TAP: RefCell<Option<CFRetained<CFMachPort>>> = const { RefCell::new(None) };
}

impl InputShare {
    pub fn new(commands: UnboundedSender<EngineCommand>, on_change: OnChange) -> Self {
        Self { shared: Arc::new(Shared { commands, state: Mutex::new(State::default()), on_change }) }
    }

    /// The peer this Mac's mouse and keyboard are over at right now.
    pub fn sharing_with(&self) -> Option<String> {
        self.shared.state.lock().unwrap().grab.as_ref().map(|g| g.peer_id.clone())
    }

    /// Puts `placements` into effect: a computer newly placed and connected
    /// gets a session asked for; one no longer placed has its session
    /// ended, and the mouse back if it was over there.
    pub fn set_placements(&self, placements: HashMap<String, ScreenEdge>) {
        let (request, end) = {
            let mut state = self.shared.state.lock().unwrap();
            let request: Vec<String> = placements
                .keys()
                .filter(|id| !state.placements.contains_key(*id) && state.platforms.contains_key(*id) && !state.sessions.contains_key(*id))
                .cloned()
                .collect();
            let end: Vec<String> = state.sessions.keys().filter(|id| !placements.contains_key(*id)).cloned().collect();
            if state.grab.as_ref().is_some_and(|g| end.contains(&g.peer_id)) {
                release(&mut state, &self.shared.commands, None);
            }
            state.placements = placements;
            (request, end)
        };
        for peer_id in request {
            let _ = self.shared.commands.send(EngineCommand::RequestRemoteControl { peer_crypto_id: peer_id, screen: false });
        }
        for peer_id in end {
            let _ = self.shared.commands.send(EngineCommand::EndRemoteControlSession { peer_crypto_id: peer_id });
        }
        self.ensure_tap();
        (self.shared.on_change)();
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
            }
            SyncEvent::Disconnected { peer_id, .. } | SyncEvent::WasRevoked { peer_id, .. } | SyncEvent::RevokedByPeer { peer_id, .. } => {
                state.platforms.remove(peer_id);
                if state.sessions.remove(peer_id).is_some() {
                    if state.grab.as_ref().is_some_and(|g| &g.peer_id == peer_id) {
                        release(&mut state, &shared.commands, None);
                    }
                    drop(state);
                    (shared.on_change)();
                }
            }
            SyncEvent::WasReset => {
                state.platforms.clear();
                state.sessions.clear();
                if state.grab.is_some() {
                    release(&mut state, &shared.commands, None);
                }
            }
            SyncEvent::RemoteControlSessionStarted { peer_id, peer_name, role, screen: false, screen_size, .. }
                if *role == RemoteControlRole::Controlling =>
            {
                let platform = state.platforms.get(peer_id).copied().unwrap_or(Platform::MacOs);
                state.sessions.insert(peer_id.clone(), Target { name: peer_name.clone(), platform, screen_size: *screen_size });
                tracing::info!("sharing the keyboard and mouse with '{peer_name}' when the cursor crosses to it");
            }
            SyncEvent::RemoteControlSessionEnded { peer_id, .. } => {
                if state.sessions.remove(peer_id).is_some() {
                    if state.grab.as_ref().is_some_and(|g| &g.peer_id == peer_id) {
                        release(&mut state, &shared.commands, None);
                    }
                    drop(state);
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
                    let bounds = main_bounds();
                    release(&mut state, &shared.commands, Some(point_on_edge(bounds, placed, *position)));
                    drop(state);
                    (shared.on_change)();
                }
            }
            _ => {}
        }
    }

    /// Starts the event tap if it isn't running and there's a placement to
    /// watch for. Needs Accessibility; without it this logs and tries again
    /// the next time a placement changes.
    fn ensure_tap(&self) {
        {
            let state = self.shared.state.lock().unwrap();
            if state.tap_running || state.placements.is_empty() {
                return;
            }
        }
        crate::media_mac::ensure_accessibility_trust();
        let shared = self.shared.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("input-share".to_string()).spawn(move || run_tap(shared, started_tx));
        if let Err(e) = spawned {
            tracing::warn!("couldn't start the input-sharing thread: {e}");
            return;
        }
        match started_rx.recv() {
            Ok(true) => self.shared.state.lock().unwrap().tap_running = true,
            _ => tracing::warn!(
                "couldn't watch the mouse for keyboard-and-mouse sharing — Continuity needs the Accessibility permission (System Settings > Privacy & Security > Accessibility)"
            ),
        }
    }
}

/// The tap thread: creates the tap and runs its run loop for as long as the
/// process lives.
fn run_tap(shared: Arc<Shared>, started: std::sync::mpsc::Sender<bool>) {
    let types = [
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ];
    let mask: CGEventMask = types.iter().fold(0, |mask, kind| mask | (1u64 << kind.0));
    let user_info = Arc::into_raw(shared) as *mut c_void;
    // SAFETY: `user_info` is an `Arc<Shared>` kept alive for the tap's whole
    // life (never released: the tap runs until the process ends), and the
    // callback only reads it through `Arc::increment_strong_count`-free
    // borrows while the tap exists.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(tap_callback),
            user_info,
        )
    };
    let Some(port) = port else {
        let _ = started.send(false);
        return;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        let _ = started.send(false);
        return;
    };
    let Some(run_loop) = CFRunLoop::current() else {
        let _ = started.send(false);
        return;
    };
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    TAP.with(|tap| *tap.borrow_mut() = Some(port));
    let _ = started.send(true);
    CFRunLoop::run();
}

/// Every mouse and keyboard event on this Mac passes through here. Returns
/// the event to let it through, null to swallow it.
unsafe extern "C-unwind" fn tap_callback(_proxy: CGEventTapProxy, kind: CGEventType, event: NonNull<CGEvent>, user_info: *mut c_void) -> *mut CGEvent {
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        // The system switched the tap off (a callback took too long, or the
        // user asked); back on, and nothing swallowed meanwhile.
        TAP.with(|tap| {
            if let Some(port) = tap.borrow().as_ref() {
                CGEvent::tap_enable(port, true);
            }
        });
        return event.as_ptr();
    }
    // SAFETY: see `run_tap`.
    let shared: &Shared = unsafe { &*(user_info as *const Shared) };
    let event_ref: &CGEvent = unsafe { event.as_ref() };
    let swallow = handle_event(shared, kind, event_ref);
    if swallow {
        std::ptr::null_mut()
    } else {
        event.as_ptr()
    }
}

/// Whether to swallow `event`: everything while the mouse is over there,
/// nothing while it's here.
fn handle_event(shared: &Shared, kind: CGEventType, event: &CGEvent) -> bool {
    let mut state = shared.state.lock().unwrap();
    if state.grab.is_none() {
        if matches!(kind, CGEventType::MouseMoved | CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged | CGEventType::OtherMouseDragged) {
            let location = CGEvent::location(Some(event));
            if let Some(peer_id) = crossing(&state, location) {
                grab(&mut state, &shared.commands, peer_id, location);
                drop(state);
                (shared.on_change)();
            }
        }
        return false;
    }
    let commands = &shared.commands;
    let Some(grabbed) = state.grab.as_mut() else { return false };
    let peer_id = grabbed.peer_id.clone();
    let send = |event: InputEventKind| {
        let _ = commands.send(EngineCommand::SendInputEvent { peer_crypto_id: peer_id.clone(), event });
    };
    match kind {
        CGEventType::MouseMoved | CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged | CGEventType::OtherMouseDragged => {
            let dx = CGEvent::integer_value_field(Some(event), CGEventField::MouseEventDeltaX) as f64;
            let dy = CGEvent::integer_value_field(Some(event), CGEventField::MouseEventDeltaY) as f64;
            if dx != 0.0 || dy != 0.0 {
                send(InputEventKind::MouseMoveRelative { dx, dy });
            }
        }
        CGEventType::LeftMouseDown | CGEventType::LeftMouseUp => button(grabbed, &send, ProtoMouseButton::Left, kind == CGEventType::LeftMouseDown),
        CGEventType::RightMouseDown | CGEventType::RightMouseUp => button(grabbed, &send, ProtoMouseButton::Right, kind == CGEventType::RightMouseDown),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => button(grabbed, &send, ProtoMouseButton::Middle, kind == CGEventType::OtherMouseDown),
        CGEventType::ScrollWheel => {
            // Point deltas: pixels, as the viewer sends them too.
            let dy = CGEvent::integer_value_field(Some(event), CGEventField::ScrollWheelEventPointDeltaAxis1) as f64;
            let dx = CGEvent::integer_value_field(Some(event), CGEventField::ScrollWheelEventPointDeltaAxis2) as f64;
            if dx != 0.0 || dy != 0.0 {
                send(InputEventKind::MouseScroll { delta_x: dx, delta_y: dy });
            }
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            grabbed.last_control_tap = None;
            let code = CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
            if let Some(key) = u16::try_from(code).ok().and_then(key_from_mac_code) {
                if let Some(forwarded) = grabbed.keyboard.key(key, kind == CGEventType::KeyDown) {
                    send(forwarded);
                }
            }
        }
        CGEventType::FlagsChanged => {
            let code = CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
            let flags = CGEvent::flags(Some(event));
            let Some((key, pressed)) = modifier_change(code, flags) else { return true };
            // The escape chord: Control twice in a row.
            if pressed && matches!(key, KeyCode::ControlLeft | KeyCode::ControlRight) {
                let now = Instant::now();
                if grabbed.last_control_tap.is_some_and(|last| now.duration_since(last) <= ESCAPE_DOUBLE_TAP) {
                    let back_to = grabbed.left_at;
                    release(&mut state, commands, Some(back_to));
                    drop(state);
                    (shared.on_change)();
                    return true;
                }
                grabbed.last_control_tap = Some(now);
            } else if pressed {
                grabbed.last_control_tap = None;
            }
            if let Some(forwarded) = grabbed.keyboard.key(key, pressed) {
                send(forwarded);
            }
        }
        _ => {}
    }
    true
}

fn button(grabbed: &mut Grab, send: &dyn Fn(InputEventKind), button: ProtoMouseButton, down: bool) {
    if down {
        if !grabbed.buttons.contains(&button) {
            grabbed.buttons.push(button);
        }
    } else {
        grabbed.buttons.retain(|&held| held != button);
    }
    send(InputEventKind::MouseButton { button, down });
}

// Carbon's `kVK_*` codes for the modifier keys, and the device-specific
// flag bits (IOKit's `NX_DEVICE*KEYMASK`) that say which of a pair is down.
const MODIFIERS: [(u16, KeyCode, u64); 9] = [
    (0x38, KeyCode::ShiftLeft, 0x0002),
    (0x3C, KeyCode::ShiftRight, 0x0004),
    (0x3B, KeyCode::ControlLeft, 0x0001),
    (0x3E, KeyCode::ControlRight, 0x2000),
    (0x3A, KeyCode::AltLeft, 0x0020),
    (0x3D, KeyCode::AltRight, 0x0040),
    (0x37, KeyCode::SuperLeft, 0x0008),
    (0x36, KeyCode::SuperRight, 0x0010),
    (0x39, KeyCode::CapsLock, 0x0001_0000),
];

/// Which modifier key a "flags changed" event is about, and whether it went
/// down — from the device-specific bit for that key, so letting go of one
/// Shift while the other is held reads as that one's release.
fn modifier_change(code: i64, flags: CGEventFlags) -> Option<(KeyCode, bool)> {
    let code = u16::try_from(code).ok()?;
    let &(_, key, bit) = MODIFIERS.iter().find(|(vk, _, _)| *vk == code)?;
    Some((key, flags.0 & bit != 0))
}

/// The peer the cursor at `location` has just crossed onto, if any: at a
/// placed edge of the main display, with a session up for the computer
/// placed there.
fn crossing(state: &State, location: CGPoint) -> Option<String> {
    let (edge, _) = edge_of(location, main_bounds())?;
    state.placements.iter().find(|(peer_id, placed)| **placed == edge && state.sessions.contains_key(*peer_id)).map(|(peer_id, _)| peer_id.clone())
}

fn main_bounds() -> CGRect {
    CGDisplayBounds(CGMainDisplayID())
}

/// The point `position` (0.0..=1.0) of the way along `edge` of `bounds`,
/// one pixel in.
fn point_on_edge(bounds: CGRect, edge: ScreenEdge, position: f64) -> CGPoint {
    let (left, top) = (bounds.origin.x, bounds.origin.y);
    let (right, bottom) = (left + bounds.size.width - 1.0, top + bounds.size.height - 1.0);
    let position = position.clamp(0.0, 1.0);
    match edge {
        ScreenEdge::Left => CGPoint { x: left + 1.0, y: top + position * (bottom - top) },
        ScreenEdge::Right => CGPoint { x: right - 1.0, y: top + position * (bottom - top) },
        ScreenEdge::Top => CGPoint { x: left + position * (right - left), y: top + 1.0 },
        ScreenEdge::Bottom => CGPoint { x: left + position * (right - left), y: bottom - 1.0 },
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

/// A move at least this long pins the pointer to an edge wherever it was
/// (see `arrival_moves`); the Linux host recognizes it as such.
const PIN: f64 = 1_000_000.0;

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

/// Takes the mouse and keyboard over to `peer_id`.
fn grab(state: &mut State, commands: &UnboundedSender<EngineCommand>, peer_id: String, location: CGPoint) {
    let Some(target) = state.sessions.get(&peer_id).cloned() else { return };
    let placed = state.placements[&peer_id];
    let bounds = main_bounds();
    let (_, position) = edge_of(location, bounds).unwrap_or((placed, 0.5));
    // A computer that can place its pointer (it said how big its screen is)
    // gets the exact point; one that can only move it (Linux, input-only)
    // gets moves that pin it to the arriving edge.
    let events = match target.screen_size {
        Some(_) => {
            let (x, y) = arrival(placed, position);
            vec![InputEventKind::MouseMove { x, y }]
        }
        None => arrival_moves(placed, position, None),
    };
    for event in events {
        let _ = commands.send(EngineCommand::SendInputEvent { peer_crypto_id: peer_id.clone(), event });
    }
    set_cursor_in_background();
    // The cursor stays put (and hidden) while the mouse drives the other
    // computer; the events still arrive here, with their deltas.
    CGAssociateMouseAndMouseCursorPosition(false);
    CGDisplayHideCursor(CGMainDisplayID());
    tracing::info!("keyboard and mouse now go to '{}'", target.name);
    state.grab = Some(Grab {
        peer_id,
        keyboard: KeyboardForwarder::new(KeyTranslator::new(true, target.platform)),
        buttons: Vec::new(),
        left_at: location,
        last_control_tap: None,
    });
}

/// Brings the mouse and keyboard back, letting go of whatever was held down
/// over there first. `at`: where the cursor appears on this screen (where
/// it left, for the escape chord).
fn release(state: &mut State, commands: &UnboundedSender<EngineCommand>, at: Option<CGPoint>) {
    let Some(mut grabbed) = state.grab.take() else { return };
    for &button in grabbed.buttons.iter().rev() {
        let _ = commands.send(EngineCommand::SendInputEvent { peer_crypto_id: grabbed.peer_id.clone(), event: InputEventKind::MouseButton { button, down: false } });
    }
    for event in grabbed.keyboard.release_all() {
        let _ = commands.send(EngineCommand::SendInputEvent { peer_crypto_id: grabbed.peer_id.clone(), event });
    }
    CGAssociateMouseAndMouseCursorPosition(true);
    CGWarpMouseCursorPosition(at.unwrap_or(grabbed.left_at));
    CGDisplayShowCursor(CGMainDisplayID());
    tracing::info!("keyboard and mouse are back on this Mac");
}

/// Lets this process hide the cursor while another app is in front —
/// `CGDisplayHideCursor` otherwise only works for the frontmost app, and a
/// menu-bar app never is. The private `CGSSetConnectionProperty`, looked up
/// at runtime like MediaRemote; without it the cursor stays visible, parked
/// at the edge, which is only cosmetic.
fn set_cursor_in_background() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        type MainConnectionFn = unsafe extern "C" fn() -> i32;
        type SetPropertyFn = unsafe extern "C" fn(i32, i32, core_foundation::string::CFStringRef, core_foundation::base::CFTypeRef) -> i32;
        let handle = libc::dlopen(c"/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices".as_ptr(), libc::RTLD_LAZY);
        if handle.is_null() {
            return;
        }
        let connection = libc::dlsym(handle, c"CGSMainConnectionID".as_ptr());
        let set_property = libc::dlsym(handle, c"CGSSetConnectionProperty".as_ptr());
        if connection.is_null() || set_property.is_null() {
            tracing::debug!("CGSSetConnectionProperty isn't available; the cursor may stay visible while over there");
            return;
        }
        let connection: MainConnectionFn = std::mem::transmute(connection);
        let set_property: SetPropertyFn = std::mem::transmute(set_property);
        use core_foundation::base::TCFType;
        let key = core_foundation::string::CFString::new("SetsCursorInBackground");
        let value = core_foundation::boolean::CFBoolean::true_value();
        let id = connection();
        set_property(id, id, key.as_concrete_TypeRef(), value.as_CFTypeRef());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CGSize;

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
    fn the_point_on_an_edge_is_one_pixel_in() {
        let bounds = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: 1920.0, height: 1080.0 } };
        let p = point_on_edge(bounds, ScreenEdge::Right, 0.5);
        assert_eq!((p.x, p.y), (1918.0, 539.5));
        let p = point_on_edge(bounds, ScreenEdge::Top, 0.0);
        assert_eq!((p.x, p.y), (0.0, 1.0));
        // Round trip with `edge_of`: a point put on an edge is at that edge.
        for edge in [ScreenEdge::Left, ScreenEdge::Right, ScreenEdge::Top, ScreenEdge::Bottom] {
            let p = point_on_edge(bounds, edge, 0.3);
            assert_eq!(edge_of(p, bounds).map(|(e, _)| e), Some(edge), "{edge:?}");
        }
    }

    #[test]
    fn modifier_changes_say_which_key_and_whether_down() {
        let left_shift_down = CGEventFlags(CGEventFlags::MaskShift.0 | 0x0002);
        assert_eq!(modifier_change(0x38, left_shift_down), Some((KeyCode::ShiftLeft, true)));
        // Right Shift still down, left let go: the left's release.
        let right_only = CGEventFlags(CGEventFlags::MaskShift.0 | 0x0004);
        assert_eq!(modifier_change(0x38, right_only), Some((KeyCode::ShiftLeft, false)));
        assert_eq!(modifier_change(0x3C, right_only), Some((KeyCode::ShiftRight, true)));
        assert_eq!(modifier_change(0x00, right_only), None, "not a modifier");
    }

    #[test]
    fn crossing_needs_a_placed_edge_with_a_session() {
        let mut state = State::default();
        state.placements.insert("pc".into(), ScreenEdge::Right);
        let bounds = main_bounds();
        let right = CGPoint { x: bounds.origin.x + bounds.size.width - 1.0, y: bounds.origin.y + 10.0 };
        assert_eq!(crossing(&state, right), None, "placed, but no session yet");
        state.sessions.insert("pc".into(), Target { name: "PC".into(), platform: Platform::Windows, screen_size: None });
        assert_eq!(crossing(&state, right), Some("pc".to_string()));
        let middle = CGPoint { x: bounds.origin.x + bounds.size.width / 2.0, y: bounds.origin.y + bounds.size.height / 2.0 };
        assert_eq!(crossing(&state, middle), None);
        let left = CGPoint { x: bounds.origin.x, y: bounds.origin.y + 10.0 };
        assert_eq!(crossing(&state, left), None, "nothing placed there");
    }
}

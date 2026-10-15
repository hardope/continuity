//! The desktop half of remote control's *controlling* side: a real
//! window showing the peer's live screen, with local mouse/keyboard
//! forwarded back as input events. Exists on every desktop platform
//! (macOS/Windows/Linux can all initiate and view a session), unlike
//! `remote_control_mac.rs`/`remote_control_windows.rs`/
//! `remote_control_linux.rs`, which are the OS-specific *controlled*-side
//! capture/injection code.
//!
//! Rendering is plain `softbuffer` (a CPU pixel buffer blitted straight
//! into the window, no GPU API setup) rather than anything wgpu-based —
//! this is a JPEG decoded and redrawn a handful of times a second, nowhere
//! near demanding enough to justify a real graphics pipeline.
//!
//! **Why frames never touch the UI thread until they're ready to blit.**
//! The first version decoded every incoming JPEG, and scaled it pixel by
//! pixel, on the tao event loop's own thread, with each frame queued there
//! as a `SyncEvent::ScreenFrameReceived` through the (unbounded)
//! `EventLoopProxy`. Real testing (a Mac controlling a Mac) froze the
//! controlling app solid the moment the peer accepted. A Retina screen's
//! JPEG takes ~150ms just to decode in the size-optimized release build —
//! longer than the ~100ms between frames — so the proxy's queue only ever
//! grew. And on macOS tao drains that queue with `try_iter()` in one go
//! before returning to the run loop, so while frames kept arriving faster
//! than they were handled the main thread never got back to the run loop
//! at all: no menu, no window events, no redraw, ever again. Now:
//!
//! - The engine thread hands each frame to a [`FrameRouter`] instead of the
//!   proxy (see `start_engine_thread` in main.rs), which drops it into the
//!   open viewer's single latest-frame slot ([`FrameInbox`]) — a newer frame
//!   replaces an older one nobody has decoded yet, so nothing ever queues.
//! - A dedicated decoder thread per viewer decodes the newest JPEG and
//!   resizes it to the window's current size into a ready-to-blit 0RGB
//!   buffer ([`FrameOutput`]), then wakes the UI thread with one
//!   lightweight `AppEvent` — never a second one while the first is still
//!   in flight.
//! - The UI thread only copies that buffer into the window and presents.
//!
//! Keyboard input maps `tao`'s layout-independent `physical_key` to the
//! *peer's* platform-native key codes (`InputEventKind` carries raw
//! platform codes — the receiving side's platform, not this one, decides
//! what a code means), covering the whole US-layout keyboard: letters,
//! digits, punctuation, editing and navigation keys, F1–F12, both sides'
//! modifiers and the keypad. Between a Mac and a PC, Command and Control
//! trade places (see [`KeyTranslator`]), so the shortcut a person's hands
//! already know does the same thing on the other computer.
//!
//! **Unverified beyond compiling and unit tests** — there's no way in
//! this environment to actually open a real window, receive a real frame,
//! and click on it.

use continuity_daemon::EngineCommand;
use continuity_proto::{InputEventKind, MouseButton as ProtoMouseButton, Platform};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{ElementState, MouseButton as TaoMouseButton, MouseScrollDelta, WindowEvent};
use tao::event_loop::EventLoopWindowTarget;
use tao::keyboard::KeyCode;
use tao::window::{Window, WindowBuilder, WindowId};
use tokio::sync::mpsc::UnboundedSender;

pub enum ViewerAction {
    None,
    Closed,
}

/// Wakes the UI thread once a decoded frame is waiting (in main.rs: sends
/// `AppEvent::ViewerFrameReady` through the event loop's proxy). Returns
/// `false` once the event loop is gone, which ends the decoder thread.
pub type Wake = Box<dyn Fn() -> bool + Send>;

/// Who a viewer window is for — the session `RemoteControlSessionStarted`
/// announced.
pub struct ViewerPeer<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub session_id: &'a str,
    pub platform: Platform,
}

pub struct RemoteViewer {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    peer_id: String,
    commands: UnboundedSender<EngineCommand>,
    keyboard: KeyboardForwarder,
    /// Mouse buttons pressed on the peer and not yet released — released
    /// on focus loss and close, like `keyboard`'s held keys, so a drag
    /// interrupted by Cmd-Tab/Alt-Tab doesn't stay "held" over there.
    held_buttons: Vec<ProtoMouseButton>,
    /// The most recent decoded frame, already sized for the window by the
    /// decoder thread — `None` until the first one arrives, so `redraw`
    /// has something to show a placeholder for instead of drawing garbage.
    frame: Option<ReadyFrame>,
    /// Where `frame` was last actually drawn within the window
    /// (letterboxed to preserve aspect ratio) — `handle_window_event`
    /// needs this to map a click's window-pixel position back to a
    /// normalized position *on the frame*, not the whole window. Always
    /// the rect of what's really on screen, even mid-resize when the frame
    /// on screen was sized for the window's previous size.
    placement: Option<Placement>,
    /// The pointer's latest position over the window, so a click can first
    /// send the exact position the throttle may have skipped.
    cursor: Option<PhysicalPosition<f64>>,
    last_move_sent: Instant,
    last_sent_position: Option<(f64, f64)>,
    /// Set once `request_redraw` has been called and cleared at the top
    /// of `redraw` — without this, a frame arriving every ~100ms calls
    /// `request_redraw` every time regardless of whether the *previous*
    /// request has actually been serviced yet. Confirmed for real this
    /// isn't just a performance nicety: tao's Linux `request_redraw`
    /// just pushes onto an unbounded channel with no deduping of its
    /// own, so an unthrottled flood of ~10/sec queued up indefinitely
    /// against a window stuck in an abnormal, never-mapped state (see
    /// `rendered_once` below) — live testing on a real GNOME 50/Wayland
    /// session showed this preceding an actual Mutter segfault
    /// (`clutter_sprite_invalidate_cursor`, confirmed via the saved
    /// apport crash report's backtrace) that took down the whole
    /// desktop session.
    redraw_pending: bool,
    /// Whether `redraw` has ever successfully reached `buffer.present()`
    /// — becomes the basis for `is_stuck`'s watchdog below.
    rendered_once: bool,
    opened_at: Instant,
    /// Declared last so it's dropped last: its `Drop` stops the decoder
    /// thread after `RemoteViewer`'s own `Drop` has released held input.
    pipeline: Pipeline,
}

/// Throttles outgoing `MouseMove` — a real desktop mouse reports position
/// far more often than the Android touch-drag path this protocol
/// message was originally sized around, and sending every single OS
/// mouse-move event would needlessly flood the connection that also
/// carries clipboard/ping traffic. ~120Hz is well past what's
/// perceptible as latency but a fraction of raw OS mouse-move rate.
const MOVE_THROTTLE: Duration = Duration::from_millis(8);

/// If the window hasn't rendered a single real frame within this long,
/// something is fundamentally broken (confirmed on Linux: the window
/// never gets properly presented by the compositor at all when created
/// from a background event rather than direct user input) — better to
/// give up and end the session cleanly than leave a permanently-blank,
/// still-input-receiving window open indefinitely, which is the state
/// that preceded a real GNOME Shell crash during testing.
const RENDER_WATCHDOG: Duration = Duration::from_secs(5);

/// 0x00RRGGBB per softbuffer's pixel format — a dark neutral fill so the
/// letterbox bars read as intentional, not a rendering bug.
const BACKGROUND: u32 = 0x001c1c1e;

impl RemoteViewer {
    pub fn open<T: 'static>(
        target: &EventLoopWindowTarget<T>,
        peer: ViewerPeer<'_>,
        router: &Arc<FrameRouter>,
        wake: Wake,
        commands: UnboundedSender<EngineCommand>,
    ) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_title(format!("Continuity — Controlling '{}'", peer.name))
            .with_inner_size(LogicalSize::new(1024.0, 640.0))
            .build(target)?;
        let window = Rc::new(window);
        // Best-effort nudge for the same underlying issue `is_stuck`
        // guards against: this window is created from a background
        // engine event, not directly in response to a user click, which
        // some Wayland compositors can refuse to ever properly present
        // at all (confirmed on GNOME 50/Mutter — see `RENDER_WATCHDOG`).
        // Explicit focus doesn't reliably fix that, but it's free.
        window.set_visible(true);
        // On macOS a window only gets keyboard input while its app is the
        // active one, and a menu-bar app usually isn't: using its menu
        // doesn't activate it, so by the time the peer accepts, whatever
        // the person clicked last is. Without activating, every key went
        // to that other app.
        #[cfg(target_os = "macos")]
        activate_for_keyboard(&window);
        window.set_focus();
        let window_id = window.id();
        let context = softbuffer::Context::new(window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let surface = softbuffer::Surface::new(&context, window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;

        let size = window.inner_size();
        let inbox = Arc::new(FrameInbox::new((size.width, size.height)));
        let output = Arc::new(FrameOutput::default());
        {
            let (inbox, output) = (inbox.clone(), output.clone());
            std::thread::Builder::new()
                .name("remote-viewer-decoder".to_string())
                .spawn(move || run_decoder(&inbox, &output, &*wake))
                .map_err(|e| anyhow::anyhow!("couldn't start the frame decoder: {e}"))?;
        }
        router.attach(SessionKey::new(peer.id, peer.session_id), inbox.clone());
        let pipeline = Pipeline { router: router.clone(), inbox, output };
        tracing::debug!("RemoteViewer::open: window {window_id:?}, softbuffer surface and decoder created for peer '{}'", peer.name);

        Ok(Self {
            window,
            surface,
            peer_id: peer.id.to_string(),
            commands,
            keyboard: KeyboardForwarder::new(KeyTranslator::new(cfg!(target_os = "macos"), peer.platform)),
            held_buttons: Vec::new(),
            frame: None,
            placement: None,
            cursor: None,
            last_move_sent: Instant::now() - MOVE_THROTTLE,
            last_sent_position: None,
            redraw_pending: false,
            rendered_once: false,
            opened_at: Instant::now(),
            pipeline,
        })
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    /// True once the window has gone `RENDER_WATCHDOG` without ever
    /// successfully painting a frame — the caller should treat this as
    /// a fatal viewer failure and tear the session down rather than
    /// leave the window open. See `RENDER_WATCHDOG`'s doc comment.
    pub fn is_stuck(&self) -> bool {
        !self.rendered_once && self.opened_at.elapsed() > RENDER_WATCHDOG
    }

    /// Whether at least one real frame has been painted yet — the caller
    /// uses this to decide whether the event loop still needs to keep
    /// waking itself on a timer so `is_stuck`'s watchdog actually gets a
    /// chance to run. A session whose screen-stream connection never
    /// arrives produces no frames at all, and `ControlFlow::Wait` only
    /// wakes the loop on a real OS/user event — without an explicit timer
    /// poll, `is_stuck` would never be checked a second time and a
    /// permanently blank window would sit open forever instead of the
    /// watchdog ever catching it.
    pub fn is_rendered(&self) -> bool {
        self.rendered_once
    }

    /// The decoder thread's wake-up (`AppEvent::ViewerFrameReady`): picks
    /// up the frame it left and asks the window to repaint — the actual
    /// blit happens in `redraw`, on the OS's own schedule
    /// (`RedrawRequested`), not synchronously here.
    pub fn show_ready_frame(&mut self) {
        if let Some(frame) = self.pipeline.output.take() {
            self.frame = Some(frame);
            self.request_redraw();
        }
    }

    /// Only requests a redraw if one isn't already pending (see
    /// `redraw_pending`'s doc comment).
    fn request_redraw(&mut self) {
        if !self.redraw_pending {
            self.redraw_pending = true;
            self.window.request_redraw();
        }
    }

    /// Copies the latest decoded frame into the window, centered — the
    /// decoder already sized it to fit, so this is a row-by-row memcpy and
    /// nothing more. No-ops gracefully if the window is momentarily
    /// zero-sized (e.g. mid-resize) rather than erroring — `softbuffer`
    /// can't resize to zero either way.
    pub fn redraw(&mut self) {
        self.redraw_pending = false;
        let size = self.window.inner_size();
        // Belt and braces next to `Resized`: whatever size the window is
        // really being painted at is what the next frame gets decoded for.
        self.pipeline.inbox.set_target((size.width, size.height));
        let (Some(width), Some(height)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            tracing::debug!("redraw: window reported a zero-sized dimension ({}x{}), skipping", size.width, size.height);
            return;
        };
        if let Err(e) = self.surface.resize(width, height) {
            tracing::warn!("redraw: softbuffer surface resize failed: {e}");
            return;
        }
        let mut buffer = match self.surface.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("redraw: softbuffer buffer_mut failed: {e}");
                return;
            }
        };
        let window = (width.get(), height.get());
        let placement = self.frame.as_ref().map(|f| Placement::centered(window, (f.width, f.height)));
        paint(&mut buffer, window, self.frame.as_ref().zip(placement));
        if let Err(e) = buffer.present() {
            tracing::warn!("redraw: softbuffer present failed: {e}");
            return;
        }
        self.placement = placement;
        self.rendered_once = true;
    }

    /// Handles one local window event: mouse/keyboard become
    /// `EngineCommand::SendInputEvent`s to the peer, `CloseRequested`
    /// ends the session. Returns `ViewerAction::Closed` when the caller
    /// should drop this viewer (the window is gone either way once tao
    /// processes the close).
    pub fn handle_window_event(&mut self, event: &WindowEvent<'_>) -> ViewerAction {
        match event {
            WindowEvent::CloseRequested => {
                // Before the session ends, while the peer still accepts
                // input — nothing should stay held down over there.
                self.release_all();
                let _ = self.commands.send(EngineCommand::EndRemoteControlSession { peer_crypto_id: self.peer_id.clone() });
                return ViewerAction::Closed;
            }
            WindowEvent::Resized(size) => {
                self.pipeline.inbox.set_target((size.width, size.height));
                // Re-centers the frame already on screen right away; the
                // decoder's re-fitted one follows.
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(*position);
                if self.last_move_sent.elapsed() >= MOVE_THROTTLE {
                    self.send_position(*position);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = proto_mouse_button(*button) {
                    self.handle_mouse_button(button, *state);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (delta_x, delta_y) = match delta {
                    // Lines aren't pixels, but there's no real "line
                    // height" to convert against here — a flat
                    // per-notch multiplier gets scroll speed in the
                    // right ballpark without needing to know anything
                    // about the peer's actual font/UI metrics.
                    MouseScrollDelta::LineDelta(x, y) => (*x as f64 * 24.0, *y as f64 * 24.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
                    _ => (0.0, 0.0),
                };
                if delta_x != 0.0 || delta_y != 0.0 {
                    // Natural/"reverse" scrolling on this device (common
                    // default on trackpads) shouldn't dictate the sign
                    // sent to the peer — negating y here would just be
                    // guessing at a convention neither side has agreed
                    // on, so this passes the delta through as-is.
                    self.send(InputEventKind::MouseScroll { delta_x, delta_y });
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                tracing::trace!(
                    "handle_window_event: KeyboardInput physical_key={:?} state={:?} repeat={} synthetic={is_synthetic}",
                    event.physical_key,
                    event.state,
                    event.repeat
                );
                // tao makes up presses for keys already held when the
                // window gains focus (and releases when it loses it) on
                // some platforms. Forwarding a made-up press would hold,
                // say, the Alt that was part of the Alt-Tab into this
                // window down on the peer; focus loss is handled below.
                if *is_synthetic {
                    return ViewerAction::None;
                }
                let pressed = match event.state {
                    ElementState::Pressed => true,
                    ElementState::Released => false,
                    _ => return ViewerAction::None,
                };
                match self.keyboard.key(event.physical_key, pressed) {
                    Some(kind) => self.send(kind),
                    None if self.keyboard.translator.code(event.physical_key).is_none() => tracing::debug!(
                        "handle_window_event: no target key code for {:?} on platform {:?}",
                        event.physical_key,
                        self.keyboard.translator.peer
                    ),
                    None => {}
                }
            }
            WindowEvent::Focused(focused) => {
                tracing::debug!("handle_window_event: window focus changed to {focused}");
                if !*focused {
                    // Key-ups for whatever's held go to whichever window
                    // has focus now, never to this one — so release here.
                    self.release_all();
                }
            }
            _ => {}
        }
        ViewerAction::None
    }

    /// A press only counts on the picture itself, not on the letterbox
    /// bars around it (where there's nothing of the peer's to click on —
    /// clamping it to the nearest edge could land on a taskbar or Dock).
    /// A release always goes through for a press that did, wherever the
    /// pointer has wandered since, so a drag ends where it was let go.
    fn handle_mouse_button(&mut self, button: ProtoMouseButton, state: ElementState) {
        match state {
            ElementState::Pressed => {
                let on_frame = match (self.cursor, self.placement) {
                    (Some(cursor), Some(placement)) => placement.contains(cursor.x, cursor.y),
                    _ => false,
                };
                if !on_frame || self.held_buttons.contains(&button) {
                    return;
                }
                self.flush_position();
                self.held_buttons.push(button);
                self.send(InputEventKind::MouseButton { button, down: true });
            }
            ElementState::Released => {
                if let Some(index) = self.held_buttons.iter().position(|b| *b == button) {
                    self.held_buttons.remove(index);
                    self.flush_position();
                    self.send(InputEventKind::MouseButton { button, down: false });
                }
            }
            _ => {}
        }
    }

    /// Sends the pointer's latest position if the throttle held it back —
    /// a click is placed wherever the last `MouseMove` left the peer's
    /// cursor, so without this a quick move-and-click could land up to one
    /// throttle interval's worth of motion short of where it was clicked.
    fn flush_position(&mut self) {
        let Some(cursor) = self.cursor else { return };
        let target = self.placement.and_then(|p| p.normalize(cursor.x, cursor.y));
        if target.is_some() && target != self.last_sent_position {
            self.send_position(cursor);
        }
    }

    fn send_position(&mut self, position: PhysicalPosition<f64>) {
        if let Some((x, y)) = self.placement.and_then(|p| p.normalize(position.x, position.y)) {
            self.send(InputEventKind::MouseMove { x, y });
            self.last_move_sent = Instant::now();
            self.last_sent_position = Some((x, y));
        }
    }

    /// Lets go of every key and mouse button this viewer still holds down
    /// on the peer.
    fn release_all(&mut self) {
        for button in std::mem::take(&mut self.held_buttons) {
            self.send(InputEventKind::MouseButton { button, down: false });
        }
        for event in self.keyboard.release_all() {
            self.send(event);
        }
    }

    fn send(&self, event: InputEventKind) {
        let _ = self.commands.send(EngineCommand::SendInputEvent { peer_crypto_id: self.peer_id.clone(), event });
    }
}

impl Drop for RemoteViewer {
    /// Every way a viewer goes away (closed, the session ended, the render
    /// watchdog) goes through here. When the session is already over the
    /// engine just ignores these; when it isn't, they arrive before the
    /// `EndRemoteControlSession` the caller sends next.
    fn drop(&mut self) {
        self.release_all();
    }
}

/// Brings the tray app forward so the viewer window gets keyboard focus —
/// see the call site in `RemoteViewer::open`. `activateIgnoringOtherApps:`
/// is the call through macOS 13 (deprecated since); macOS 14 adds the
/// cooperative `activate`, so there both are called. `orderFrontRegardless`
/// puts the window above other apps' windows even if activation is
/// refused, so it's at least there to click on. (tao's own `set_focus`,
/// called next, then makes it the key window.)
#[cfg(target_os = "macos")]
fn activate_for_keyboard(window: &Window) {
    use tao::platform::macos::WindowExtMacOS;
    let Some(mtm) = objc2_foundation::MainThreadMarker::new() else {
        tracing::debug!("activate_for_keyboard: not on the main thread");
        return;
    };
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    if objc2::available!(macos = 14.0) {
        app.activate();
    }
    // SAFETY: tao hands back its live `NSWindow` pointer, valid for as long
    // as `window` is, and this runs on the main thread (checked above).
    if let Some(ns_window) = unsafe { (window.ns_window() as *const objc2_app_kit::NSWindow).as_ref() } {
        ns_window.orderFrontRegardless();
    }
}

fn proto_mouse_button(button: TaoMouseButton) -> Option<ProtoMouseButton> {
    match button {
        TaoMouseButton::Left => Some(ProtoMouseButton::Left),
        TaoMouseButton::Right => Some(ProtoMouseButton::Right),
        TaoMouseButton::Middle => Some(ProtoMouseButton::Middle),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Frame pipeline: engine thread -> router -> inbox -> decoder thread ->
// output -> UI thread. See the module doc comment for why it's built this way.
// ---------------------------------------------------------------------------

/// Which session a frame belongs to — a peer can be controlled again right
/// after a session ends, and a late frame from the old one shouldn't open
/// the new one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionKey {
    peer_id: String,
    session_id: String,
}

impl SessionKey {
    fn new(peer_id: &str, session_id: &str) -> Self {
        Self { peer_id: peer_id.to_string(), session_id: session_id.to_string() }
    }

    fn matches(&self, peer_id: &str, session_id: &str) -> bool {
        self.peer_id == peer_id && self.session_id == session_id
    }
}

/// Where the engine thread hands each incoming screen frame (see
/// `start_engine_thread` in main.rs) — straight into the open viewer's
/// decoder inbox, never through the tao event loop. Shared between that
/// thread and the UI thread, which attaches and detaches viewers.
#[derive(Default)]
pub struct FrameRouter {
    state: Mutex<RouterState>,
}

#[derive(Default)]
struct RouterState {
    /// The open viewer's session and inbox, if any.
    active: Option<(SessionKey, Arc<FrameInbox>)>,
    /// The newest frame for a session no viewer has claimed yet. The engine
    /// announces a session (through the event loop, to open the window)
    /// and starts receiving its frames on another path, so the first frames
    /// can beat the window. Mac and Windows peers send a frame every 100ms
    /// regardless, but a Linux peer only sends one when its screen changes
    /// — losing the first could leave the window blank until something
    /// moves over there. One frame, replaced by every later one.
    parked: Option<(SessionKey, Vec<u8>)>,
}

impl FrameRouter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Engine thread: one frame arrived. Never blocks for longer than a
    /// couple of uncontended locks.
    pub fn offer(&self, peer_id: &str, session_id: &str, jpeg: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        if let Some((key, inbox)) = &state.active {
            if key.matches(peer_id, session_id) {
                inbox.put_frame(jpeg);
                return;
            }
        }
        tracing::trace!("screen frame for '{peer_id}' ({} bytes) with no viewer open for it — kept as the latest", jpeg.len());
        state.parked = Some((SessionKey::new(peer_id, session_id), jpeg));
    }

    fn attach(&self, key: SessionKey, inbox: Arc<FrameInbox>) {
        let mut state = self.state.lock().unwrap();
        if let Some((parked_key, jpeg)) = state.parked.take() {
            if parked_key == key {
                inbox.put_frame(jpeg);
            }
        }
        state.active = Some((key, inbox));
    }

    /// Only detaches `inbox` itself: when a new viewer replaces an old one,
    /// the new one has already attached by the time the old one is dropped.
    fn detach(&self, inbox: &Arc<FrameInbox>) {
        let mut state = self.state.lock().unwrap();
        if state.active.as_ref().is_some_and(|(_, active)| Arc::ptr_eq(active, inbox)) {
            state.active = None;
        }
        state.parked = None;
    }
}

/// The decoder thread's single latest-frame slot, plus the size it should
/// fit frames to. A frame that arrives before the previous one was picked
/// up replaces it — an undecoded frame is never queued behind another.
struct FrameInbox {
    state: Mutex<InboxState>,
    signal: Condvar,
}

#[derive(Default)]
struct InboxState {
    jpeg: Option<Vec<u8>>,
    /// The viewer's inner size, in physical pixels.
    target: (u32, u32),
    /// The target changed since the decoder last looked: the last frame
    /// needs fitting again even if no new one arrives (a Linux peer whose
    /// screen is still sends none).
    target_changed: bool,
    closed: bool,
}

/// What the decoder thread does next: decode `jpeg` if there is one, then
/// fit the newest picture to `target`.
#[derive(Debug, PartialEq)]
struct DecodeJob {
    jpeg: Option<Vec<u8>>,
    target: (u32, u32),
}

impl FrameInbox {
    fn new(target: (u32, u32)) -> Self {
        Self { state: Mutex::new(InboxState { target, ..Default::default() }), signal: Condvar::new() }
    }

    /// Returns whether an older, still-undecoded frame was dropped for it.
    fn put_frame(&self, jpeg: Vec<u8>) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return false;
        }
        let replaced = state.jpeg.replace(jpeg).is_some();
        drop(state);
        self.signal.notify_one();
        replaced
    }

    fn set_target(&self, target: (u32, u32)) {
        let mut state = self.state.lock().unwrap();
        if state.target != target {
            state.target = target;
            state.target_changed = true;
            drop(state);
            self.signal.notify_one();
        }
    }

    /// Ends the decoder thread once it finishes whatever it's doing now.
    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.jpeg = None;
        drop(state);
        self.signal.notify_all();
    }

    /// Blocks until there's something to do; `None` once closed.
    fn next_job(&self) -> Option<DecodeJob> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return None;
            }
            if state.jpeg.is_some() || state.target_changed {
                state.target_changed = false;
                return Some(DecodeJob { jpeg: state.jpeg.take(), target: state.target });
            }
            state = self.signal.wait(state).unwrap();
        }
    }
}

/// A frame ready to blit: `width * height` 0RGB pixels, row-major.
#[derive(Debug, PartialEq)]
pub struct ReadyFrame {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

/// Hands decoded frames to the UI thread, latest only, with the wake-ups
/// coalesced: the decoder sends one `Wake` and no other until the UI
/// thread has picked a frame up. One mutex guards both the frame and the
/// pending flag, so a wake can't be lost between the two.
#[derive(Default)]
struct FrameOutput {
    slot: Mutex<OutputSlot>,
}

#[derive(Default)]
struct OutputSlot {
    frame: Option<ReadyFrame>,
    wake_pending: bool,
}

impl FrameOutput {
    /// Decoder thread: replaces whatever frame the UI thread hasn't picked
    /// up yet. Returns whether to send a wake — `false` while one is
    /// already on its way, which will pick this frame up instead.
    fn publish(&self, frame: ReadyFrame) -> bool {
        let mut slot = self.slot.lock().unwrap();
        slot.frame = Some(frame);
        !std::mem::replace(&mut slot.wake_pending, true)
    }

    /// UI thread, on a wake: the newest frame, if one is waiting.
    fn take(&self) -> Option<ReadyFrame> {
        let mut slot = self.slot.lock().unwrap();
        slot.wake_pending = false;
        slot.frame.take()
    }
}

/// The UI thread's end of one viewer's pipeline. Dropping it stops the
/// decoder thread (after any decode already under way) and stops the
/// router feeding it.
struct Pipeline {
    router: Arc<FrameRouter>,
    inbox: Arc<FrameInbox>,
    output: Arc<FrameOutput>,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.router.detach(&self.inbox);
        self.inbox.close();
    }
}

/// The decoder thread: decode the newest JPEG, fit it to the window, hand
/// it over, repeat — until the viewer closes its inbox or the event loop
/// goes away. Keeps the last decoded picture so a resize can be answered
/// without waiting for (or re-decoding) a frame.
fn run_decoder(inbox: &FrameInbox, output: &FrameOutput, wake: &dyn Fn() -> bool) {
    let mut source: Option<image::RgbImage> = None;
    while let Some(job) = inbox.next_job() {
        if let Some(jpeg) = job.jpeg {
            match image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg) {
                Ok(decoded) => source = Some(decoded.into_rgb8()),
                Err(e) => tracing::debug!("couldn't decode incoming remote-control frame ({} bytes): {e}", jpeg.len()),
            }
        }
        let Some(frame) = source.as_ref().and_then(|source| fit_frame(source, job.target)) else {
            continue;
        };
        if output.publish(frame) && !wake() {
            tracing::debug!("remote-control viewer's event loop is gone — stopping its decoder");
            return;
        }
    }
    tracing::debug!("remote-control viewer closed — decoder stopped");
}

/// Scales `source` to the largest size with its aspect ratio that fits
/// `target`, as 0RGB pixels. `image`'s `thumbnail` rather than its
/// general `resize`: both are the image crate's resamplers, but
/// `thumbnail` is an integer area average — the right filter for
/// shrinking a screenshot, text stays legible — and measured 2–4x faster
/// than `resize`'s cheapest filter for this exact job (a 2880x1800 frame
/// into a 2048x1280 Retina window: ~90ms vs ~190ms optimized, ~145ms vs
/// ~500ms+ size-optimized). It interpolates when enlarging, too.
fn fit_frame(source: &image::RgbImage, target: (u32, u32)) -> Option<ReadyFrame> {
    let (width, height) = fit_within(source.dimensions(), target)?;
    let scaled;
    let image = if (width, height) == source.dimensions() {
        source
    } else {
        scaled = image::imageops::thumbnail(source, width, height);
        &scaled
    };
    let pixels = image.as_raw().chunks_exact(3).map(|p| (u32::from(p[0]) << 16) | (u32::from(p[1]) << 8) | u32::from(p[2])).collect();
    Some(ReadyFrame { width, height, pixels })
}

/// The largest `(width, height)` with `source`'s aspect ratio that fits
/// inside `bounds` (rounded, never zero) — `None` if either is empty.
fn fit_within(source: (u32, u32), bounds: (u32, u32)) -> Option<(u32, u32)> {
    let (sw, sh) = (u64::from(source.0), u64::from(source.1));
    let (bw, bh) = (u64::from(bounds.0), u64::from(bounds.1));
    if sw == 0 || sh == 0 || bw == 0 || bh == 0 {
        return None;
    }
    // Integer cross-multiplication: whichever side runs out of room first
    // sets the scale.
    let (w, h) = if sw * bh >= bw * sh { (bw, (sh * bw + sw / 2) / sw) } else { ((sw * bh + sh / 2) / sh, bh) };
    Some((w.clamp(1, bw) as u32, h.clamp(1, bh) as u32))
}

/// Where a frame sits in the window, in physical pixels: centered, so
/// letterboxed when it's smaller, and cropped equally on both sides when
/// it's bigger (for the moment after the window shrinks and before the
/// decoder's re-fitted frame arrives).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placement {
    x: i64,
    y: i64,
    width: u32,
    height: u32,
}

impl Placement {
    fn centered(window: (u32, u32), frame: (u32, u32)) -> Self {
        Self {
            x: (i64::from(window.0) - i64::from(frame.0)) / 2,
            y: (i64::from(window.1) - i64::from(frame.1)) / 2,
            width: frame.0,
            height: frame.1,
        }
    }

    /// A window position as a `0.0..=1.0` position on the frame, clamped to
    /// its edges (a drag that leaves the picture keeps going along them).
    fn normalize(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let nx = ((x - self.x as f64) / f64::from(self.width)).clamp(0.0, 1.0);
        let ny = ((y - self.y as f64) / f64::from(self.height)).clamp(0.0, 1.0);
        Some((nx, ny))
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        let (left, top) = (self.x as f64, self.y as f64);
        x >= left && y >= top && x < left + f64::from(self.width) && y < top + f64::from(self.height)
    }
}

/// Fills a `window`-sized 0RGB buffer: the background, then `frame` copied
/// in row by row at its placement, cropped to the window.
fn paint(buffer: &mut [u32], window: (u32, u32), frame: Option<(&ReadyFrame, Placement)>) {
    buffer.fill(BACKGROUND);
    let Some((frame, at)) = frame else { return };
    let (win_w, win_h) = (i64::from(window.0), i64::from(window.1));
    let (left, right) = (at.x.max(0), (at.x + i64::from(at.width)).min(win_w));
    let (top, bottom) = (at.y.max(0), (at.y + i64::from(at.height)).min(win_h));
    if left >= right || top >= bottom {
        return;
    }
    let columns = (right - left) as usize;
    let src_x = (left - at.x) as usize;
    for y in top..bottom {
        let src = (y - at.y) as usize * frame.width as usize + src_x;
        let dst = y as usize * win_w as usize + left as usize;
        let (Some(dst), Some(src)) = (buffer.get_mut(dst..dst + columns), frame.pixels.get(src..src + columns)) else {
            break;
        };
        dst.copy_from_slice(src);
    }
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

/// Turns this machine's physical keys into the peer's native key codes.
///
/// Between a Mac and a PC (Windows or Linux), Command and Control trade
/// places, so the shortcut someone's hands already know does the same
/// thing over there: from a Mac, Command arrives as Control and Control as
/// the Windows/Super key; from a PC, Control arrives as Command and the
/// Windows/Super key as Control. Mac to Mac and PC to PC send every key as
/// it is. Since both directions are the same exchange, a key that crosses
/// over and back comes home unchanged.
#[derive(Clone, Copy, Debug)]
struct KeyTranslator {
    peer: Platform,
    swap_command_and_control: bool,
}

impl KeyTranslator {
    fn new(this_is_mac: bool, peer: Platform) -> Self {
        Self { peer, swap_command_and_control: this_is_mac != (peer == Platform::MacOs) }
    }

    /// The physical key the peer should see pressed.
    fn peer_key(&self, key: KeyCode) -> KeyCode {
        if self.swap_command_and_control {
            swap_command_and_control(key)
        } else {
            key
        }
    }

    fn code(&self, key: KeyCode) -> Option<u32> {
        native_key_code(self.peer_key(key), self.peer).map(u32::from)
    }
}

fn swap_command_and_control(key: KeyCode) -> KeyCode {
    match key {
        KeyCode::ControlLeft => KeyCode::SuperLeft,
        KeyCode::ControlRight => KeyCode::SuperRight,
        KeyCode::SuperLeft => KeyCode::ControlLeft,
        KeyCode::SuperRight => KeyCode::ControlRight,
        other => other,
    }
}

/// Keys that hold a state rather than type something, so a held one is
/// never repeated (Windows auto-repeats a held Shift; macOS would see each
/// repeat as a fresh modifier change).
fn is_modifier(key: KeyCode) -> bool {
    matches!(
        key,
        KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::AltLeft
            | KeyCode::AltRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
            | KeyCode::CapsLock
            | KeyCode::NumLock
    )
}

/// Which keys are down on the peer because of this viewer — the codes
/// sent a `KeyDown` and no `KeyUp` yet, in press order.
struct KeyboardForwarder {
    translator: KeyTranslator,
    held: Vec<u32>,
}

impl KeyboardForwarder {
    fn new(translator: KeyTranslator) -> Self {
        Self { translator, held: Vec::new() }
    }

    /// The event to send for one local key press or release, if any. A
    /// held key's auto-repeat goes out as another `KeyDown` to a Mac or a
    /// Windows PC, which is how the peer repeats it too (injected keys
    /// don't auto-repeat there). Not to Linux: a Wayland compositor repeats
    /// a held key for its clients by itself, and counts presses — **real
    /// bug fixed here**: GNOME took each repeat as one more press, so the
    /// single release at the end left the key down there, Backspace
    /// deleting text until the session ended, and nothing on either side
    /// could let go of it. Only keys this viewer pressed get released, so a
    /// key already down when the window got focus doesn't send a stray
    /// `KeyUp`.
    fn key(&mut self, key: KeyCode, pressed: bool) -> Option<InputEventKind> {
        let code = self.translator.code(key)?;
        let held = self.held.iter().position(|&c| c == code);
        if pressed {
            match held {
                Some(_) if is_modifier(key) || self.translator.peer == Platform::Linux => None,
                Some(_) => Some(InputEventKind::KeyDown { code }),
                None => {
                    self.held.push(code);
                    Some(InputEventKind::KeyDown { code })
                }
            }
        } else {
            self.held.remove(held?);
            Some(InputEventKind::KeyUp { code })
        }
    }

    /// `KeyUp`s for everything still held, last pressed first — so a
    /// shortcut lets go of its letter before its modifier, as hands do.
    fn release_all(&mut self) -> Vec<InputEventKind> {
        self.held.drain(..).rev().map(|code| InputEventKind::KeyUp { code }).collect()
    }
}

/// A physical key's native code on `platform`: macOS `kVK_*` virtual key
/// codes (Carbon's `Events.h`), Windows `VK_*` virtual-key codes
/// (`WinUser.h`), Linux evdev `KEY_*` codes (`linux/input-event-codes.h` —
/// what `remote_control_linux.rs`'s `NotifyKeyboardKeycode` expects).
/// `None` for keys outside `KEY_CODES` and for Android/iOS, which can't be
/// controlled.
fn native_key_code(key: KeyCode, platform: Platform) -> Option<u16> {
    let row = KEY_CODES.iter().find(|row| row.0 == key)?;
    match platform {
        Platform::MacOs => row.1,
        Platform::Windows => row.2,
        Platform::Linux => row.3,
        Platform::Android | Platform::Ios => None,
    }
}

/// `(key, macOS, Windows, Linux)` — a US-layout keyboard, by position.
/// The letters, digits, space, Return and Backspace are the same codes the
/// Android app sends (`macKeyCodeFor`/`windowsKeyCodeFor`/`linuxKeyCodeFor`
/// in `MainActivity.kt`), so the controlled side handles both alike.
/// A `None` is a key that platform doesn't have — see each one's comment.
#[rustfmt::skip]
const KEY_CODES: &[(KeyCode, Option<u16>, Option<u16>, Option<u16>)] = &[
    (KeyCode::KeyA, Some(0x00), Some(0x41), Some(30)),
    (KeyCode::KeyB, Some(0x0B), Some(0x42), Some(48)),
    (KeyCode::KeyC, Some(0x08), Some(0x43), Some(46)),
    (KeyCode::KeyD, Some(0x02), Some(0x44), Some(32)),
    (KeyCode::KeyE, Some(0x0E), Some(0x45), Some(18)),
    (KeyCode::KeyF, Some(0x03), Some(0x46), Some(33)),
    (KeyCode::KeyG, Some(0x05), Some(0x47), Some(34)),
    (KeyCode::KeyH, Some(0x04), Some(0x48), Some(35)),
    (KeyCode::KeyI, Some(0x22), Some(0x49), Some(23)),
    (KeyCode::KeyJ, Some(0x26), Some(0x4A), Some(36)),
    (KeyCode::KeyK, Some(0x28), Some(0x4B), Some(37)),
    (KeyCode::KeyL, Some(0x25), Some(0x4C), Some(38)),
    (KeyCode::KeyM, Some(0x2E), Some(0x4D), Some(50)),
    (KeyCode::KeyN, Some(0x2D), Some(0x4E), Some(49)),
    (KeyCode::KeyO, Some(0x1F), Some(0x4F), Some(24)),
    (KeyCode::KeyP, Some(0x23), Some(0x50), Some(25)),
    (KeyCode::KeyQ, Some(0x0C), Some(0x51), Some(16)),
    (KeyCode::KeyR, Some(0x0F), Some(0x52), Some(19)),
    (KeyCode::KeyS, Some(0x01), Some(0x53), Some(31)),
    (KeyCode::KeyT, Some(0x11), Some(0x54), Some(20)),
    (KeyCode::KeyU, Some(0x20), Some(0x55), Some(22)),
    (KeyCode::KeyV, Some(0x09), Some(0x56), Some(47)),
    (KeyCode::KeyW, Some(0x0D), Some(0x57), Some(17)),
    (KeyCode::KeyX, Some(0x07), Some(0x58), Some(45)),
    (KeyCode::KeyY, Some(0x10), Some(0x59), Some(21)),
    (KeyCode::KeyZ, Some(0x06), Some(0x5A), Some(44)),
    (KeyCode::Digit1, Some(0x12), Some(0x31), Some(2)),
    (KeyCode::Digit2, Some(0x13), Some(0x32), Some(3)),
    (KeyCode::Digit3, Some(0x14), Some(0x33), Some(4)),
    (KeyCode::Digit4, Some(0x15), Some(0x34), Some(5)),
    (KeyCode::Digit5, Some(0x17), Some(0x35), Some(6)),
    (KeyCode::Digit6, Some(0x16), Some(0x36), Some(7)),
    (KeyCode::Digit7, Some(0x1A), Some(0x37), Some(8)),
    (KeyCode::Digit8, Some(0x1C), Some(0x38), Some(9)),
    (KeyCode::Digit9, Some(0x19), Some(0x39), Some(10)),
    (KeyCode::Digit0, Some(0x1D), Some(0x30), Some(11)),
    // Punctuation: kVK_ANSI_*, VK_OEM_*, KEY_*.
    (KeyCode::Minus,        Some(0x1B), Some(0xBD), Some(12)),
    (KeyCode::Equal,        Some(0x18), Some(0xBB), Some(13)),
    (KeyCode::BracketLeft,  Some(0x21), Some(0xDB), Some(26)),
    (KeyCode::BracketRight, Some(0x1E), Some(0xDD), Some(27)),
    (KeyCode::Backslash,    Some(0x2A), Some(0xDC), Some(43)),
    (KeyCode::Semicolon,    Some(0x29), Some(0xBA), Some(39)),
    (KeyCode::Quote,        Some(0x27), Some(0xDE), Some(40)),
    (KeyCode::Backquote,    Some(0x32), Some(0xC0), Some(41)),
    (KeyCode::Comma,        Some(0x2B), Some(0xBC), Some(51)),
    (KeyCode::Period,       Some(0x2F), Some(0xBE), Some(52)),
    (KeyCode::Slash,        Some(0x2C), Some(0xBF), Some(53)),
    (KeyCode::Space,     Some(0x31), Some(0x20), Some(57)),
    (KeyCode::Enter,     Some(0x24), Some(0x0D), Some(28)),
    (KeyCode::Tab,       Some(0x30), Some(0x09), Some(15)),
    // macOS calls Backspace "Delete" (kVK_Delete) and Delete
    // "Forward Delete" (kVK_ForwardDelete).
    (KeyCode::Backspace, Some(0x33), Some(0x08), Some(14)),
    (KeyCode::Escape,    Some(0x35), Some(0x1B), Some(1)),
    (KeyCode::Delete,    Some(0x75), Some(0x2E), Some(111)),
    // Mac keyboards have Help (kVK_Help) where PC ones have Insert; tao
    // reports Help as Insert, so the two map onto each other.
    (KeyCode::Insert,    Some(0x72), Some(0x2D), Some(110)),
    (KeyCode::Home,      Some(0x73), Some(0x24), Some(102)),
    (KeyCode::End,       Some(0x77), Some(0x23), Some(107)),
    (KeyCode::PageUp,    Some(0x74), Some(0x21), Some(104)),
    (KeyCode::PageDown,  Some(0x79), Some(0x22), Some(109)),
    (KeyCode::ArrowLeft,  Some(0x7B), Some(0x25), Some(105)),
    (KeyCode::ArrowRight, Some(0x7C), Some(0x27), Some(106)),
    (KeyCode::ArrowUp,    Some(0x7E), Some(0x26), Some(103)),
    (KeyCode::ArrowDown,  Some(0x7D), Some(0x28), Some(108)),
    (KeyCode::F1,  Some(0x7A), Some(0x70), Some(59)),
    (KeyCode::F2,  Some(0x78), Some(0x71), Some(60)),
    (KeyCode::F3,  Some(0x63), Some(0x72), Some(61)),
    (KeyCode::F4,  Some(0x76), Some(0x73), Some(62)),
    (KeyCode::F5,  Some(0x60), Some(0x74), Some(63)),
    (KeyCode::F6,  Some(0x61), Some(0x75), Some(64)),
    (KeyCode::F7,  Some(0x62), Some(0x76), Some(65)),
    (KeyCode::F8,  Some(0x64), Some(0x77), Some(66)),
    (KeyCode::F9,  Some(0x65), Some(0x78), Some(67)),
    (KeyCode::F10, Some(0x6D), Some(0x79), Some(68)),
    (KeyCode::F11, Some(0x67), Some(0x7A), Some(87)),
    (KeyCode::F12, Some(0x6F), Some(0x7B), Some(88)),
    // Modifiers, left and right apart: Shift, Control, Alt/Option and
    // Super (Command on a Mac, the Windows key on Windows, Meta on Linux).
    (KeyCode::CapsLock,     Some(0x39), Some(0x14), Some(58)),
    (KeyCode::ShiftLeft,    Some(0x38), Some(0xA0), Some(42)),
    (KeyCode::ShiftRight,   Some(0x3C), Some(0xA1), Some(54)),
    (KeyCode::ControlLeft,  Some(0x3B), Some(0xA2), Some(29)),
    (KeyCode::ControlRight, Some(0x3E), Some(0xA3), Some(97)),
    (KeyCode::AltLeft,      Some(0x3A), Some(0xA4), Some(56)),
    (KeyCode::AltRight,     Some(0x3D), Some(0xA5), Some(100)),
    (KeyCode::SuperLeft,    Some(0x37), Some(0x5B), Some(125)),
    (KeyCode::SuperRight,   Some(0x36), Some(0x5C), Some(126)),
    // Keypad: kVK_ANSI_Keypad*, VK_NUMPAD*/VK_MULTIPLY etc., KEY_KP*.
    (KeyCode::Numpad0, Some(0x52), Some(0x60), Some(82)),
    (KeyCode::Numpad1, Some(0x53), Some(0x61), Some(79)),
    (KeyCode::Numpad2, Some(0x54), Some(0x62), Some(80)),
    (KeyCode::Numpad3, Some(0x55), Some(0x63), Some(81)),
    (KeyCode::Numpad4, Some(0x56), Some(0x64), Some(75)),
    (KeyCode::Numpad5, Some(0x57), Some(0x65), Some(76)),
    (KeyCode::Numpad6, Some(0x58), Some(0x66), Some(77)),
    (KeyCode::Numpad7, Some(0x59), Some(0x67), Some(71)),
    (KeyCode::Numpad8, Some(0x5B), Some(0x68), Some(72)),
    (KeyCode::Numpad9, Some(0x5C), Some(0x69), Some(73)),
    (KeyCode::NumpadDecimal,  Some(0x41), Some(0x6E), Some(83)),
    (KeyCode::NumpadAdd,      Some(0x45), Some(0x6B), Some(78)),
    (KeyCode::NumpadSubtract, Some(0x4E), Some(0x6D), Some(74)),
    (KeyCode::NumpadMultiply, Some(0x43), Some(0x6A), Some(55)),
    (KeyCode::NumpadDivide,   Some(0x4B), Some(0x6F), Some(98)),
    // Windows has one virtual key for both Enters (the keypad one differs
    // only by a scan-code flag a VK can't carry), so the keypad's arrives
    // as the main Enter there.
    (KeyCode::NumpadEnter,    Some(0x4C), Some(0x0D), Some(96)),
    // Mac and Linux keypads have "="; Windows has no virtual key for one.
    (KeyCode::NumpadEqual,    Some(0x51), None,       Some(117)),
    // A Mac keypad's Clear (kVK_ANSI_KeypadClear) sits where Num Lock is,
    // and tao reports it as Num Lock.
    (KeyCode::NumLock,        Some(0x47), Some(0x90), Some(69)),
];

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOPS: [Platform; 3] = [Platform::MacOs, Platform::Windows, Platform::Linux];

    /// Every key the viewer is meant to send — the brief's full list.
    const EXPECTED_KEYS: &[KeyCode] = &[
        KeyCode::KeyA, KeyCode::KeyB, KeyCode::KeyC, KeyCode::KeyD, KeyCode::KeyE, KeyCode::KeyF, KeyCode::KeyG,
        KeyCode::KeyH, KeyCode::KeyI, KeyCode::KeyJ, KeyCode::KeyK, KeyCode::KeyL, KeyCode::KeyM, KeyCode::KeyN,
        KeyCode::KeyO, KeyCode::KeyP, KeyCode::KeyQ, KeyCode::KeyR, KeyCode::KeyS, KeyCode::KeyT, KeyCode::KeyU,
        KeyCode::KeyV, KeyCode::KeyW, KeyCode::KeyX, KeyCode::KeyY, KeyCode::KeyZ,
        KeyCode::Digit0, KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4,
        KeyCode::Digit5, KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9,
        KeyCode::Minus, KeyCode::Equal, KeyCode::BracketLeft, KeyCode::BracketRight, KeyCode::Backslash,
        KeyCode::Semicolon, KeyCode::Quote, KeyCode::Backquote, KeyCode::Comma, KeyCode::Period, KeyCode::Slash,
        KeyCode::Tab, KeyCode::Escape, KeyCode::Enter, KeyCode::Backspace, KeyCode::Delete, KeyCode::Insert,
        KeyCode::Home, KeyCode::End, KeyCode::PageUp, KeyCode::PageDown,
        KeyCode::ArrowLeft, KeyCode::ArrowRight, KeyCode::ArrowUp, KeyCode::ArrowDown,
        KeyCode::F1, KeyCode::F2, KeyCode::F3, KeyCode::F4, KeyCode::F5, KeyCode::F6,
        KeyCode::F7, KeyCode::F8, KeyCode::F9, KeyCode::F10, KeyCode::F11, KeyCode::F12,
        KeyCode::CapsLock, KeyCode::ShiftLeft, KeyCode::ShiftRight, KeyCode::ControlLeft, KeyCode::ControlRight,
        KeyCode::AltLeft, KeyCode::AltRight, KeyCode::SuperLeft, KeyCode::SuperRight, KeyCode::Space,
        KeyCode::Numpad0, KeyCode::Numpad1, KeyCode::Numpad2, KeyCode::Numpad3, KeyCode::Numpad4,
        KeyCode::Numpad5, KeyCode::Numpad6, KeyCode::Numpad7, KeyCode::Numpad8, KeyCode::Numpad9,
        KeyCode::NumpadDecimal, KeyCode::NumpadAdd, KeyCode::NumpadSubtract, KeyCode::NumpadMultiply,
        KeyCode::NumpadDivide, KeyCode::NumpadEnter, KeyCode::NumpadEqual, KeyCode::NumLock,
    ];

    /// The `None`s in `KEY_CODES`, on purpose.
    const DELIBERATELY_ABSENT: &[(KeyCode, Platform)] = &[(KeyCode::NumpadEqual, Platform::Windows)];

    /// Keys that share a code with another on one platform, on purpose.
    const ALIASES: &[(KeyCode, KeyCode, Platform)] = &[(KeyCode::NumpadEnter, KeyCode::Enter, Platform::Windows)];

    #[test]
    fn every_expected_key_has_a_code_on_every_desktop_or_is_deliberately_absent() {
        for &key in EXPECTED_KEYS {
            for platform in DESKTOPS {
                let absent = DELIBERATELY_ABSENT.contains(&(key, platform));
                assert_eq!(native_key_code(key, platform).is_none(), absent, "{key:?} on {platform:?}");
            }
        }
        // And the table holds nothing beyond the list, each key once.
        assert_eq!(KEY_CODES.len(), EXPECTED_KEYS.len());
        for (i, row) in KEY_CODES.iter().enumerate() {
            assert!(EXPECTED_KEYS.contains(&row.0), "{:?} isn't expected", row.0);
            assert!(KEY_CODES[..i].iter().all(|earlier| earlier.0 != row.0), "{:?} listed twice", row.0);
        }
    }

    #[test]
    fn no_two_keys_share_a_code_on_one_platform_except_known_aliases() {
        for platform in DESKTOPS {
            for (i, a) in KEY_CODES.iter().enumerate() {
                for b in &KEY_CODES[i + 1..] {
                    let (Some(code_a), Some(code_b)) = (native_key_code(a.0, platform), native_key_code(b.0, platform)) else {
                        continue;
                    };
                    if code_a == code_b {
                        assert!(ALIASES.contains(&(b.0, a.0, platform)), "{:?} and {:?} both send {code_a:#x} to {platform:?}", a.0, b.0);
                    }
                }
            }
        }
    }

    #[test]
    fn keys_android_also_sends_keep_their_codes() {
        // Straight from MainActivity.kt's tables, which controlled sides
        // already handle.
        assert_eq!(native_key_code(KeyCode::KeyA, Platform::MacOs), Some(0x00));
        assert_eq!(native_key_code(KeyCode::KeyQ, Platform::MacOs), Some(0x0C));
        assert_eq!(native_key_code(KeyCode::Digit0, Platform::MacOs), Some(0x1D));
        assert_eq!(native_key_code(KeyCode::Space, Platform::MacOs), Some(0x31));
        assert_eq!(native_key_code(KeyCode::Enter, Platform::MacOs), Some(0x24));
        assert_eq!(native_key_code(KeyCode::Backspace, Platform::MacOs), Some(0x33));
        for (i, key) in [KeyCode::KeyA, KeyCode::KeyB, KeyCode::KeyC].into_iter().enumerate() {
            assert_eq!(native_key_code(key, Platform::Windows), Some(b'A' as u16 + i as u16));
        }
        assert_eq!(native_key_code(KeyCode::KeyZ, Platform::Windows), Some(b'Z' as u16));
        assert_eq!(native_key_code(KeyCode::Digit7, Platform::Windows), Some(b'7' as u16));
        assert_eq!(native_key_code(KeyCode::Enter, Platform::Windows), Some(0x0D));
        assert_eq!(native_key_code(KeyCode::Backspace, Platform::Windows), Some(0x08));
        assert_eq!(native_key_code(KeyCode::KeyQ, Platform::Linux), Some(16));
        assert_eq!(native_key_code(KeyCode::KeyM, Platform::Linux), Some(50));
        assert_eq!(native_key_code(KeyCode::Digit0, Platform::Linux), Some(11));
        assert_eq!(native_key_code(KeyCode::Space, Platform::Linux), Some(57));
        assert_eq!(native_key_code(KeyCode::Enter, Platform::Linux), Some(28));
        assert_eq!(native_key_code(KeyCode::Backspace, Platform::Linux), Some(14));
    }

    #[test]
    fn phones_have_no_key_codes() {
        assert_eq!(native_key_code(KeyCode::KeyA, Platform::Android), None);
        assert_eq!(native_key_code(KeyCode::KeyA, Platform::Ios), None);
    }

    #[test]
    fn command_and_control_swap_only_between_mac_and_pc() {
        let mac_to_mac = KeyTranslator::new(true, Platform::MacOs);
        let mac_to_windows = KeyTranslator::new(true, Platform::Windows);
        let mac_to_linux = KeyTranslator::new(true, Platform::Linux);
        let windows_to_mac = KeyTranslator::new(false, Platform::MacOs);
        let linux_to_windows = KeyTranslator::new(false, Platform::Windows);
        let windows_to_linux = KeyTranslator::new(false, Platform::Linux);

        // Mac -> PC: Command -> Control, Control -> Windows/Super.
        assert_eq!(mac_to_windows.code(KeyCode::SuperLeft), Some(0xA2)); // VK_LCONTROL
        assert_eq!(mac_to_windows.code(KeyCode::SuperRight), Some(0xA3)); // VK_RCONTROL
        assert_eq!(mac_to_windows.code(KeyCode::ControlLeft), Some(0x5B)); // VK_LWIN
        assert_eq!(mac_to_linux.code(KeyCode::SuperLeft), Some(29)); // KEY_LEFTCTRL
        assert_eq!(mac_to_linux.code(KeyCode::ControlRight), Some(126)); // KEY_RIGHTMETA
        // PC -> Mac: Control -> Command, Windows/Super -> Control.
        assert_eq!(windows_to_mac.code(KeyCode::ControlLeft), Some(0x37)); // kVK_Command
        assert_eq!(windows_to_mac.code(KeyCode::ControlRight), Some(0x36)); // kVK_RightCommand
        assert_eq!(windows_to_mac.code(KeyCode::SuperLeft), Some(0x3B)); // kVK_Control
        // Same family: unchanged.
        assert_eq!(mac_to_mac.code(KeyCode::SuperLeft), Some(0x37));
        assert_eq!(mac_to_mac.code(KeyCode::ControlLeft), Some(0x3B));
        assert_eq!(linux_to_windows.code(KeyCode::ControlLeft), Some(0xA2));
        assert_eq!(windows_to_linux.code(KeyCode::SuperLeft), Some(125));
        // Everything else is never touched, in any direction.
        for translator in [mac_to_mac, mac_to_windows, mac_to_linux, windows_to_mac, linux_to_windows, windows_to_linux] {
            for &key in EXPECTED_KEYS {
                if !matches!(key, KeyCode::ControlLeft | KeyCode::ControlRight | KeyCode::SuperLeft | KeyCode::SuperRight) {
                    assert_eq!(translator.peer_key(key), key);
                }
            }
        }
    }

    #[test]
    fn the_swap_round_trips() {
        let mac_to_pc = KeyTranslator::new(true, Platform::Windows);
        let pc_to_mac = KeyTranslator::new(false, Platform::MacOs);
        let mac_to_mac = KeyTranslator::new(true, Platform::MacOs);
        for &key in EXPECTED_KEYS {
            // A key that crosses Mac -> PC and back lands as itself...
            assert_eq!(pc_to_mac.peer_key(mac_to_pc.peer_key(key)), key);
            assert_eq!(swap_command_and_control(swap_command_and_control(key)), key);
            // ...so it reaches a Mac with the code a Mac would have sent.
            assert_eq!(pc_to_mac.code(mac_to_pc.peer_key(key)), mac_to_mac.code(key), "{key:?}");
        }
    }

    #[test]
    fn held_keys_repeat_and_release_once() {
        let mut keyboard = KeyboardForwarder::new(KeyTranslator::new(false, Platform::Windows));
        assert_eq!(keyboard.key(KeyCode::KeyA, true), Some(InputEventKind::KeyDown { code: 0x41 }));
        // Auto-repeat: another KeyDown, still one key held.
        assert_eq!(keyboard.key(KeyCode::KeyA, true), Some(InputEventKind::KeyDown { code: 0x41 }));
        assert_eq!(keyboard.held, vec![0x41]);
        assert_eq!(keyboard.key(KeyCode::KeyA, false), Some(InputEventKind::KeyUp { code: 0x41 }));
        // A release for a key this viewer never pressed goes nowhere.
        assert_eq!(keyboard.key(KeyCode::KeyA, false), None);
        // Modifiers don't repeat.
        assert_eq!(keyboard.key(KeyCode::ShiftLeft, true), Some(InputEventKind::KeyDown { code: 0xA0 }));
        assert_eq!(keyboard.key(KeyCode::ShiftLeft, true), None);
        // Unmapped keys send nothing and aren't tracked.
        assert_eq!(keyboard.key(KeyCode::PrintScreen, true), None);
        assert_eq!(keyboard.held, vec![0xA0]);
    }

    #[test]
    fn a_linux_peer_gets_one_press_and_one_release_for_a_held_key() {
        let mut keyboard = KeyboardForwarder::new(KeyTranslator::new(true, Platform::Linux));
        assert_eq!(keyboard.key(KeyCode::Backspace, true), Some(InputEventKind::KeyDown { code: 14 }));
        // A second of auto-repeat: the compositor over there repeats it.
        for _ in 0..15 {
            assert_eq!(keyboard.key(KeyCode::Backspace, true), None);
        }
        assert_eq!(keyboard.key(KeyCode::Backspace, false), Some(InputEventKind::KeyUp { code: 14 }));
        assert!(keyboard.held.is_empty());
    }

    #[test]
    fn release_all_lets_go_of_everything_last_pressed_first() {
        let mut keyboard = KeyboardForwarder::new(KeyTranslator::new(true, Platform::Windows));
        keyboard.key(KeyCode::SuperLeft, true); // Command -> VK_LCONTROL
        keyboard.key(KeyCode::KeyC, true);
        assert_eq!(keyboard.release_all(), vec![InputEventKind::KeyUp { code: 0x43 }, InputEventKind::KeyUp { code: 0xA2 }]);
        assert!(keyboard.release_all().is_empty());
        // A key released after release_all (the real key-up arriving late)
        // doesn't send a second KeyUp.
        assert_eq!(keyboard.key(KeyCode::KeyC, false), None);
    }

    #[test]
    fn fit_within_preserves_aspect_ratio() {
        // Wider than the window: width-bound, letterboxed top and bottom.
        assert_eq!(fit_within((2880, 1800), (1000, 1000)), Some((1000, 625)));
        // Taller than the window: height-bound, bars at the sides.
        assert_eq!(fit_within((1920, 1080), (2000, 900)), Some((1600, 900)));
        // Exactly the window's shape: fills it.
        assert_eq!(fit_within((2880, 1800), (2048, 1280)), Some((2048, 1280)));
        // Enlarges too.
        assert_eq!(fit_within((800, 600), (1600, 1600)), Some((1600, 1200)));
        // Never collapses to zero.
        assert_eq!(fit_within((10_000, 1), (100, 100)), Some((100, 1)));
        assert_eq!(fit_within((0, 10), (100, 100)), None);
        assert_eq!(fit_within((10, 10), (0, 100)), None);
    }

    #[test]
    fn placement_centers_letterboxes_and_normalizes() {
        let at = Placement::centered((1000, 1000), (1000, 625));
        assert_eq!(at, Placement { x: 0, y: 187, width: 1000, height: 625 });
        assert_eq!(at.normalize(0.0, 187.0), Some((0.0, 0.0)));
        assert_eq!(at.normalize(500.0, 187.0 + 312.5), Some((0.5, 0.5)));
        assert_eq!(at.normalize(1000.0, 812.0), Some((1.0, 1.0)));
        // Off the picture: clamped onto its edge for moves...
        assert_eq!(at.normalize(250.0, 10.0), Some((0.25, 0.0)));
        assert_eq!(at.normalize(-50.0, 2000.0), Some((0.0, 1.0)));
        // ...and not a place a click counts.
        assert!(at.contains(500.0, 500.0));
        assert!(!at.contains(500.0, 100.0));
        assert!(!at.contains(500.0, 812.0));
        // A frame bigger than the window (just after it shrank) hangs over
        // both sides equally.
        let big = Placement::centered((100, 100), (120, 100));
        assert_eq!(big, Placement { x: -10, y: 0, width: 120, height: 100 });
        assert_eq!(big.normalize(50.0, 50.0), Some((0.5, 0.5)));
        assert_eq!(Placement::centered((10, 10), (0, 0)).normalize(1.0, 1.0), None);
    }

    fn frame(width: u32, height: u32) -> ReadyFrame {
        ReadyFrame { width, height, pixels: (0..width * height).collect() }
    }

    #[test]
    fn paint_letterboxes_and_crops() {
        // 2x1 frame centered in a 4x3 window: one row of picture, bars around.
        let small = frame(2, 1);
        let mut buffer = vec![0; 12];
        paint(&mut buffer, (4, 3), Some((&small, Placement::centered((4, 3), (2, 1)))));
        let b = BACKGROUND;
        assert_eq!(buffer, vec![b, b, b, b, b, 0, 1, b, b, b, b, b]);
        // 4x2 frame in a 2x2 window: the middle two columns.
        let wide = frame(4, 2);
        let mut buffer = vec![0; 4];
        paint(&mut buffer, (2, 2), Some((&wide, Placement::centered((2, 2), (4, 2)))));
        assert_eq!(buffer, vec![1, 2, 5, 6]);
        // No frame yet: just the background.
        let mut buffer = vec![0; 4];
        paint(&mut buffer, (2, 2), None);
        assert_eq!(buffer, vec![b; 4]);
    }

    #[test]
    fn fit_frame_scales_and_packs_0rgb() {
        let source = image::RgbImage::from_pixel(40, 20, image::Rgb([0x12, 0x34, 0x56]));
        let fitted = fit_frame(&source, (10, 10)).unwrap();
        assert_eq!((fitted.width, fitted.height), (10, 5));
        assert_eq!(fitted.pixels.len(), 50);
        assert!(fitted.pixels.iter().all(|&p| p == 0x0012_3456));
        // Already the right size: passed through as is.
        let same = fit_frame(&source, (40, 20)).unwrap();
        assert_eq!((same.width, same.height, same.pixels.len()), (40, 20, 800));
        assert_eq!(fit_frame(&source, (0, 10)), None);
    }

    #[test]
    fn inbox_keeps_only_the_newest_undecoded_frame() {
        let inbox = FrameInbox::new((100, 50));
        assert!(!inbox.put_frame(vec![1]));
        assert!(inbox.put_frame(vec![2]), "the older, undecoded frame is dropped");
        assert!(inbox.put_frame(vec![3]));
        assert_eq!(inbox.next_job(), Some(DecodeJob { jpeg: Some(vec![3]), target: (100, 50) }));
        // A resize alone is a job too: the last picture needs fitting again.
        inbox.set_target((200, 100));
        assert_eq!(inbox.next_job(), Some(DecodeJob { jpeg: None, target: (200, 100) }));
        // The same size again isn't.
        inbox.set_target((200, 100));
        inbox.put_frame(vec![4]);
        assert_eq!(inbox.next_job(), Some(DecodeJob { jpeg: Some(vec![4]), target: (200, 100) }));
        inbox.close();
        assert_eq!(inbox.next_job(), None);
        assert!(!inbox.put_frame(vec![5]), "nothing is kept once closed");
    }

    #[test]
    fn output_sends_one_wake_until_the_frame_is_taken() {
        let output = FrameOutput::default();
        assert!(output.publish(frame(1, 1)), "first frame: wake");
        assert!(!output.publish(frame(2, 1)), "wake already in flight");
        assert!(!output.publish(frame(3, 1)));
        // The UI thread gets only the newest.
        assert_eq!(output.take().map(|f| f.width), Some(3));
        assert_eq!(output.take(), None);
        // Picked up: the next frame wakes again.
        assert!(output.publish(frame(4, 1)));
    }

    #[test]
    fn router_feeds_only_the_attached_session_and_hands_over_an_early_frame() {
        let router = FrameRouter::new();
        // A frame that beats the window is kept...
        router.offer("peer", "s1", vec![1]);
        router.offer("peer", "s1", vec![2]);
        let inbox = Arc::new(FrameInbox::new((10, 10)));
        router.attach(SessionKey::new("peer", "s1"), inbox.clone());
        // ...and only the newest is handed over.
        assert_eq!(inbox.next_job().unwrap().jpeg, Some(vec![2]));
        // Another session's or peer's frames don't reach this viewer.
        router.offer("peer", "s0", vec![9]);
        router.offer("other", "s1", vec![9]);
        router.offer("peer", "s1", vec![3]);
        assert_eq!(inbox.next_job().unwrap().jpeg, Some(vec![3]));
        // A newer viewer attached before an older one detaches stays fed.
        let newer = Arc::new(FrameInbox::new((10, 10)));
        router.attach(SessionKey::new("peer", "s2"), newer.clone());
        router.detach(&inbox);
        router.offer("peer", "s2", vec![4]);
        assert_eq!(newer.next_job().unwrap().jpeg, Some(vec![4]));
        // Detached: the router stops feeding it.
        router.detach(&newer);
        router.offer("peer", "s2", vec![5]);
        assert!(!newer.put_frame(vec![6]), "frame 5 never reached the detached inbox");
    }

    fn tiny_jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 100, 50]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode(image.as_raw(), width, height, image::ExtendedColorType::Rgb8)
            .unwrap();
        jpeg
    }

    #[test]
    fn decoder_thread_decodes_fits_wakes_once_and_stops_when_closed() {
        let inbox = Arc::new(FrameInbox::new((32, 32)));
        let output = Arc::new(FrameOutput::default());
        let (wakes_tx, wakes) = std::sync::mpsc::channel();
        let decoder = {
            let (inbox, output) = (inbox.clone(), output.clone());
            std::thread::spawn(move || run_decoder(&inbox, &output, &move || wakes_tx.send(()).is_ok()))
        };
        inbox.put_frame(tiny_jpeg(64, 32));
        wakes.recv_timeout(Duration::from_secs(10)).expect("woken for the first frame");
        let first = output.take().expect("a frame is waiting");
        assert_eq!((first.width, first.height), (32, 16));
        // A resize re-fits the last picture without a new frame.
        inbox.set_target((16, 16));
        wakes.recv_timeout(Duration::from_secs(10)).expect("woken for the re-fit");
        let refit = output.take().expect("re-fitted frame");
        assert_eq!((refit.width, refit.height), (16, 8));
        // A frame that won't decode leaves the last good picture up...
        inbox.put_frame(vec![0, 1, 2, 3]);
        wakes.recv_timeout(Duration::from_secs(10)).expect("woken after the bad frame");
        let kept = output.take().expect("the last good picture");
        assert_eq!((kept.width, kept.height), (16, 8));
        // ...and doesn't stop the next one.
        inbox.put_frame(tiny_jpeg(8, 8));
        wakes.recv_timeout(Duration::from_secs(10)).expect("still decoding after a bad frame");
        let next = output.take().expect("the next frame");
        assert_eq!((next.width, next.height), (16, 16));
        inbox.close();
        decoder.join().expect("decoder thread ends once its inbox closes");
    }

    #[test]
    fn decoder_thread_stops_when_the_event_loop_is_gone() {
        let inbox = Arc::new(FrameInbox::new((8, 8)));
        let output = Arc::new(FrameOutput::default());
        let decoder = {
            let (inbox, output) = (inbox.clone(), output.clone());
            std::thread::spawn(move || run_decoder(&inbox, &output, &|| false))
        };
        inbox.put_frame(tiny_jpeg(8, 8));
        decoder.join().expect("decoder thread ends when its wake fails");
    }
}

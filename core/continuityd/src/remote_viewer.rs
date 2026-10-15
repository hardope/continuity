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
//! Keyboard input reuses the same deliberately-small subset the Android
//! app's bridge covers (lowercase letters, digits, space, enter,
//! backspace — see `MainActivity.kt`'s `macKeyCodeFor`/
//! `windowsKeyCodeFor`), mapped from `tao`'s layout-independent
//! `physical_key` instead of a typed character, for the same reason
//! `InputEventKind` itself uses raw platform key codes: the receiving
//! side's platform, not this one, decides what a code means.
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
    peer_platform: Platform,
    /// Mouse buttons pressed on the peer and not yet released — released
    /// on focus loss and close, so a drag interrupted by Cmd-Tab/Alt-Tab
    /// doesn't stay "held" over there.
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
            peer_platform: peer.platform,
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
                // input — no button should stay held down over there.
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
            WindowEvent::KeyboardInput { event, .. } => {
                tracing::debug!("handle_window_event: KeyboardInput physical_key={:?} state={:?} repeat={}", event.physical_key, event.state, event.repeat);
                if !event.repeat {
                    match target_key_code(event.physical_key, self.peer_platform) {
                        Some(code) => {
                            let kind = if event.state == ElementState::Pressed {
                                InputEventKind::KeyDown { code }
                            } else {
                                InputEventKind::KeyUp { code }
                            };
                            self.send(kind);
                        }
                        None => tracing::debug!(
                            "handle_window_event: no target key code for {:?} on platform {:?} — outside the supported subset",
                            event.physical_key,
                            self.peer_platform
                        ),
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                tracing::debug!("handle_window_event: window focus changed to {focused}");
                if !*focused {
                    // A button let go of now goes to whichever window has
                    // focus, never to this one — so release here.
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

    /// Lets go of every mouse button this viewer still holds down on the
    /// peer.
    fn release_all(&mut self) {
        for button in std::mem::take(&mut self.held_buttons) {
            self.send(InputEventKind::MouseButton { button, down: false });
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

/// Translates a physical key position to the *target* (peer's) platform
/// native code — same intentionally small subset as the Android
/// keyboard bridge (see that file's own doc comment for why): lowercase
/// letters, digits, space, enter, backspace. Anything else (shift,
/// symbols, arrows, function keys) is silently dropped rather than sent
/// as a wrong/misleading code.
fn target_key_code(key: KeyCode, platform: Platform) -> Option<u32> {
    match platform {
        Platform::Windows => windows_key_code(key),
        Platform::Linux => linux_key_code(key),
        _ => mac_key_code(key),
    }
}

/// macOS `kVK_ANSI_*` codes — same table as `MainActivity.kt`'s
/// `macKeyCodeFor`, just keyed by `tao::keyboard::KeyCode` instead of a
/// typed character.
fn mac_key_code(key: KeyCode) -> Option<u32> {
    Some(match key {
        KeyCode::KeyA => 0x00,
        KeyCode::KeyS => 0x01,
        KeyCode::KeyD => 0x02,
        KeyCode::KeyF => 0x03,
        KeyCode::KeyH => 0x04,
        KeyCode::KeyG => 0x05,
        KeyCode::KeyZ => 0x06,
        KeyCode::KeyX => 0x07,
        KeyCode::KeyC => 0x08,
        KeyCode::KeyV => 0x09,
        KeyCode::KeyB => 0x0B,
        KeyCode::KeyQ => 0x0C,
        KeyCode::KeyW => 0x0D,
        KeyCode::KeyE => 0x0E,
        KeyCode::KeyR => 0x0F,
        KeyCode::KeyY => 0x10,
        KeyCode::KeyT => 0x11,
        KeyCode::Digit1 => 0x12,
        KeyCode::Digit2 => 0x13,
        KeyCode::Digit3 => 0x14,
        KeyCode::Digit4 => 0x15,
        KeyCode::Digit6 => 0x16,
        KeyCode::Digit5 => 0x17,
        KeyCode::Digit9 => 0x19,
        KeyCode::Digit7 => 0x1A,
        KeyCode::Digit8 => 0x1C,
        KeyCode::Digit0 => 0x1D,
        KeyCode::KeyO => 0x1F,
        KeyCode::KeyU => 0x20,
        KeyCode::KeyI => 0x22,
        KeyCode::KeyP => 0x23,
        KeyCode::KeyL => 0x25,
        KeyCode::KeyJ => 0x26,
        KeyCode::KeyK => 0x28,
        KeyCode::KeyN => 0x2D,
        KeyCode::KeyM => 0x2E,
        KeyCode::Space => 0x31,
        KeyCode::Enter => 0x24,
        KeyCode::Backspace => 0x33,
        _ => return None,
    })
}

/// Windows virtual-key codes — the alphanumeric range is just the ASCII
/// uppercase-letter/digit values, same as `MainActivity.kt`'s
/// `windowsKeyCodeFor`.
fn windows_key_code(key: KeyCode) -> Option<u32> {
    Some(match key {
        KeyCode::KeyA => b'A' as u32,
        KeyCode::KeyB => b'B' as u32,
        KeyCode::KeyC => b'C' as u32,
        KeyCode::KeyD => b'D' as u32,
        KeyCode::KeyE => b'E' as u32,
        KeyCode::KeyF => b'F' as u32,
        KeyCode::KeyG => b'G' as u32,
        KeyCode::KeyH => b'H' as u32,
        KeyCode::KeyI => b'I' as u32,
        KeyCode::KeyJ => b'J' as u32,
        KeyCode::KeyK => b'K' as u32,
        KeyCode::KeyL => b'L' as u32,
        KeyCode::KeyM => b'M' as u32,
        KeyCode::KeyN => b'N' as u32,
        KeyCode::KeyO => b'O' as u32,
        KeyCode::KeyP => b'P' as u32,
        KeyCode::KeyQ => b'Q' as u32,
        KeyCode::KeyR => b'R' as u32,
        KeyCode::KeyS => b'S' as u32,
        KeyCode::KeyT => b'T' as u32,
        KeyCode::KeyU => b'U' as u32,
        KeyCode::KeyV => b'V' as u32,
        KeyCode::KeyW => b'W' as u32,
        KeyCode::KeyX => b'X' as u32,
        KeyCode::KeyY => b'Y' as u32,
        KeyCode::KeyZ => b'Z' as u32,
        KeyCode::Digit0 => b'0' as u32,
        KeyCode::Digit1 => b'1' as u32,
        KeyCode::Digit2 => b'2' as u32,
        KeyCode::Digit3 => b'3' as u32,
        KeyCode::Digit4 => b'4' as u32,
        KeyCode::Digit5 => b'5' as u32,
        KeyCode::Digit6 => b'6' as u32,
        KeyCode::Digit7 => b'7' as u32,
        KeyCode::Digit8 => b'8' as u32,
        KeyCode::Digit9 => b'9' as u32,
        KeyCode::Space => 0x20,
        KeyCode::Enter => 0x0D,
        KeyCode::Backspace => 0x08,
        _ => return None,
    })
}

/// Linux evdev `KEY_*` codes (`linux/input-event-codes.h`) — what
/// `remote_control_linux.rs`'s `NotifyKeyboardKeycode` call expects
/// directly. Same table as `MainActivity.kt`'s `linuxKeyCodeFor`.
fn linux_key_code(key: KeyCode) -> Option<u32> {
    Some(match key {
        KeyCode::KeyQ => 16,
        KeyCode::KeyW => 17,
        KeyCode::KeyE => 18,
        KeyCode::KeyR => 19,
        KeyCode::KeyT => 20,
        KeyCode::KeyY => 21,
        KeyCode::KeyU => 22,
        KeyCode::KeyI => 23,
        KeyCode::KeyO => 24,
        KeyCode::KeyP => 25,
        KeyCode::KeyA => 30,
        KeyCode::KeyS => 31,
        KeyCode::KeyD => 32,
        KeyCode::KeyF => 33,
        KeyCode::KeyG => 34,
        KeyCode::KeyH => 35,
        KeyCode::KeyJ => 36,
        KeyCode::KeyK => 37,
        KeyCode::KeyL => 38,
        KeyCode::KeyZ => 44,
        KeyCode::KeyX => 45,
        KeyCode::KeyC => 46,
        KeyCode::KeyV => 47,
        KeyCode::KeyB => 48,
        KeyCode::KeyN => 49,
        KeyCode::KeyM => 50,
        KeyCode::Digit1 => 2,
        KeyCode::Digit2 => 3,
        KeyCode::Digit3 => 4,
        KeyCode::Digit4 => 5,
        KeyCode::Digit5 => 6,
        KeyCode::Digit6 => 7,
        KeyCode::Digit7 => 8,
        KeyCode::Digit8 => 9,
        KeyCode::Digit9 => 10,
        KeyCode::Digit0 => 11,
        KeyCode::Space => 57,
        KeyCode::Enter => 28,
        KeyCode::Backspace => 14,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

//! Linux remote control: the controlled side's screen capture and input
//! injection, through the xdg-desktop-portal `RemoteDesktop` + `ScreenCast`
//! portals (the `portal` module) and the PipeWire stream they hand back.
//! Works under native Wayland (GNOME, KDE Plasma) — where XTest, the
//! classic X11 route, doesn't work at all — and on X11 sessions that run a
//! portal.
//!
//! Unlike macOS and Windows, the desktop asks its own user to approve each
//! session (on top of Continuity's per-device consent), and that dialog
//! can take as long as it takes someone to walk over, or never be answered.
//! So capture here can still fail after `start_capture` has returned a
//! channel: the engine waits for the first frame before opening the screen
//! stream, and asks `capture_failure_reason` if one never comes. Portals
//! that support it (`RemoteDesktop` v2) can remember the approval, so after
//! the first time a session can start without the dialog; the token for
//! that lives next to the trust store.

/// Also what keyboard-and-mouse sharing's Linux backend (input_share/linux.rs)
/// drives the InputCapture portal with.
pub(crate) mod portal;
mod pointer_track;

use crate::frame_encoding::{self, LastFrame};
use pointer_track::{Desktop, PointerTrack};
use continuity_daemon::RemoteControlHost;
use continuity_proto::{InputEventKind, MouseButton as ProtoMouseButton, ScreenEdge};
use pipewire as pw;
use pw::spa;
use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

// Evdev button codes (linux/input-event-codes.h) — `NotifyPointerButton`
// takes these directly; a different numbering entirely from Windows'
// `MOUSEEVENTF_*`/macOS's `CGMouseButton`.
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

/// The same rate macOS and Windows capture at. Negotiated with the
/// compositor as the stream's *maximum* framerate, so it throttles at the
/// source — GNOME then sends a follow-up frame for any change it held
/// back, where dropping frames here could lose the last one.
const MAX_FRAMES_PER_SECOND: u32 = frame_encoding::FRAMES_PER_SECOND as u32;

/// A stream that has connected but produced no picture in this long is
/// treated as broken, rather than leaving the other device on a spinner.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(20);

pub struct LinuxRemoteControlHost {
    restore_token_path: Option<PathBuf>,
    /// The session currently being set up or captured, if any.
    current: Mutex<Option<Arc<CaptureRun>>>,
    /// Why the latest capture stopped on its own — see
    /// `capture_failure_reason`.
    failure: Arc<Mutex<Option<String>>>,
    /// Whether frames show the pointer — see
    /// `RemoteControlHost::set_pointer_in_frames`. Chosen with the portal
    /// when a session starts.
    pointer_in_frames: AtomicBool,
    /// Where the pointer is thought to be, during keyboard-and-mouse sharing
    /// — see `PointerTrack`.
    pointer: Mutex<PointerTrack>,
    /// Whether computers are sharing their keyboard and mouse with this one
    /// (`start_input_only` to `stop_input_only`) — so a screen session's
    /// portal session ending starts an input-only one for them again.
    input_only_wanted: AtomicBool,
    /// Why the latest input-only portal session stopped on its own — see
    /// `input_only_failure`.
    input_failure: Arc<Mutex<Option<String>>>,
}

/// The desktop's extent in the compositor's logical pixels — the bounding
/// box of its monitors, as GDK sees them — kept current from the main
/// thread by `watch_desktop`, since GTK may only be used there. What an
/// input-only session measures the pointer against, and gives as this
/// screen's size. (GDK's monitor geometry is the compositor's own layout,
/// in the units the portal's relative moves are in.)
static DESKTOP: Mutex<Option<Desktop>> = Mutex::new(None);

fn desktop() -> Option<Desktop> {
    *DESKTOP.lock().unwrap()
}

/// Reads the desktop's extent now and every few seconds after, on the main
/// thread (GTK's). Call once, after GTK is up.
pub fn watch_desktop() {
    fn update() {
        use gtk::gdk::prelude::MonitorExt;
        let desktop = gtk::gdk::Display::default().and_then(|display| {
            let monitors: Vec<gtk::gdk::Rectangle> = (0..display.n_monitors()).filter_map(|i| display.monitor(i)).map(|m| m.geometry()).collect();
            pointer_track::union(monitors.iter().map(|m| (m.x(), m.y(), m.width(), m.height())))
        });
        let mut current = DESKTOP.lock().unwrap();
        if *current != desktop {
            tracing::info!("desktop for keyboard-and-mouse sharing: {desktop:?}");
            *current = desktop;
        }
    }
    update();
    gtk::glib::timeout_add_seconds_local(3, || {
        update();
        gtk::glib::ControlFlow::Continue
    });
}

/// One `start_capture`'s worth of state, so a session that's still winding
/// down can't be mistaken for the next one.
struct CaptureRun {
    /// With a monitor's stream (`start_capture`), or input only.
    screen: bool,
    stop: watch::Sender<bool>,
    /// Feeds the single task that delivers this session's input, once the
    /// portal session exists.
    input: Mutex<Option<mpsc::UnboundedSender<InputEventKind>>>,
    /// Input is being delivered: the desktop approved, and the session
    /// hasn't ended.
    ready: AtomicBool,
    /// The session is over (stopped, or failed on its own).
    finished: AtomicBool,
}

impl LinuxRemoteControlHost {
    pub fn new(restore_token_path: Option<PathBuf>) -> Self {
        Self {
            restore_token_path,
            current: Mutex::new(None),
            failure: Arc::new(Mutex::new(None)),
            pointer_in_frames: AtomicBool::new(true),
            pointer: Mutex::new(PointerTrack::default()),
            input_only_wanted: AtomicBool::new(false),
            input_failure: Arc::new(Mutex::new(None)),
        }
    }

    /// Starts a portal session — with a monitor's stream for the screen
    /// (`start_capture`), or input only — on a thread of its own, since
    /// portal setup is async D-Bus traffic that can wait minutes on a
    /// person. Returns the frame channel, which stays empty for input only.
    /// Input only keeps its approval in a restore token of its own: the
    /// desktop won't restore a screen session's for it, or the other way
    /// round, and each would otherwise spend the other's.
    fn start_run(&self, screen: bool) -> Option<mpsc::Receiver<Vec<u8>>> {
        let (stop_tx, stop_rx) = watch::channel(false);
        let run = Arc::new(CaptureRun {
            screen,
            stop: stop_tx,
            input: Mutex::new(None),
            ready: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        if let Some(previous) = self.current.lock().unwrap().replace(run.clone()) {
            let _ = previous.stop.send(true);
        }
        *self.failure.lock().unwrap() = None;
        *self.input_failure.lock().unwrap() = None;
        *self.pointer.lock().unwrap() = PointerTrack::default();

        let (frames_tx, frames_rx) = mpsc::channel(2);
        let failure = if screen { self.failure.clone() } else { self.input_failure.clone() };
        let token_path = if screen { self.restore_token_path.clone() } else { self.restore_token_path.as_deref().map(input_only_token_path) };
        let pointer = self.pointer_in_frames.load(Ordering::Relaxed);
        let spawned = std::thread::Builder::new().name("remote-desktop".to_string()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    *failure.lock().unwrap() = Some(format!("couldn't start the desktop's remote-desktop session: {e}"));
                    run.finished.store(true, Ordering::Relaxed);
                    return;
                }
            };
            runtime.block_on(run_session(run, stop_rx, frames_tx, failure, token_path, pointer, screen));
        });
        if let Err(e) = spawned {
            tracing::warn!("couldn't start the remote-desktop thread: {e}");
            return None;
        }
        Some(frames_rx)
    }

    /// The portal session that injects input now, if one is up or on its
    /// way.
    fn live_run(&self) -> Option<Arc<CaptureRun>> {
        self.current.lock().unwrap().clone().filter(|run| !run.finished.load(Ordering::Relaxed))
    }
}

/// Where an input-only session's restore token is kept: next to the
/// screen session's.
fn input_only_token_path(screen_token: &Path) -> PathBuf {
    let mut name = screen_token.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    name.push(".input-only");
    screen_token.with_file_name(name)
}

/// Where the portal's restore token is kept: next to the trust store, one
/// per profile, readable only by this user.
pub fn restore_token_path(profile: &str) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("app", "continuity", "continuity")?;
    Some(dirs.config_dir().join(format!("remote-desktop-restore-token.{profile}")))
}

impl RemoteControlHost for LinuxRemoteControlHost {
    fn inject(&self, event: InputEventKind) {
        crate::input_share::note_remote_input();
        match event {
            InputEventKind::MouseMove { x, y } => self.pointer.lock().unwrap().placed(desktop(), x, y),
            InputEventKind::MouseMoveRelative { dx, dy } => self.pointer.lock().unwrap().moved(desktop(), dx, dy),
            _ => {}
        }
        let Some(run) = self.current.lock().unwrap().clone() else {
            return;
        };
        let input = run.input.lock().unwrap().clone();
        if let Some(input) = input {
            let _ = input.send(event);
        }
    }

    fn start_capture(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        // Replaces an input-only session, if there is one: this one carries
        // their input too, and they get theirs back when it ends.
        self.start_run(true)
    }

    /// A `RemoteDesktop` session with no screen source: the desktop's dialog
    /// asks about input only (or a stored approval restores it), and input
    /// is injected once it's up. Setup is asynchronous, so this can't say
    /// whether the person here approves; until they do, injected input is
    /// dropped and the controller's cursor sent straight back
    /// (`pointer_at_edge`), and a refusal ends the sessions
    /// (`input_only_failure`). A screen session already up carries the
    /// input instead.
    fn start_input_only(&self) -> Result<(), String> {
        self.input_only_wanted.store(true, Ordering::Relaxed);
        if self.live_run().is_some() {
            return Ok(());
        }
        match self.start_run(false) {
            Some(_frames) => Ok(()),
            None => Err("couldn't start the desktop's remote-desktop session".to_string()),
        }
    }

    fn stop_input_only(&self) {
        self.input_only_wanted.store(false, Ordering::Relaxed);
        let mut current = self.current.lock().unwrap();
        if current.as_ref().is_some_and(|run| !run.screen) {
            if let Some(run) = current.take() {
                let _ = run.stop.send(true);
            }
        }
    }

    fn input_only_failure(&self) -> Option<String> {
        self.input_failure.lock().unwrap().clone()
    }

    /// This screen's size, for a controller placing the pointer: the
    /// desktop's, when GDK has said.
    fn screen_size(&self) -> Option<(u32, u32)> {
        let desktop = desktop()?;
        (desktop.width >= 1.0 && desktop.height >= 1.0).then(|| (desktop.width as u32, desktop.height as u32))
    }

    /// Where the pointer is thought to be (`PointerTrack`) — or, while no
    /// portal session can inject yet (the desktop's dialog not answered),
    /// the edge it arrived at, so the controller takes its cursor straight
    /// back instead of sending it into nothing.
    fn pointer_at_edge(&self) -> Option<(ScreenEdge, f64)> {
        let ready = self.live_run().is_some_and(|run| run.ready.load(Ordering::Relaxed));
        let track = self.pointer.lock().unwrap();
        if ready {
            track.at_edge(desktop())
        } else {
            track.arrived()
        }
    }

    fn stop_capture(&self) {
        {
            let mut current = self.current.lock().unwrap();
            if current.as_ref().is_some_and(|run| run.screen) {
                if let Some(run) = current.take() {
                    let _ = run.stop.send(true);
                }
            }
        }
        // Computers sharing their keyboard and mouse with this one had their
        // input carried by the screen session; they get a session of their
        // own back.
        if self.input_only_wanted.load(Ordering::Relaxed) && self.live_run().is_none() {
            self.start_run(false);
        }
    }

    fn capture_failure_reason(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }

    fn set_pointer_in_frames(&self, include: bool) {
        self.pointer_in_frames.store(include, Ordering::Relaxed);
    }
}

/// One session start to finish: set it up with the portal, capture until
/// told to stop (or the stream fails), then close the portal session. The
/// engine's frame channel only closes at the very end, after any failure
/// reason has been recorded — that ordering is what lets the engine say
/// *why* a session ended.
async fn run_session(
    run: Arc<CaptureRun>,
    stop: watch::Receiver<bool>,
    frames: mpsc::Sender<Vec<u8>>,
    failure: Arc<Mutex<Option<String>>>,
    token_path: Option<PathBuf>,
    pointer: bool,
    screen: bool,
) {
    let restore_token = token_path.as_deref().and_then(read_restore_token);
    let negotiated = match portal::negotiate(restore_token.as_deref(), pointer, screen, &stop).await {
        Ok(negotiated) => negotiated,
        Err(e) => {
            if e.kind == portal::ErrorKind::Cancelled {
                tracing::debug!("remote-desktop setup cancelled: {e}");
            } else {
                tracing::warn!("remote-desktop setup failed: {e}");
                // The portal's own reasons are worded for screen sharing.
                let reason = match (screen, e.kind) {
                    (true, _) => e.reason,
                    (false, portal::ErrorKind::Declined) => "remote interaction was declined on this computer".to_string(),
                    (false, portal::ErrorKind::TimedOut) if e.step == "Start" => {
                        format!("nobody allowed remote interaction on this computer within {} seconds", portal::APPROVAL_TIMEOUT.as_secs())
                    }
                    (false, _) => e.reason,
                };
                *failure.lock().unwrap() = Some(reason);
            }
            run.finished.store(true, Ordering::Relaxed);
            return;
        }
    };
    if negotiated.persistence_requested {
        if let Some(path) = &token_path {
            store_restore_token(path, negotiated.restore_token.as_deref());
        }
    }

    let session = Arc::new(negotiated.session);
    // The size of the frames PipeWire delivers (width << 32 | height):
    // written by the capture thread, read when placing the pointer.
    let frame_size = Arc::new(AtomicU64::new(0));

    // One task delivers all of this session's input, so events reach the
    // compositor in the order they were sent — a click has to land after
    // the move before it.
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    *run.input.lock().unwrap() = Some(input_tx);
    let input_task = tokio::spawn(forward_input(session.clone(), input_rx, frame_size.clone()));
    run.ready.store(true, Ordering::Relaxed);

    // The portal closes the session when sharing is stopped from this
    // computer's own controls (GNOME's "Stop" button in the top bar).
    let portal_closed = Arc::new(AtomicBool::new(false));
    let closed_watch = tokio::spawn({
        let session = session.clone();
        let portal_closed = portal_closed.clone();
        async move {
            session.closed().await;
            portal_closed.store(true, Ordering::Relaxed);
        }
    });

    let outcome = match negotiated.pipewire_fd {
        // PipeWire runs its own (non-tokio) loop, so it gets its own
        // thread; this runtime stays free to deliver input in the meantime.
        Some(pipewire_fd) => {
            let (done_tx, done_rx) = oneshot::channel();
            let capture = Capture {
                node_id: session.node_id,
                stop: stop.clone(),
                portal_closed: portal_closed.clone(),
                frames: frames.clone(),
                frame_size,
            };
            let spawned = std::thread::Builder::new()
                .name("remote-desktop-capture".to_string())
                .spawn(move || {
                    let _ = done_tx.send(run_pipewire_capture(pipewire_fd, capture));
                });
            match spawned {
                Ok(_) => done_rx.await.unwrap_or_else(|_| Err("screen capture stopped unexpectedly".to_string())),
                Err(e) => Err(format!("couldn't start screen capture: {e}")),
            }
        }
        // Input only: nothing to capture; the session lasts until it's
        // stopped from either side.
        None => {
            let mut stop = stop.clone();
            loop {
                if *stop.borrow_and_update() {
                    break Ok(());
                }
                if portal_closed.load(Ordering::Relaxed) {
                    break Err("remote interaction was stopped on this computer".to_string());
                }
                tokio::select! {
                    changed = stop.changed() => if changed.is_err() { break Ok(()) },
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
            }
        }
    };
    if let Err(reason) = outcome {
        if !*stop.borrow() {
            tracing::warn!("remote-desktop session ended: {reason}");
            *failure.lock().unwrap() = Some(reason);
        }
    }

    run.ready.store(false, Ordering::Relaxed);
    run.finished.store(true, Ordering::Relaxed);
    input_task.abort();
    closed_watch.abort();
    run.input.lock().unwrap().take();
    session.close().await;
    // Only now does the engine see its frame channel close — after any
    // failure reason above is in place.
    drop(frames);
}

async fn forward_input(session: Arc<portal::PortalSession>, mut events: mpsc::UnboundedReceiver<InputEventKind>, frame_size: Arc<AtomicU64>) {
    // Which size `NotifyPointerMotionAbsolute`'s coordinates are measured
    // against differs by desktop. GNOME's compositor (mutter) divides them
    // by the monitor's scale — they're in the stream's own pixels, the size
    // of the frames PipeWire delivers, which under display scaling is
    // larger than the logical size. KDE Plasma's portal adds them to the
    // output's logical position instead, i.e. the size the portal
    // reported. Using the matching one is what makes a tap on the phone
    // land where it was tapped.
    let logical = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| desktop.to_ascii_uppercase().contains("KDE"));
    while let Some(event) = events.recv().await {
        let result = match event {
            InputEventKind::KeyDown { code } => session.notify_keyboard_keycode(code as i32, true).await,
            InputEventKind::KeyUp { code } => session.notify_keyboard_keycode(code as i32, false).await,
            // Input only: no stream to place the pointer on, only moves —
            // see `pointer_track::placing_moves`.
            InputEventKind::MouseMove { x, y } if session.node_id == 0 => {
                let Some(desktop) = desktop() else {
                    continue;
                };
                let mut result = Ok(());
                for (dx, dy) in pointer_track::placing_moves(desktop, x, y) {
                    result = session.notify_pointer_motion(dx, dy).await;
                    if result.is_err() {
                        break;
                    }
                }
                result
            }
            InputEventKind::MouseMove { x, y } => {
                let Some((width, height)) = pointer_space(&session, &frame_size, logical) else {
                    continue;
                };
                session.notify_pointer_motion_absolute(x.clamp(0.0, 1.0) * width, y.clamp(0.0, 1.0) * height).await
            }
            InputEventKind::MouseMoveRelative { dx, dy } => session.notify_pointer_motion(dx, dy).await,
            InputEventKind::MouseButton { button, down } => {
                let button = match button {
                    ProtoMouseButton::Left => BTN_LEFT,
                    ProtoMouseButton::Right => BTN_RIGHT,
                    ProtoMouseButton::Middle => BTN_MIDDLE,
                };
                session.notify_pointer_button(button, down).await
            }
            // The controller sends tao's "content moves" convention
            // (positive = scroll up/left); the portal takes libinput's
            // (positive = down/right).
            InputEventKind::MouseScroll { delta_x, delta_y } => session.notify_pointer_axis(-delta_x, -delta_y).await,
        };
        if let Err(e) = result {
            tracing::debug!("couldn't deliver input to the portal: {e}");
        }
    }
}

fn pointer_space(session: &portal::PortalSession, frame_size: &AtomicU64, logical: bool) -> Option<(f64, f64)> {
    let packed = frame_size.load(Ordering::Relaxed);
    let frame = (packed != 0).then(|| ((packed >> 32) as f64, (packed & 0xffff_ffff) as f64));
    let reported = session.logical_size.filter(|&(w, h)| w > 0 && h > 0).map(|(w, h)| (f64::from(w), f64::from(h)));
    if logical {
        reported.or(frame)
    } else {
        frame.or(reported)
    }
}

fn read_restore_token(path: &Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// A restore token is single-use: the session that used one hands back its
/// replacement (or nothing, if the person here didn't allow it to persist).
fn store_restore_token(path: &Path, token: Option<&str>) {
    let result = match token {
        Some(token) => crate::share::write_private_file(path, token.as_bytes()),
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    };
    if let Err(e) = result {
        tracing::debug!("couldn't update {}: {e}", path.display());
    }
}

/// Everything the capture thread needs besides the PipeWire remote itself.
struct Capture {
    node_id: u32,
    stop: watch::Receiver<bool>,
    portal_closed: Arc<AtomicBool>,
    frames: mpsc::Sender<Vec<u8>>,
    frame_size: Arc<AtomicU64>,
}

/// What the stream callbacks share — PipeWire runs them all on this thread.
struct CaptureState {
    format: spa::param::video::VideoInfoRaw,
    frames: mpsc::Sender<Vec<u8>>,
    frame_size: Arc<AtomicU64>,
    progress: Rc<CaptureProgress>,
    warned_unreadable: bool,
    /// The last frame sent — an identical one isn't sent again.
    last: LastFrame,
}

/// What the callbacks report back to the loop driving them.
#[derive(Default)]
struct CaptureProgress {
    frames_sent: Cell<u64>,
    /// The engine stopped listening — the session is over.
    receiver_gone: Cell<bool>,
    /// The stream failed or went away; why, worded for the other device.
    ended: RefCell<Option<String>>,
}

/// Runs PipeWire's own event loop on this thread until the session is
/// stopped or the stream fails, encoding each frame to JPEG for the engine.
/// `Err` carries a reason for the controlling device.
fn run_pipewire_capture(pipewire_fd: OwnedFd, capture: Capture) -> Result<(), String> {
    pw::init();

    let mainloop = pw::main_loop::MainLoop::new(None).map_err(|e| format!("couldn't start PipeWire: {e}"))?;
    let context = pw::context::Context::new(&mainloop).map_err(|e| format!("couldn't start PipeWire: {e}"))?;
    // The portal's own PipeWire remote — only this session's stream is
    // visible through it.
    let core = context.connect_fd(pipewire_fd, None).map_err(|e| format!("couldn't connect to the shared screen: {e}"))?;
    let stream = pw::stream::Stream::new(
        &core,
        "continuity-remote-control",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("couldn't open the shared screen: {e}"))?;

    let progress = Rc::new(CaptureProgress::default());
    let state = CaptureState {
        format: Default::default(),
        frames: capture.frames,
        frame_size: capture.frame_size,
        progress: progress.clone(),
        warned_unreadable: false,
        last: LastFrame::default(),
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, state, old, new| {
            tracing::debug!("PipeWire stream state: {old:?} -> {new:?}");
            let ended = match new {
                pw::stream::StreamState::Error(message) => Some(format!("the shared screen stream failed: {message}")),
                // The compositor took the stream away.
                pw::stream::StreamState::Unconnected => Some("screen sharing was stopped on this computer".to_string()),
                _ => None,
            };
            if let Some(reason) = ended {
                state.progress.ended.borrow_mut().get_or_insert(reason);
            }
        })
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video || media_subtype != spa::param::format::MediaSubtype::Raw {
                return;
            }
            if let Err(e) = state.format.parse(param) {
                tracing::warn!("couldn't parse the negotiated video format: {e}");
                return;
            }
            let size = state.format.size();
            state.frame_size.store((u64::from(size.width) << 32) | u64::from(size.height), Ordering::Relaxed);
            let max = state.format.max_framerate();
            tracing::debug!(
                "PipeWire negotiated {:?} at {}x{}, at most {}/{} fps",
                state.format.format(),
                size.width,
                size.height,
                max.num,
                max.denom
            );
        })
        .process(|stream, state| {
            // Only the newest queued frame is worth encoding; dropping a
            // `Buffer` hands it straight back to the compositor.
            let mut newest = None;
            while let Some(buffer) = stream.dequeue_buffer() {
                newest = Some(buffer);
            }
            let Some(mut buffer) = newest else { return };
            let size = state.format.size();
            if size.width == 0 || size.height == 0 {
                return; // No format negotiated yet.
            }
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let chunk = data.chunk();
            let (offset, len, stride) = (chunk.offset() as usize, chunk.size() as usize, chunk.stride());
            if len == 0 || chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) {
                return;
            }
            let data_type = data.type_();
            let Some(mapped) = data.data() else {
                // Only memory PipeWire maps for us is readable here.
                if !state.warned_unreadable {
                    tracing::warn!("PipeWire delivered a {data_type:?} screen buffer that can't be read directly");
                    state.warned_unreadable = true;
                }
                return;
            };
            let start = offset.min(mapped.len());
            let pixels = &mapped[start..(start + len).min(mapped.len())];
            let Some(jpeg) = encode_frame(pixels, size.width as usize, size.height as usize, stride, state.format.format()) else {
                return;
            };
            if !state.last.is_new(&jpeg) {
                return;
            }
            match state.frames.try_send(jpeg.clone()) {
                Ok(()) => {
                    state.last.sent(jpeg);
                    state.progress.frames_sent.set(state.progress.frames_sent.get() + 1);
                }
                // Still sending the previous frame; this one would be
                // stale by the time it could go.
                Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => state.progress.receiver_gone.set(true),
            }
        })
        .register()
        .map_err(|e| format!("couldn't watch the shared screen stream: {e}"))?;

    // What this can read: plain 32-bit RGB layouts in memory PipeWire maps
    // for us. Not offering a DMA-BUF `modifier` is what keeps GNOME on
    // shared-memory buffers (its DMA-BUF formats require one). The
    // framerate range accepts anything (GNOME reports a variable 0/1, a
    // high-refresh monitor may report more); the *maximum* framerate is the
    // actual throttle, as browsers' screen capture does it.
    let format = pw::spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(spa::param::format::FormatProperties::MediaType, Id, spa::param::format::MediaType::Video),
        pw::spa::pod::property!(spa::param::format::FormatProperties::MediaSubtype, Id, spa::param::format::MediaSubtype::Raw),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: MAX_FRAMES_PER_SECOND, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
        pw::spa::pod::property!(
            spa::param::format::FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: MAX_FRAMES_PER_SECOND, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: MAX_FRAMES_PER_SECOND, denom: 1 }
        ),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pw::spa::pod::Value::Object(format))
        .map_err(|e| format!("couldn't describe the wanted video format: {e}"))?
        .0
        .into_inner();
    let format_pod = pw::spa::pod::Pod::from_bytes(&values).ok_or_else(|| "couldn't describe the wanted video format".to_string())?;

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(capture.node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [format_pod],
        )
        .map_err(|e| format!("couldn't connect to the shared screen: {e}"))?;
    tracing::debug!("PipeWire capture started for node {}", capture.node_id);

    let started = Instant::now();
    loop {
        if *capture.stop.borrow() || progress.receiver_gone.get() {
            return Ok(());
        }
        if capture.portal_closed.load(Ordering::Relaxed) {
            return Err("screen sharing was stopped on this computer".to_string());
        }
        if let Some(reason) = progress.ended.borrow_mut().take() {
            return Err(reason);
        }
        if progress.frames_sent.get() == 0 && started.elapsed() > FIRST_FRAME_TIMEOUT {
            return Err("the shared screen never delivered a picture".to_string());
        }
        mainloop.loop_().iterate(Duration::from_millis(100));
    }
}

/// Turns one raw frame into a JPEG: honors the row `stride` (compositors
/// pad rows, so it can exceed `width * 4`) and scales down by a whole
/// factor when the frame is wider than `frame_encoding::MAX_ENCODED_WIDTH`.
/// Only the 32-bit layouts offered in `run_pipewire_capture` are handled.
fn encode_frame(pixels: &[u8], width: usize, height: usize, stride: i32, format: spa::param::video::VideoFormat) -> Option<Vec<u8>> {
    use spa::param::video::VideoFormat;
    let channels = if format == VideoFormat::BGRx || format == VideoFormat::BGRA {
        [2, 1, 0]
    } else if format == VideoFormat::RGBx || format == VideoFormat::RGBA {
        [0, 1, 2]
    } else {
        tracing::debug!("unsupported PipeWire video format {format:?}");
        return None;
    };
    let (rgb, out_width, out_height) = frame_encoding::to_rgb(pixels, width, height, stride, channels)?;
    frame_encoding::encode_jpeg(&rgb, out_width, out_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_only_keeps_its_own_restore_token() {
        let screen = Path::new("/home/me/.config/continuity/remote-desktop-restore-token.default");
        assert_eq!(input_only_token_path(screen), Path::new("/home/me/.config/continuity/remote-desktop-restore-token.default.input-only"));
    }

    #[test]
    fn encode_frame_produces_a_jpeg() {
        let pixels = vec![128u8; 64 * 32 * 4];
        let jpeg = encode_frame(&pixels, 64, 32, 64 * 4, spa::param::video::VideoFormat::BGRx).expect("encodes");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG start-of-image marker");
    }
}

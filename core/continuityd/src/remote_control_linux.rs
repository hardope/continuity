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

mod portal;

use continuity_daemon::RemoteControlHost;
use continuity_proto::{InputEventKind, MouseButton as ProtoMouseButton};
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
const MAX_FRAMES_PER_SECOND: u32 = 10;

/// Frames wider than this are scaled down by a whole factor before
/// encoding. A phone can't show more than this across anyway, and
/// JPEG-encoding a 4K frame ten times a second would keep a core busy.
const MAX_ENCODED_WIDTH: usize = 1920;

const JPEG_QUALITY: u8 = 40;

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
}

/// One `start_capture`'s worth of state, so a session that's still winding
/// down can't be mistaken for the next one.
struct CaptureRun {
    stop: watch::Sender<bool>,
    /// Feeds the single task that delivers this session's input, once the
    /// portal session exists.
    input: Mutex<Option<mpsc::UnboundedSender<InputEventKind>>>,
}

impl LinuxRemoteControlHost {
    pub fn new(restore_token_path: Option<PathBuf>) -> Self {
        Self { restore_token_path, current: Mutex::new(None), failure: Arc::new(Mutex::new(None)) }
    }
}

/// Where the portal's restore token is kept: next to the trust store, one
/// per profile, readable only by this user.
pub fn restore_token_path(profile: &str) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("app", "continuity", "continuity")?;
    Some(dirs.config_dir().join(format!("remote-desktop-restore-token.{profile}")))
}

impl RemoteControlHost for LinuxRemoteControlHost {
    fn inject(&self, event: InputEventKind) {
        let Some(run) = self.current.lock().unwrap().clone() else {
            return;
        };
        let input = run.input.lock().unwrap().clone();
        if let Some(input) = input {
            let _ = input.send(event);
        }
    }

    fn start_capture(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        let (stop_tx, stop_rx) = watch::channel(false);
        let run = Arc::new(CaptureRun { stop: stop_tx, input: Mutex::new(None) });
        if let Some(previous) = self.current.lock().unwrap().replace(run.clone()) {
            let _ = previous.stop.send(true);
        }
        *self.failure.lock().unwrap() = None;

        let (frames_tx, frames_rx) = mpsc::channel(2);
        let failure = self.failure.clone();
        let token_path = self.restore_token_path.clone();
        // Portal setup is async D-Bus traffic that can wait minutes on a
        // person, so it gets its own thread and runtime instead of
        // borrowing the engine's.
        let spawned = std::thread::Builder::new().name("remote-desktop".to_string()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(e) => {
                    *failure.lock().unwrap() = Some(format!("couldn't start screen sharing: {e}"));
                    return;
                }
            };
            runtime.block_on(run_session(run, stop_rx, frames_tx, failure, token_path));
        });
        if let Err(e) = spawned {
            tracing::warn!("couldn't start the remote-desktop thread: {e}");
            return None;
        }
        Some(frames_rx)
    }

    fn stop_capture(&self) {
        if let Some(run) = self.current.lock().unwrap().take() {
            let _ = run.stop.send(true);
        }
    }

    fn capture_failure_reason(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
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
) {
    let restore_token = token_path.as_deref().and_then(read_restore_token);
    let negotiated = match portal::negotiate(restore_token.as_deref(), &stop).await {
        Ok(negotiated) => negotiated,
        Err(e) => {
            if e.kind == portal::ErrorKind::Cancelled {
                tracing::debug!("remote-desktop setup cancelled: {e}");
            } else {
                tracing::warn!("remote-desktop setup failed: {e}");
                *failure.lock().unwrap() = Some(e.reason);
            }
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

    // PipeWire runs its own (non-tokio) loop, so it gets its own thread;
    // this runtime stays free to deliver input in the meantime.
    let (done_tx, done_rx) = oneshot::channel();
    let capture = Capture {
        node_id: session.node_id,
        stop: stop.clone(),
        portal_closed,
        frames: frames.clone(),
        frame_size,
    };
    let pipewire_fd = negotiated.pipewire_fd;
    let spawned = std::thread::Builder::new()
        .name("remote-desktop-capture".to_string())
        .spawn(move || {
            let _ = done_tx.send(run_pipewire_capture(pipewire_fd, capture));
        });
    let outcome = match spawned {
        Ok(_) => done_rx.await.unwrap_or_else(|_| Err("screen capture stopped unexpectedly".to_string())),
        Err(e) => Err(format!("couldn't start screen capture: {e}")),
    };
    if let Err(reason) = outcome {
        if !*stop.borrow() {
            tracing::warn!("screen capture ended: {reason}");
            *failure.lock().unwrap() = Some(reason);
        }
    }

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
            InputEventKind::MouseMove { x, y } => {
                let Some((width, height)) = pointer_space(&session, &frame_size, logical) else {
                    continue;
                };
                session.notify_pointer_motion_absolute(x.clamp(0.0, 1.0) * width, y.clamp(0.0, 1.0) * height).await
            }
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
            match state.frames.try_send(jpeg) {
                Ok(()) => state.progress.frames_sent.set(state.progress.frames_sent.get() + 1),
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
/// factor when the frame is wider than `MAX_ENCODED_WIDTH`. Only the
/// 32-bit layouts offered in `run_pipewire_capture` are handled.
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
    let (rgb, out_width, out_height) = to_rgb(pixels, width, height, stride, channels)?;
    let mut jpeg = Vec::new();
    if let Err(e) = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY).encode(
        &rgb,
        out_width as u32,
        out_height as u32,
        image::ExtendedColorType::Rgb8,
    ) {
        tracing::debug!("JPEG encode failed for a PipeWire frame: {e}");
        return None;
    }
    Some(jpeg)
}

/// The pixel-shuffling half of `encode_frame`: picks the R, G and B bytes
/// (at `channels`) out of each 4-byte pixel, sampling every `factor`th
/// pixel and row when downscaling. A buffer shorter than the frame it
/// claims to hold is skipped rather than drawn half-finished.
fn to_rgb(pixels: &[u8], width: usize, height: usize, stride: i32, channels: [usize; 3]) -> Option<(Vec<u8>, usize, usize)> {
    let row_bytes = width.checked_mul(4)?;
    let stride = if stride > 0 { stride as usize } else { row_bytes };
    if stride < row_bytes {
        return None;
    }
    let factor = width.div_ceil(MAX_ENCODED_WIDTH).max(1);
    let (out_width, out_height) = (width / factor, height / factor);
    if out_width == 0 || out_height == 0 {
        return None;
    }
    let mut rgb = Vec::with_capacity(out_width * out_height * 3);
    for row in 0..out_height {
        let start = row * factor * stride;
        let line = pixels.get(start..start + out_width * factor * 4)?;
        for pixel in line.chunks_exact(4 * factor) {
            rgb.extend_from_slice(&[pixel[channels[0]], pixel[channels[1]], pixel[channels[2]]]);
        }
    }
    Some((rgb, out_width, out_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BGR: [usize; 3] = [2, 1, 0];

    #[test]
    fn to_rgb_reorders_channels_and_skips_row_padding() {
        // 2x2 BGRx frame, each row padded to 12 bytes.
        let pixels = [
            1, 2, 3, 0, 4, 5, 6, 0, 0xEE, 0xEE, 0xEE, 0xEE, //
            7, 8, 9, 0, 10, 11, 12, 0, 0xEE, 0xEE, 0xEE, 0xEE,
        ];
        let (rgb, width, height) = to_rgb(&pixels, 2, 2, 12, BGR).expect("converts");
        assert_eq!((width, height), (2, 2));
        assert_eq!(rgb, vec![3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
    }

    #[test]
    fn to_rgb_downscales_frames_wider_than_the_limit() {
        let (width, height) = (MAX_ENCODED_WIDTH * 2, 4);
        let pixels = vec![9u8; width * height * 4];
        let (rgb, out_width, out_height) = to_rgb(&pixels, width, height, 0, BGR).expect("converts");
        assert_eq!((out_width, out_height), (MAX_ENCODED_WIDTH, 2));
        assert_eq!(rgb.len(), out_width * out_height * 3);
    }

    #[test]
    fn to_rgb_skips_a_buffer_shorter_than_its_frame() {
        let pixels = vec![0u8; 2 * 2 * 4 - 1];
        assert!(to_rgb(&pixels, 2, 2, 8, BGR).is_none());
    }

    #[test]
    fn encode_frame_produces_a_jpeg() {
        let pixels = vec![128u8; 64 * 32 * 4];
        let jpeg = encode_frame(&pixels, 64, 32, 64 * 4, spa::param::video::VideoFormat::BGRx).expect("encodes");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG start-of-image marker");
    }
}

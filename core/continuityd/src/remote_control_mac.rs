//! macOS remote control: keyboard/mouse injection via `CGEventPost` —
//! extending the exact same trick `media_mac.rs` already uses for media
//! keys to full, arbitrary key codes and mouse events instead of just the
//! three `NX_KEYTYPE_*` constants — and screen capture via
//! `CGDisplayCreateImage` + `NSBitmapImageRep`'s JPEG export.
//!
//! **`CGDisplayCreateImage` is deprecated** (Apple's own guidance points
//! to ScreenCaptureKit instead) but still fully functional as of this
//! writing, and dramatically simpler to get right from Rust: one
//! synchronous C call returning a `CGImage`, versus ScreenCaptureKit's
//! async, delegate-based streaming API, which would need a much larger
//! surface of hand-written Objective-C interop to bind correctly. Given
//! this dev environment's ability to verify behavior only by hand, on
//! this one real Mac, the simpler and more thoroughly-testable API is the
//! safer choice for a first working version — moving to ScreenCaptureKit
//! is a reasonable follow-up once this is proven stable in practice.
//!
//! **Verified on this dev machine**: `print_screen_capture_info` confirms
//! a real JPEG comes back (327KB at this display's resolution) — capture
//! works end to end. Input injection is **partially** verified: this dev
//! machine's `cargo test` binary isn't Accessibility-trusted (`AXIsProcessTrusted`
//! returns `false` — the same permission gate `media_mac.rs`'s media keys
//! already document, TCC tracks trust per code signature and a `cargo
//! test` binary's hash changes on every rebuild), which can't be granted
//! without a human clicking through System Settings — not something this
//! environment can do non-interactively. `verify_mouse_move_reaches_real_cursor_position`
//! confirmed that gate is real and not just theoretical: with the binary
//! untrusted, a posted `MouseMove` event constructs and posts without
//! error, but the real system cursor doesn't move at all — exactly the
//! silent-no-op shape already described for media keys. The event
//! construction and coordinate-scaling code itself is unverified beyond
//! compiling and passing review, pending a trusted binary to actually
//! confirm clicks/keystrokes land where intended.

use continuity_daemon::RemoteControlHost;
use continuity_proto::{InputEventKind, MouseButton as ProtoMouseButton};
use objc2::AnyThread;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGDisplayBounds, CGDisplayPixelsHigh, CGDisplayPixelsWide,
    CGEvent, CGEventTapLocation, CGEventType, CGImage, CGImageAlphaInfo, CGMainDisplayID, CGMouseButton, CGPreflightScreenCaptureAccess,
    CGRequestScreenCaptureAccess, CGScrollEventUnit,
};
use objc2_foundation::NSDictionary;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;
use tokio::sync::mpsc;

// Raw IOKit power-management calls — not bound by any `objc2-*` crate
// (IOKit predates the modern Objective-C-interop crates and is plain C),
// same pattern as `media_mac.rs`'s hand-declared `AXIsProcessTrusted`.
// Holding a `PreventUserIdleDisplaySleep` assertion for the duration of
// a session is the standard, documented technique screen-sharing apps
// (Zoom, TeamViewer, etc.) use to keep the display actively updating —
// without it, macOS's own power management can throttle/pause display
// refresh once there's no *local* physical input for a while, and a
// remote controller's synthetic `CGEventPost` input apparently doesn't
// count as activity for this purpose the same way real hardware input
// does: reported as the screen "freezing" until the physical mouse is
// touched, which then un-freezes it — exactly the shape idle display
// power management would produce.
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: core_foundation::string::CFStringRef,
        assertion_level: u32,
        assertion_name: core_foundation::string::CFStringRef,
        assertion_id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

/// Mirrors `media_mac::ensure_accessibility_trust` exactly, just for the
/// Screen Recording permission `capture_jpeg` needs instead of the
/// Accessibility one `inject_event` needs. Without this, nothing ever
/// asked the OS for the permission at all: `CGDisplayCreateImage` doesn't
/// fail outright when it's missing, it silently composites a *degraded*
/// image instead (desktop wallpaper and this process's own windows —
/// which is why the tray menu itself was visible — but every other app's
/// window content blanked out), which is a much more confusing failure
/// mode than an outright capture error would have been: capture appeared
/// to "work" (real JPEG bytes, real dimensions) while showing nothing
/// useful. `CGRequestScreenCaptureAccess` is the documented way to
/// actively trigger the system's one-time permission prompt; once it's
/// been answered (either way), calling it again is a silent no-op, so
/// this only really does anything the first time — same shape as the
/// Accessibility flow, which also can't re-prompt after an initial denial
/// and falls back to pointing the user at System Settings instead.
fn ensure_screen_recording_trust() {
    static CHECKED: Once = Once::new();
    CHECKED.call_once(|| {
        if CGPreflightScreenCaptureAccess() {
            return;
        }
        // Triggers the native one-time system prompt if this is the
        // first time this app's ever asked; a no-op returning `false`
        // if the user already denied it previously.
        if CGRequestScreenCaptureAccess() {
            return;
        }
        tracing::warn!(
            "Continuity isn't trusted for Screen Recording — remote screen sharing will show a blank desktop (only this app's own windows visible) until it's granted in System Settings > Privacy & Security > Screen Recording"
        );
        if let Err(e) = notify_rust::Notification::new()
            .summary("Continuity")
            .body("Grant Screen Recording permission for remote control to show the real screen, not a blank desktop.")
            .show()
        {
            tracing::debug!("screen recording notification not shown: {e}");
        }
        if let Err(e) =
            std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture").spawn()
        {
            tracing::debug!("couldn't open System Settings: {e}");
        }
    });
}

/// Frames per second for the capture loop — deliberately modest. This is
/// JPEG-over-TCP, not a hardware-encoded video codec (see the crate-level
/// doc comment in `continuity-net::framing` for why frames go out as
/// whole raw-bytes messages rather than a real codec's delta frames), so
/// every frame is a full, independent screenshot; a higher rate multiplies
/// both CPU cost and bandwidth for a "low latency" feature where the
/// bigger win is *responsiveness of input*, not silky-smooth video.
const CAPTURE_FPS: u64 = 10;
/// JPEG quality, 0.0–1.0 — well below "nice photo" quality on purpose:
/// this is re-encoded and sent many times a second, not saved once, so
/// file size (and the encode time that scales with it) matters far more
/// than per-frame fidelity.
const JPEG_QUALITY: f64 = 0.4;

pub struct MacRemoteControlHost {
    capturing: Arc<AtomicBool>,
    /// The active `IOPMAssertionID`, if a session is currently holding
    /// one — released in `stop_capture`. `None` both before the first
    /// session and after it ends.
    display_sleep_assertion: Mutex<Option<u32>>,
    /// Normalized position (matching `InputEventKind::MouseMove`'s own
    /// 0.0..=1.0, top-left-origin convention) of the most recently
    /// injected mouse position — `capture_jpeg` reads this to composite
    /// a cursor marker into the captured frame, since `CGDisplayCreateImage`
    /// has no way to include the real system cursor at all (unlike the
    /// newer ScreenCaptureKit framework, which isn't used here — see this
    /// module's top-level doc comment for why). `None` until the first
    /// `MouseMove` of a session arrives, so nothing's drawn before there's
    /// a real position to draw.
    last_cursor: Arc<Mutex<Option<(f64, f64)>>>,
}

impl MacRemoteControlHost {
    pub fn new() -> Self {
        Self {
            capturing: Arc::new(AtomicBool::new(false)),
            display_sleep_assertion: Mutex::new(None),
            last_cursor: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for MacRemoteControlHost {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteControlHost for MacRemoteControlHost {
    fn inject(&self, event: InputEventKind) {
        if let InputEventKind::MouseMove { x, y } = event {
            *self.last_cursor.lock().unwrap() = Some((x, y));
        }
        inject_event(event);
    }

    fn start_capture(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        ensure_screen_recording_trust();
        let display_id = CGMainDisplayID();
        // Fresh session, no move injected yet — otherwise a marker from
        // the *previous* session would flash onto the very first frame
        // of this one at wherever the old session's cursor last was.
        *self.last_cursor.lock().unwrap() = None;
        // Probed once, synchronously, before spinning up the capture
        // thread at all — `CGDisplayCreateImage` returning `None` means
        // no Screen Recording permission (macOS silently returns null
        // rather than any kind of error the caller can inspect further),
        // and there's no point starting a whole thread just to have its
        // very first frame fail the same way.
        if capture_jpeg(display_id, None).is_none() {
            tracing::warn!(
                "screen capture failed — Screen Recording permission likely not granted (System Settings > Privacy & Security > Screen Recording)"
            );
            return None;
        }

        *self.display_sleep_assertion.lock().unwrap() = create_display_sleep_assertion();

        self.capturing.store(true, Ordering::Relaxed);
        let capturing = self.capturing.clone();
        let last_cursor = self.last_cursor.clone();
        // Bounded to a couple of frames' worth, not unbounded — a
        // receiver that falls behind (slow network, the controlling
        // side backgrounded, whatever) should never accumulate an
        // unbounded backlog of frames that are stale by the time they'd
        // finally be sent; `try_send` below drops instead once full.
        let (tx, rx) = mpsc::channel(2);
        // A real OS thread, not `spawn_blocking` — this runs for the
        // entire lifetime of a session (potentially minutes), not a
        // single bounded call the way the media watchers' occasional
        // blocking reads are; tokio's blocking pool is sized and reused
        // for short-lived work, not a good fit for something this
        // long-running. Ends on its own once either `capturing` flips to
        // `false` (`stop_capture`) or `tx.try_send` starts failing
        // because `rx` was dropped (the receiving end gave up).
        std::thread::spawn(move || {
            let target_interval = Duration::from_millis(1000 / CAPTURE_FPS);
            while capturing.load(Ordering::Relaxed) {
                let started_at = std::time::Instant::now();
                let cursor = *last_cursor.lock().unwrap();
                if let Some(jpeg) = capture_jpeg(display_id, cursor) {
                    // Deliberately `try_send`, not a blocking send — a
                    // full channel means the consumer's behind; dropping
                    // this frame and trying again next tick keeps
                    // capture itself running at a steady rate instead of
                    // backing up, and an old frame wouldn't have been
                    // worth delivering late regardless.
                    let _ = tx.try_send(jpeg);
                } else {
                    tracing::debug!("a capture attempt failed mid-session (permission revoked, or display changed?)");
                }
                let elapsed = started_at.elapsed();
                if elapsed < target_interval {
                    std::thread::sleep(target_interval - elapsed);
                }
            }
        });
        Some(rx)
    }

    fn stop_capture(&self) {
        self.capturing.store(false, Ordering::Relaxed);
        if let Some(id) = self.display_sleep_assertion.lock().unwrap().take() {
            unsafe {
                IOPMAssertionRelease(id);
            }
        }
    }
}

/// Holds macOS's display awake/refreshing for the duration of a session —
/// see this module's `IOPMAssertionCreateWithName` doc comment above for
/// why. Logged, not treated as fatal, if it fails: worst case the
/// original freezing-when-idle behavior returns, not a broken session.
fn create_display_sleep_assertion() -> Option<u32> {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;

    let assertion_type = CFString::new("PreventUserIdleDisplaySleep");
    let assertion_name = CFString::new("Continuity remote-control session");
    let mut assertion_id: u32 = 0;
    let result = unsafe {
        IOPMAssertionCreateWithName(
            assertion_type.as_concrete_TypeRef(),
            K_IOPM_ASSERTION_LEVEL_ON,
            assertion_name.as_concrete_TypeRef(),
            &mut assertion_id,
        )
    };
    if result == 0 {
        Some(assertion_id)
    } else {
        tracing::debug!("couldn't create display-sleep-prevention assertion (IOReturn {result})");
        None
    }
}

/// One screenshot of `display_id`, JPEG-encoded, with a small cursor
/// marker composited in at `cursor_norm` (if given) — see
/// `MacRemoteControlHost::last_cursor`'s doc comment for why this isn't
/// the real system cursor. `None` covers both real capture failure modes
/// the same way, since `CGDisplayCreateImage` gives no way to tell them
/// apart: no Screen Recording permission, or (much rarer) the display
/// disappearing mid-call (sleep, unplugged external monitor).
fn capture_jpeg(display_id: u32, cursor_norm: Option<(f64, f64)>) -> Option<Vec<u8>> {
    // Deprecated in favor of ScreenCaptureKit — see this module's
    // top-level doc comment for why the simpler, synchronous API is the
    // deliberate choice for now.
    #[allow(deprecated)]
    let image = objc2_core_graphics::CGDisplayCreateImage(display_id)?;

    let image = match cursor_norm {
        Some((nx, ny)) => composite_cursor_marker(&image, nx, ny).unwrap_or(image),
        None => image,
    };

    let bitmap = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &image);
    let quality = objc2_foundation::NSNumber::new_f64(JPEG_QUALITY);
    let compression_key = unsafe { objc2_app_kit::NSImageCompressionFactor };
    let properties: objc2::rc::Retained<NSDictionary<objc2_app_kit::NSBitmapImageRepPropertyKey, objc2::runtime::AnyObject>> =
        NSDictionary::from_slices(&[compression_key], &[&*quality]);
    let data = unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::JPEG, &properties) }?;
    Some(data.to_vec())
}

/// Draws a small filled circle onto a copy of `image` at `(nx, ny)`
/// (normalized, top-left-origin, matching `InputEventKind::MouseMove`'s
/// own convention — flipped to Core Graphics' bottom-left-origin
/// coordinate space for the actual drawing). Standard
/// draw-into-a-bitmap-context-then-read-the-image-back-out pattern:
/// none of this needs to know `NSBitmapImageRep`'s internal pixel layout,
/// unlike poking the raw bitmap buffer directly would.
fn composite_cursor_marker(image: &CGImage, nx: f64, ny: f64) -> Option<CFRetained<CGImage>> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    if width == 0 || height == 0 {
        return None;
    }
    let color_space = CGColorSpace::new_device_rgb()?;
    // SAFETY: `data` is null, so CG allocates and owns the backing buffer
    // itself — nothing here holds a raw pointer into it.
    let context = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            width,
            height,
            8,
            0,
            Some(&color_space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }?;
    let full_rect = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: width as f64, height: height as f64 } };
    CGContext::draw_image(Some(&context), full_rect, Some(image));

    // ~0.6% of the smaller screen dimension — big enough to actually see,
    // small enough not to obscure whatever's under it.
    let radius = (width.min(height) as f64) * 0.006;
    let cx = nx * width as f64;
    // Core Graphics' default (non-flipped) coordinate space on macOS has
    // its origin at the *bottom*-left with Y increasing upward, while
    // `ny` follows the touch/screen convention of top-left-origin with Y
    // increasing downward — without this flip the marker would appear
    // mirrored vertically from where the cursor actually is.
    let cy = (1.0 - ny) * height as f64;
    let dot_rect =
        CGRect { origin: CGPoint { x: cx - radius, y: cy - radius }, size: CGSize { width: radius * 2.0, height: radius * 2.0 } };

    CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 0.9);
    CGContext::fill_ellipse_in_rect(Some(&context), dot_rect);
    CGContext::set_rgb_stroke_color(Some(&context), 0.0, 0.0, 0.0, 0.6);
    CGContext::set_line_width(Some(&context), (radius * 0.25).max(1.0));
    CGContext::stroke_ellipse_in_rect(Some(&context), dot_rect);

    CGBitmapContextCreateImage(Some(&context))
}

/// Injects one input event via `CGEventPost`, the same session-level
/// event-posting primitive `media_mac.rs`'s `post_media_key` already
/// uses for media keys — just building a full keyboard/mouse/scroll
/// event instead of one of the three fixed `NX_KEYTYPE_*` constants.
/// Gated behind the same Accessibility permission as those media-key
/// events (`AXIsProcessTrusted`) — calls the shared
/// `media_mac::ensure_accessibility_trust` rather than assuming media
/// control already triggered it: a user who only ever tries remote
/// control and never touches media keys would otherwise hit a silent,
/// unexplained no-op on every injected event with no path to discovering
/// why. **Confirmed by hand this gate is real and not just documented**:
/// with this dev machine's test binary untrusted, a `MouseMove` event
/// posted correctly but the real system cursor never moved at all — see
/// `verify_mouse_move_reaches_real_cursor_position` below.
fn inject_event(event: InputEventKind) {
    crate::media_mac::ensure_accessibility_trust();
    let display_id = CGMainDisplayID();
    let bounds = CGDisplayBounds(display_id);
    let width = CGDisplayPixelsWide(display_id) as f64;
    let height = CGDisplayPixelsHigh(display_id) as f64;

    match event {
        InputEventKind::KeyDown { code } => post(CGEvent::new_keyboard_event(None, code as u16, true)),
        InputEventKind::KeyUp { code } => post(CGEvent::new_keyboard_event(None, code as u16, false)),
        InputEventKind::MouseMove { x, y } => {
            let point = CGPoint { x: bounds.origin.x + x * width, y: bounds.origin.y + y * height };
            post(CGEvent::new_mouse_event(None, CGEventType::MouseMoved, point, CGMouseButton::Left));
        }
        InputEventKind::MouseButton { button, down } => {
            let point = current_mouse_location();
            let (event_type, cg_button) = match (button, down) {
                (ProtoMouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                (ProtoMouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                (ProtoMouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
                (ProtoMouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
                (ProtoMouseButton::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
                (ProtoMouseButton::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
            };
            post(CGEvent::new_mouse_event(None, event_type, point, cg_button));
        }
        InputEventKind::MouseScroll { delta_x, delta_y } => {
            // Pixel units, not "line" units — the controller's delta is
            // already a continuous, device-reported scroll amount (from
            // a touch gesture or a mouse wheel), closer in spirit to
            // pixel-precision scrolling than a fixed number of
            // discrete lines.
            post(CGEvent::new_scroll_wheel_event2(
                None,
                CGScrollEventUnit::Pixel,
                2,
                delta_y as i32,
                delta_x as i32,
                0,
            ));
        }
    }
}

fn post(event: Option<objc2_core_foundation::CFRetained<CGEvent>>) {
    let Some(event) = event else {
        tracing::debug!("couldn't construct synthetic input event");
        return;
    };
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
}

/// `CGEventCreateMouseEvent` needs a position even for a button-only
/// (not move) event — reads the cursor's actual current location so a
/// click lands wherever the most recent `MouseMove` already put it,
/// rather than needing every click bundled with redundant coordinates.
fn current_mouse_location() -> CGPoint {
    // `CGEventCreate(NULL)` followed by `CGEventGetLocation` is the
    // standard, if slightly indirect, way to query the current cursor
    // position via Core Graphics — a freshly created event (never
    // posted anywhere) already reflects the real input devices' current
    // state, there's no separate "just tell me where the mouse is" call.
    CGEvent::new(None).map(|e| CGEvent::location(Some(&e))).unwrap_or_default()
}

#[cfg(test)]
mod manual_verification {
    // Not real tests — run with `cargo test -p continuityd <name> --
    // --ignored --nocapture`. `print_screen_capture_info` just needs
    // Screen Recording permission; the input ones need a focused text
    // field/window to type/click into, and Accessibility permission
    // (see `media_mac::ensure_accessibility_trust` — this file doesn't
    // call it itself, see `inject_event`'s doc comment for why).
    use super::*;

    #[test]
    #[ignore]
    fn print_screen_capture_info() {
        let display_id = CGMainDisplayID();
        match capture_jpeg(display_id, None) {
            Some(jpeg) => println!("captured {} JPEG bytes", jpeg.len()),
            None => println!("capture failed — check Screen Recording permission"),
        }
    }

    /// Visual verification for `composite_cursor_marker`: captures with a
    /// marker at a known, deliberately off-center position and writes the
    /// JPEG to disk so it can be eyeballed directly — the thing this
    /// actually needs to prove (marker present, correctly oriented, not
    /// mirrored) isn't something a numeric assertion can check on its
    /// own the way `verify_mouse_move_reaches_real_cursor_position` can
    /// for cursor movement.
    #[test]
    #[ignore]
    fn print_cursor_marker_capture_info() {
        let display_id = CGMainDisplayID();
        // (0.15, 0.15): near the top-left corner — if Y ends up flipped,
        // this lands near the bottom-left instead, which is easy to tell
        // apart at a glance.
        match capture_jpeg(display_id, Some((0.15, 0.15))) {
            Some(jpeg) => {
                let path = std::env::var("CURSOR_MARKER_OUT").unwrap_or_else(|_| "/tmp/continuity_cursor_marker.jpg".to_string());
                std::fs::write(&path, &jpeg).unwrap();
                println!("captured {} JPEG bytes with cursor marker at (0.15, 0.15), wrote {path}", jpeg.len());
            }
            None => println!("capture failed — check Screen Recording permission"),
        }
    }

    #[test]
    #[ignore]
    fn send_test_keystroke() {
        // 'A' key, per-platform virtual keycode (kVK_ANSI_A = 0x00) —
        // types into whatever's focused when this runs.
        inject_event(InputEventKind::KeyDown { code: 0x00 });
        std::thread::sleep(Duration::from_millis(50));
        inject_event(InputEventKind::KeyUp { code: 0x00 });
    }

    #[test]
    #[ignore]
    fn send_test_click() {
        inject_event(InputEventKind::MouseMove { x: 0.5, y: 0.5 });
        std::thread::sleep(Duration::from_millis(50));
        inject_event(InputEventKind::MouseButton { button: ProtoMouseButton::Left, down: true });
        std::thread::sleep(Duration::from_millis(50));
        inject_event(InputEventKind::MouseButton { button: ProtoMouseButton::Left, down: false });
    }

    /// Self-contained — doesn't depend on any app being focused. Moves
    /// the cursor to two different normalized positions and reads the
    /// *real* system cursor location back after each (via the same
    /// `current_mouse_location` helper `inject_event` itself uses for
    /// click coordinates) to confirm `CGEventPost` is actually taking
    /// effect on live system state, not just constructing well-formed
    /// events that go nowhere.
    #[test]
    #[ignore]
    fn verify_mouse_move_reaches_real_cursor_position() {
        let display_id = CGMainDisplayID();
        let bounds = CGDisplayBounds(display_id);
        let width = CGDisplayPixelsWide(display_id) as f64;
        let height = CGDisplayPixelsHigh(display_id) as f64;

        for (x, y) in [(0.25, 0.25), (0.75, 0.6)] {
            inject_event(InputEventKind::MouseMove { x, y });
            std::thread::sleep(Duration::from_millis(100));
            let expected = CGPoint { x: bounds.origin.x + x * width, y: bounds.origin.y + y * height };
            let actual = current_mouse_location();
            println!("requested ({x}, {y}) normalized -> expected {expected:?}, real cursor now at {actual:?}");
            assert!((actual.x - expected.x).abs() < 1.0, "cursor x didn't move to the requested position");
            assert!((actual.y - expected.y).abs() < 1.0, "cursor y didn't move to the requested position");
        }
    }
}

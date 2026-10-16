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

use crate::frame_encoding::{self, LastFrame, MAX_ENCODED_WIDTH};
use continuity_daemon::RemoteControlHost;
use continuity_proto::{InputEventKind, MouseButton as ProtoMouseButton};
use objc2::AnyThread;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSEvent};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGDisplayBounds, CGDisplayPixelsHigh, CGDisplayPixelsWide,
    CGInterpolationQuality,
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType, CGImage, CGImageAlphaInfo, CGMainDisplayID, CGMouseButton,
    CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess, CGScrollEventUnit,
};
use objc2_foundation::NSDictionary;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};
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

/// JPEG quality for `NSBitmapImageRep`, 0.0–1.0 — the shared
/// `frame_encoding::JPEG_QUALITY`, which is out of 100. Frame rate and
/// size limits are shared the same way (`frame_encoding`).
const JPEG_QUALITY: f64 = frame_encoding::JPEG_QUALITY as f64 / 100.0;

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
    /// The keys, modifiers and mouse buttons the controlling device
    /// currently holds down here — see `HeldKeys` and `MouseState`.
    held: Mutex<HeldInput>,
    /// Whether frames show the pointer marker — see
    /// `RemoteControlHost::set_pointer_in_frames`.
    pointer_in_frames: Arc<AtomicBool>,
}

impl MacRemoteControlHost {
    pub fn new() -> Self {
        Self {
            capturing: Arc::new(AtomicBool::new(false)),
            display_sleep_assertion: Mutex::new(None),
            last_cursor: Arc::new(Mutex::new(None)),
            held: Mutex::new(HeldInput::default()),
            pointer_in_frames: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Lets go of everything the controlling device left held down. A
    /// session can end without its key-ups ever arriving (the other
    /// computer slept, the network dropped), and an injected modifier left
    /// down here would go on modifying the local person's own typing — a
    /// button left down, dragging whatever they then move the mouse over.
    /// Buttons first, so a Shift-drag ends as one.
    fn release_held_input(&self) {
        let mut held = self.held.lock().unwrap();
        let buttons = held.mouse.release_all();
        if !buttons.is_empty() {
            let point = current_mouse_location();
            for button in buttons {
                let (event_type, cg_button) = button_event(button, false);
                post(CGEvent::new_mouse_event(None, event_type, point, cg_button), held.keys.flags());
            }
        }
        for release in &held.keys.release_all() {
            post_key(release);
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
        inject_event(event, &mut self.held.lock().unwrap());
    }

    fn start_capture(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        // Nothing should be held at the start of a session; if a previous
        // one somehow left something, let go of it before it can combine
        // with this session's first keys.
        self.release_held_input();
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
        let pointer = self.pointer_in_frames.clone();
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
            let target_interval = Duration::from_millis(1000 / frame_encoding::FRAMES_PER_SECOND);
            let mut last = LastFrame::default();
            while capturing.load(Ordering::Relaxed) {
                let started_at = std::time::Instant::now();
                let cursor = if pointer.load(Ordering::Relaxed) { *last_cursor.lock().unwrap() } else { None };
                if let Some(jpeg) = capture_jpeg(display_id, cursor) {
                    // Deliberately `try_send`, not a blocking send — a
                    // full channel means the consumer's behind; dropping
                    // this frame and trying again next tick keeps
                    // capture itself running at a steady rate instead of
                    // backing up, and an old frame wouldn't have been
                    // worth delivering late regardless. An unchanged
                    // screen isn't sent again (`LastFrame`).
                    if last.is_new(&jpeg) && tx.try_send(jpeg.clone()).is_ok() {
                        last.sent(jpeg);
                    }
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

    fn set_pointer_in_frames(&self, include: bool) {
        self.pointer_in_frames.store(include, Ordering::Relaxed);
    }

    fn stop_capture(&self) {
        self.capturing.store(false, Ordering::Relaxed);
        self.release_held_input();
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

    let image = render_frame(&image, cursor_norm).unwrap_or(image);

    let bitmap = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &image);
    let quality = objc2_foundation::NSNumber::new_f64(JPEG_QUALITY);
    let compression_key = unsafe { objc2_app_kit::NSImageCompressionFactor };
    let properties: objc2::rc::Retained<NSDictionary<objc2_app_kit::NSBitmapImageRepPropertyKey, objc2::runtime::AnyObject>> =
        NSDictionary::from_slices(&[compression_key], &[&*quality]);
    let data = unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::JPEG, &properties) }?;
    Some(data.to_vec())
}

/// The size `width` × `height` is encoded at: divided by the smallest whole
/// factor that brings it within `MAX_ENCODED_WIDTH` (2880 → 1440, 5120 →
/// 1707 — a whole factor keeps a Retina frame as sharp as the screen's own
/// point grid).
fn encoded_size(width: usize, height: usize) -> (usize, usize) {
    let factor = width.div_ceil(MAX_ENCODED_WIDTH).max(1);
    (width.div_ceil(factor).max(1), height.div_ceil(factor).max(1))
}

/// A copy of `image` at its encoded size (`encoded_size`), with, if
/// there's a position, a small filled circle drawn at `cursor` (normalized,
/// top-left-origin, matching `InputEventKind::MouseMove`'s own convention —
/// flipped to Core Graphics' bottom-left-origin coordinate space for the
/// actual drawing). `None` when there's nothing to change. Standard
/// draw-into-a-bitmap-context-then-read-the-image-back-out pattern: none of
/// this needs to know `NSBitmapImageRep`'s internal pixel layout, unlike
/// poking the raw bitmap buffer directly would.
fn render_frame(image: &CGImage, cursor: Option<(f64, f64)>) -> Option<CFRetained<CGImage>> {
    let source = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    if source.0 == 0 || source.1 == 0 {
        return None;
    }
    let (width, height) = encoded_size(source.0, source.1);
    if (width, height) == source && cursor.is_none() {
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
    CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::Medium);
    CGContext::draw_image(Some(&context), full_rect, Some(image));
    let Some((nx, ny)) = cursor else {
        return CGBitmapContextCreateImage(Some(&context));
    };

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
///
/// Every event carries the modifiers `held` says are down — see `HeldKeys`
/// for why that has to be done by hand — and mouse events the button
/// state `MouseState` keeps.
fn inject_event(event: InputEventKind, held: &mut HeldInput) {
    crate::media_mac::ensure_accessibility_trust();
    let display_id = CGMainDisplayID();
    let bounds = CGDisplayBounds(display_id);
    let width = CGDisplayPixelsWide(display_id) as f64;
    let height = CGDisplayPixelsHigh(display_id) as f64;
    let HeldInput { keys: held, mouse } = held;
    let flags = held.flags();

    match event {
        // Codes past `u16` can't be a key; truncating one could alias a
        // real key, so it's dropped instead.
        InputEventKind::KeyDown { code } => match u16::try_from(code).ok().and_then(|code| held.key_down(code)) {
            Some(press) => post_key(&press),
            None => tracing::trace!("key down {code:#x}: nothing to post"),
        },
        InputEventKind::KeyUp { code } => match u16::try_from(code).ok().and_then(|code| held.key_up(code)) {
            Some(release) => post_key(&release),
            None => tracing::trace!("key up {code:#x}: nothing to post"),
        },
        InputEventKind::MouseMove { x, y } => {
            let point = CGPoint { x: bounds.origin.x + x * width, y: bounds.origin.y + y * height };
            let (event_type, cg_button) = mouse.move_event();
            post(CGEvent::new_mouse_event(None, event_type, point, cg_button), flags);
        }
        InputEventKind::MouseButton { button, down } => {
            let point = current_mouse_location();
            let (event_type, cg_button) = button_event(button, down);
            let clicks = mouse.button(button, down, (point.x, point.y), Instant::now(), double_click_interval());
            let event = CGEvent::new_mouse_event(None, event_type, point, cg_button);
            if let Some(event) = &event {
                CGEvent::set_integer_value_field(Some(event), CGEventField::MouseEventClickState, clicks);
            }
            post(event, flags);
        }
        InputEventKind::MouseScroll { delta_x, delta_y } => {
            // Pixel units, not "line" units — the controller's delta is
            // already a continuous, device-reported scroll amount (from
            // a touch gesture or a mouse wheel), closer in spirit to
            // pixel-precision scrolling than a fixed number of
            // discrete lines.
            post(CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, delta_y as i32, delta_x as i32, 0), flags);
        }
    }
}

/// Posts `event` with `held` as its modifier flags (see `HeldKeys`).
fn post(event: Option<objc2_core_foundation::CFRetained<CGEvent>>, held: u64) {
    let Some(event) = event else {
        tracing::debug!("couldn't construct synthetic input event");
        return;
    };
    // Only the modifier bits this file manages are replaced; anything
    // else the system set on the new event (Caps Lock's state, the
    // non-coalesced bit) is left as it was.
    let flags = CGEvent::flags(Some(&event)).0;
    CGEvent::set_flags(Some(&event), CGEventFlags((flags & !MANAGED_MODIFIER_FLAGS) | held));
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
}

/// Posts one key event worked out by `HeldKeys`.
fn post_key(key: &KeyPost) {
    let event = CGEvent::new_keyboard_event(None, key.code, key.down);
    if let Some(event) = &event {
        match key.kind {
            // What a real keyboard sends for a modifier: not a key down or
            // up but a "flags changed", carrying the new set of modifiers.
            KeyPostKind::FlagsChanged => CGEvent::set_type(Some(event), CGEventType::FlagsChanged),
            KeyPostKind::Key { autorepeat: true } => {
                CGEvent::set_integer_value_field(Some(event), CGEventField::KeyboardEventAutorepeat, 1)
            }
            KeyPostKind::Key { autorepeat: false } => {}
        }
    }
    post(event, key.flags);
}

// Carbon's `kVK_*` codes for the modifier keys (`Events.h`).
const KVK_COMMAND: u16 = 0x37;
const KVK_RIGHT_COMMAND: u16 = 0x36;
const KVK_SHIFT: u16 = 0x38;
const KVK_RIGHT_SHIFT: u16 = 0x3C;
const KVK_OPTION: u16 = 0x3A;
const KVK_RIGHT_OPTION: u16 = 0x3D;
const KVK_CONTROL: u16 = 0x3B;
const KVK_RIGHT_CONTROL: u16 = 0x3E;

// The left/right-specific halves of the modifier flags (IOKit's
// `IOLLEvent.h`, `NX_DEVICE*KEYMASK`), which real keyboard events carry
// alongside the device-independent `CGEventFlags` masks and which apps
// that tell left from right look at.
const DEVICE_LEFT_CONTROL: u64 = 0x0000_0001;
const DEVICE_LEFT_SHIFT: u64 = 0x0000_0002;
const DEVICE_RIGHT_SHIFT: u64 = 0x0000_0004;
const DEVICE_LEFT_COMMAND: u64 = 0x0000_0008;
const DEVICE_RIGHT_COMMAND: u64 = 0x0000_0010;
const DEVICE_LEFT_OPTION: u64 = 0x0000_0020;
const DEVICE_RIGHT_OPTION: u64 = 0x0000_0040;
const DEVICE_RIGHT_CONTROL: u64 = 0x0000_2000;

/// Every flag bit `modifier_flags` can set — the part of an injected
/// event's flags this file owns.
const MANAGED_MODIFIER_FLAGS: u64 = CGEventFlags::MaskShift.0
    | CGEventFlags::MaskControl.0
    | CGEventFlags::MaskAlternate.0
    | CGEventFlags::MaskCommand.0
    | DEVICE_LEFT_CONTROL
    | DEVICE_LEFT_SHIFT
    | DEVICE_RIGHT_SHIFT
    | DEVICE_LEFT_COMMAND
    | DEVICE_RIGHT_COMMAND
    | DEVICE_LEFT_OPTION
    | DEVICE_RIGHT_OPTION
    | DEVICE_RIGHT_CONTROL;

/// The flags a modifier key sets while it's held; `None` for any other key.
fn modifier_flags(code: u16) -> Option<u64> {
    Some(match code {
        KVK_COMMAND => CGEventFlags::MaskCommand.0 | DEVICE_LEFT_COMMAND,
        KVK_RIGHT_COMMAND => CGEventFlags::MaskCommand.0 | DEVICE_RIGHT_COMMAND,
        KVK_SHIFT => CGEventFlags::MaskShift.0 | DEVICE_LEFT_SHIFT,
        KVK_RIGHT_SHIFT => CGEventFlags::MaskShift.0 | DEVICE_RIGHT_SHIFT,
        KVK_OPTION => CGEventFlags::MaskAlternate.0 | DEVICE_LEFT_OPTION,
        KVK_RIGHT_OPTION => CGEventFlags::MaskAlternate.0 | DEVICE_RIGHT_OPTION,
        KVK_CONTROL => CGEventFlags::MaskControl.0 | DEVICE_LEFT_CONTROL,
        KVK_RIGHT_CONTROL => CGEventFlags::MaskControl.0 | DEVICE_RIGHT_CONTROL,
        _ => return None,
    })
}

/// What the controlling device is holding down on this Mac.
///
/// A key event posted with `CGEventPost` can't be relied on to pick up
/// modifiers from earlier posted events — posting Command down and then C
/// is widely reported to type a plain "c" in many apps rather than
/// trigger Command-C. A real keyboard's events each carry the full set of
/// modifiers held at that moment, so this tracks them from the modifier
/// keys' own downs and ups and stamps them onto every key and mouse event
/// (Shift-click, Command-click too). The modifier keys themselves go out
/// as "flags changed" events, like a real keyboard's. Codes from the
/// Android app (letters, digits, space, Return, Delete — never a modifier)
/// pass through as before, with no modifier flags.
#[derive(Default)]
struct HeldKeys {
    /// Modifier key codes held, in press order.
    modifiers: Vec<u16>,
    /// Other key codes held — a second down for one of these is the
    /// controlling side's auto-repeat.
    keys: Vec<u16>,
}

#[derive(Debug, PartialEq)]
struct KeyPost {
    code: u16,
    down: bool,
    kind: KeyPostKind,
    /// The modifier flags to post with — for a modifier key, the set after
    /// this press or release, as a "flags changed" event reports it.
    flags: u64,
}

#[derive(Debug, PartialEq)]
enum KeyPostKind {
    FlagsChanged,
    Key { autorepeat: bool },
}

impl HeldKeys {
    fn flags(&self) -> u64 {
        self.modifiers.iter().filter_map(|&code| modifier_flags(code)).fold(0, |all, flags| all | flags)
    }

    fn key_down(&mut self, code: u16) -> Option<KeyPost> {
        if modifier_flags(code).is_some() {
            // Modifiers don't repeat on a real keyboard.
            if self.modifiers.contains(&code) {
                return None;
            }
            self.modifiers.push(code);
            return Some(KeyPost { code, down: true, kind: KeyPostKind::FlagsChanged, flags: self.flags() });
        }
        let autorepeat = self.keys.contains(&code);
        if !autorepeat {
            self.keys.push(code);
        }
        Some(KeyPost { code, down: true, kind: KeyPostKind::Key { autorepeat }, flags: self.flags() })
    }

    fn key_up(&mut self, code: u16) -> Option<KeyPost> {
        if modifier_flags(code).is_some() {
            let index = self.modifiers.iter().position(|&held| held == code)?;
            self.modifiers.remove(index);
            return Some(KeyPost { code, down: false, kind: KeyPostKind::FlagsChanged, flags: self.flags() });
        }
        // Posted even if it wasn't held: a stray key-up is harmless, a key
        // left down isn't.
        self.keys.retain(|&held| held != code);
        Some(KeyPost { code, down: false, kind: KeyPostKind::Key { autorepeat: false }, flags: self.flags() })
    }

    /// Releases for everything held — keys first, then modifiers, each
    /// last-pressed first.
    fn release_all(&mut self) -> Vec<KeyPost> {
        let keys: Vec<u16> = self.keys.iter().rev().chain(self.modifiers.iter().rev()).copied().collect();
        keys.into_iter().filter_map(|code| self.key_up(code)).collect()
    }
}

/// What the controlling device holds down on this Mac, keyboard and mouse.
#[derive(Default)]
struct HeldInput {
    keys: HeldKeys,
    mouse: MouseState,
}

fn button_event(button: ProtoMouseButton, down: bool) -> (CGEventType, CGMouseButton) {
    match (button, down) {
        (ProtoMouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
        (ProtoMouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
        (ProtoMouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
        (ProtoMouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
        (ProtoMouseButton::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
        (ProtoMouseButton::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
    }
}

/// The double-click speed set in System Settings.
fn double_click_interval() -> Duration {
    Duration::from_secs_f64(NSEvent::doubleClickInterval().clamp(0.1, 5.0))
}

/// How far apart, in points, two presses can land and still be one
/// double-click — a hand's slight shake.
const CLICK_SLOP: f64 = 4.0;

/// What the controlling device is doing with the mouse on this Mac.
///
/// **Real bugs fixed here**: every move was posted as a plain `MouseMoved`,
/// even with a button held, and macOS only takes the `*MouseDragged` types
/// as a drag — so selecting text, moving a window by its title bar,
/// resizing one or dragging a file did nothing here. And no click carried a
/// click count, so a double-click arrived as two single clicks: files
/// didn't open, words didn't get selected. Buttons still held when a
/// session ends without their releases (the connection dropping mid-drag)
/// are let go too, like `HeldKeys`.
#[derive(Default)]
struct MouseState {
    /// Buttons held, in press order.
    held: Vec<ProtoMouseButton>,
    last_press: Option<LastPress>,
}

struct LastPress {
    button: ProtoMouseButton,
    at: (f64, f64),
    when: Instant,
    clicks: i64,
}

impl MouseState {
    /// What a move is posted as: a drag while a button is held (the left
    /// one's, if it's among them, as a real mouse reports it), a plain
    /// move otherwise.
    fn move_event(&self) -> (CGEventType, CGMouseButton) {
        if self.held.contains(&ProtoMouseButton::Left) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.held.contains(&ProtoMouseButton::Right) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if self.held.contains(&ProtoMouseButton::Middle) {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        }
    }

    /// Records a press or release at `at`, and returns the click count to
    /// post it with: a press of the same button within `interval` of the
    /// last one, and close to it, counts one more (a double-click, then a
    /// triple); a release carries its press's count.
    fn button(&mut self, button: ProtoMouseButton, down: bool, at: (f64, f64), now: Instant, interval: Duration) -> i64 {
        if !down {
            self.held.retain(|&held| held != button);
            return self.last_press.as_ref().filter(|press| press.button == button).map_or(1, |press| press.clicks);
        }
        if !self.held.contains(&button) {
            self.held.push(button);
        }
        let clicks = match &self.last_press {
            Some(press)
                if press.button == button
                    && now.duration_since(press.when) <= interval
                    && (press.at.0 - at.0).abs() <= CLICK_SLOP
                    && (press.at.1 - at.1).abs() <= CLICK_SLOP =>
            {
                press.clicks + 1
            }
            _ => 1,
        };
        self.last_press = Some(LastPress { button, at, when: now, clicks });
        clicks
    }

    /// The buttons still held, last pressed first — to let go of.
    fn release_all(&mut self) -> Vec<ProtoMouseButton> {
        self.last_press = None;
        self.held.drain(..).rev().collect()
    }
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

    /// Visual verification for `render_frame`'s cursor marker: captures with a
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
        let mut held = HeldInput::default();
        inject_event(InputEventKind::KeyDown { code: 0x00 }, &mut held);
        std::thread::sleep(Duration::from_millis(50));
        inject_event(InputEventKind::KeyUp { code: 0x00 }, &mut held);
    }

    /// Shift-A then Command-A: an "A", then everything in the focused
    /// text field selected — proof the modifier flags reach the target.
    #[test]
    #[ignore]
    fn send_test_shortcuts() {
        let mut held = HeldInput::default();
        for modifier in [KVK_SHIFT, KVK_COMMAND] {
            for event in [
                InputEventKind::KeyDown { code: modifier.into() },
                InputEventKind::KeyDown { code: 0x00 },
                InputEventKind::KeyUp { code: 0x00 },
                InputEventKind::KeyUp { code: modifier.into() },
            ] {
                inject_event(event, &mut held);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    #[test]
    #[ignore]
    fn send_test_click() {
        let mut held = HeldInput::default();
        inject_event(InputEventKind::MouseMove { x: 0.5, y: 0.5 }, &mut held);
        std::thread::sleep(Duration::from_millis(50));
        inject_event(InputEventKind::MouseButton { button: ProtoMouseButton::Left, down: true }, &mut held);
        std::thread::sleep(Duration::from_millis(50));
        inject_event(InputEventKind::MouseButton { button: ProtoMouseButton::Left, down: false }, &mut held);
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
            inject_event(InputEventKind::MouseMove { x, y }, &mut HeldInput::default());
            std::thread::sleep(Duration::from_millis(100));
            let expected = CGPoint { x: bounds.origin.x + x * width, y: bounds.origin.y + y * height };
            let actual = current_mouse_location();
            println!("requested ({x}, {y}) normalized -> expected {expected:?}, real cursor now at {actual:?}");
            assert!((actual.x - expected.x).abs() < 1.0, "cursor x didn't move to the requested position");
            assert!((actual.y - expected.y).abs() < 1.0, "cursor y didn't move to the requested position");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: u64 = 0x0010_0000 | DEVICE_LEFT_COMMAND;
    const SHIFT: u64 = 0x0002_0000 | DEVICE_LEFT_SHIFT;
    const KVK_ANSI_C: u16 = 0x08;

    fn key(code: u16, down: bool, autorepeat: bool, flags: u64) -> Option<KeyPost> {
        Some(KeyPost { code, down, kind: KeyPostKind::Key { autorepeat }, flags })
    }

    fn modifier(code: u16, down: bool, flags: u64) -> Option<KeyPost> {
        Some(KeyPost { code, down, kind: KeyPostKind::FlagsChanged, flags })
    }

    #[test]
    fn retina_frames_are_encoded_at_a_whole_fraction_of_their_size() {
        assert_eq!(encoded_size(1920, 1080), (1920, 1080), "small enough already");
        assert_eq!(encoded_size(1440, 900), (1440, 900));
        assert_eq!(encoded_size(2880, 1800), (1440, 900), "a 13-inch Retina MacBook");
        assert_eq!(encoded_size(3456, 2234), (1728, 1117), "a 16-inch MacBook Pro");
        assert_eq!(encoded_size(5120, 2880), (1707, 960), "a 5K display");
        assert_eq!(encoded_size(1, 1), (1, 1));
    }

    /// A plain image of `width` × `height`, standing in for a capture.
    fn solid_image(width: usize, height: usize) -> CFRetained<CGImage> {
        let color_space = CGColorSpace::new_device_rgb().unwrap();
        let context = unsafe {
            CGBitmapContextCreate(std::ptr::null_mut(), width, height, 8, 0, Some(&color_space), CGImageAlphaInfo::PremultipliedLast.0)
        }
        .unwrap();
        CGContext::set_rgb_fill_color(Some(&context), 0.2, 0.4, 0.6, 1.0);
        CGContext::fill_rect(
            Some(&context),
            CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: width as f64, height: height as f64 } },
        );
        CGBitmapContextCreateImage(Some(&context)).unwrap()
    }

    #[test]
    fn a_retina_capture_is_drawn_at_its_encoded_size_with_the_cursor() {
        let frame = render_frame(&solid_image(2880, 1800), Some((0.5, 0.5))).expect("scaled");
        assert_eq!((CGImage::width(Some(&frame)), CGImage::height(Some(&frame))), (1440, 900));
        let frame = render_frame(&solid_image(2880, 1800), None).expect("scaled even with no cursor");
        assert_eq!(CGImage::width(Some(&frame)), 1440);
        // Small enough, and no cursor to draw: left as it is.
        assert!(render_frame(&solid_image(1280, 800), None).is_none());
        let frame = render_frame(&solid_image(1280, 800), Some((0.1, 0.9))).expect("cursor drawn");
        assert_eq!(CGImage::width(Some(&frame)), 1280);
    }

    #[test]
    fn a_move_with_a_button_held_is_a_drag_of_that_button() {
        let mut mouse = MouseState::default();
        let (now, interval) = (Instant::now(), Duration::from_millis(500));
        assert_eq!(mouse.move_event(), (CGEventType::MouseMoved, CGMouseButton::Left));
        mouse.button(ProtoMouseButton::Right, true, (0.0, 0.0), now, interval);
        assert_eq!(mouse.move_event(), (CGEventType::RightMouseDragged, CGMouseButton::Right));
        // The left button's drag wins while both are held.
        mouse.button(ProtoMouseButton::Left, true, (0.0, 0.0), now, interval);
        assert_eq!(mouse.move_event(), (CGEventType::LeftMouseDragged, CGMouseButton::Left));
        mouse.button(ProtoMouseButton::Left, false, (0.0, 0.0), now, interval);
        mouse.button(ProtoMouseButton::Right, false, (0.0, 0.0), now, interval);
        mouse.button(ProtoMouseButton::Middle, true, (0.0, 0.0), now, interval);
        assert_eq!(mouse.move_event(), (CGEventType::OtherMouseDragged, CGMouseButton::Center));
        mouse.button(ProtoMouseButton::Middle, false, (0.0, 0.0), now, interval);
        assert_eq!(mouse.move_event(), (CGEventType::MouseMoved, CGMouseButton::Left));
    }

    #[test]
    fn quick_clicks_in_one_place_count_up_and_others_start_again() {
        let mut mouse = MouseState::default();
        let interval = Duration::from_millis(500);
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut click = |button, place, ms| {
            let down = mouse.button(button, true, place, at(ms), interval);
            let up = mouse.button(button, false, place, at(ms + 50), interval);
            assert_eq!(down, up, "a release carries its press's count");
            down
        };
        assert_eq!(click(ProtoMouseButton::Left, (100.0, 100.0), 0), 1);
        assert_eq!(click(ProtoMouseButton::Left, (102.0, 99.0), 200), 2, "a double-click, despite a little shake");
        assert_eq!(click(ProtoMouseButton::Left, (101.0, 100.0), 400), 3, "a triple-click");
        assert_eq!(click(ProtoMouseButton::Left, (101.0, 100.0), 1400), 1, "too slow");
        assert_eq!(click(ProtoMouseButton::Left, (140.0, 100.0), 1500), 1, "too far");
        assert_eq!(click(ProtoMouseButton::Right, (140.0, 100.0), 1600), 1, "another button");
    }

    #[test]
    fn buttons_held_at_the_end_are_let_go_last_pressed_first() {
        let mut mouse = MouseState::default();
        let (now, interval) = (Instant::now(), Duration::from_millis(500));
        mouse.button(ProtoMouseButton::Left, true, (0.0, 0.0), now, interval);
        mouse.button(ProtoMouseButton::Right, true, (0.0, 0.0), now, interval);
        assert_eq!(mouse.release_all(), vec![ProtoMouseButton::Right, ProtoMouseButton::Left]);
        assert!(mouse.release_all().is_empty());
        assert_eq!(mouse.move_event(), (CGEventType::MouseMoved, CGMouseButton::Left));
    }

    #[test]
    fn command_c_carries_the_command_flag() {
        let mut held = HeldKeys::default();
        assert_eq!(held.key_down(KVK_COMMAND), modifier(KVK_COMMAND, true, COMMAND));
        assert_eq!(held.key_down(KVK_ANSI_C), key(KVK_ANSI_C, true, false, COMMAND));
        assert_eq!(held.key_up(KVK_ANSI_C), key(KVK_ANSI_C, false, false, COMMAND));
        // The modifier's own release reports the set *after* it.
        assert_eq!(held.key_up(KVK_COMMAND), modifier(KVK_COMMAND, false, 0));
        assert_eq!(held.key_down(KVK_ANSI_C), key(KVK_ANSI_C, true, false, 0));
    }

    #[test]
    fn modifiers_combine_and_left_right_stay_apart() {
        let mut held = HeldKeys::default();
        held.key_down(KVK_SHIFT);
        assert_eq!(held.key_down(KVK_COMMAND), modifier(KVK_COMMAND, true, COMMAND | SHIFT));
        held.key_down(KVK_RIGHT_COMMAND);
        held.key_up(KVK_COMMAND);
        // Right Command still down: still Command, but only the right one.
        assert_eq!(held.flags(), 0x0010_0000 | DEVICE_RIGHT_COMMAND | SHIFT);
    }

    #[test]
    fn repeats_are_marked_and_modifiers_dont_repeat() {
        let mut held = HeldKeys::default();
        assert_eq!(held.key_down(KVK_ANSI_C), key(KVK_ANSI_C, true, false, 0));
        assert_eq!(held.key_down(KVK_ANSI_C), key(KVK_ANSI_C, true, true, 0));
        held.key_down(KVK_SHIFT);
        assert_eq!(held.key_down(KVK_SHIFT), None);
        // A release of a modifier that isn't down changes nothing.
        assert_eq!(held.key_up(KVK_OPTION), None);
    }

    #[test]
    fn release_all_lets_go_keys_first_then_modifiers() {
        let mut held = HeldKeys::default();
        held.key_down(KVK_COMMAND);
        held.key_down(KVK_SHIFT);
        held.key_down(KVK_ANSI_C);
        assert_eq!(
            held.release_all(),
            vec![
                key(KVK_ANSI_C, false, false, COMMAND | SHIFT).unwrap(),
                modifier(KVK_SHIFT, false, COMMAND).unwrap(),
                modifier(KVK_COMMAND, false, 0).unwrap(),
            ]
        );
        assert!(held.release_all().is_empty());
        assert_eq!(held.flags(), 0);
    }

    #[test]
    fn android_codes_pass_through_without_flags() {
        let mut held = HeldKeys::default();
        // kVK_ANSI_A, kVK_Space, kVK_Return, kVK_Delete.
        for code in [0x00, 0x31, 0x24, 0x33] {
            assert_eq!(held.key_down(code), key(code, true, false, 0));
            assert_eq!(held.key_up(code), key(code, false, false, 0));
        }
    }
}

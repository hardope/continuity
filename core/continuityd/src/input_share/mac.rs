//! Keyboard-and-mouse sharing's macOS backend: a session-level `CGEventTap`
//! sees every mouse and keyboard event, hands it to the shared core
//! (`Shared::on_input`) and swallows it while the mouse is over there. The
//! cursor is unlinked from the mouse and hidden meanwhile; the events still
//! arrive here, with their deltas.
//!
//! An active tap on keyboard events needs the Accessibility permission the
//! remote-control host already asks for, and on macOS 10.15 and later Input
//! Monitoring too. Only the main display is watched; a cursor on a second
//! display never crosses onto another computer.

use super::{Bounds, LocalInput, Point, Shared};
use crate::remote_viewer::key_from_native_code;
use continuity_proto::{MouseButton as ProtoMouseButton, Platform};
use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRetained, CFRunLoop, CGPoint};
use objc2_core_graphics::{
    CGAssociateMouseAndMouseCursorPosition, CGDisplayBounds, CGDisplayHideCursor, CGDisplayShowCursor, CGEvent, CGEventField,
    CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMainDisplayID,
    CGPreflightListenEventAccess, CGRequestListenEventAccess, CGWarpMouseCursorPosition,
};
use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use tao::keyboard::KeyCode;

thread_local! {
    /// The tap thread's own port, to re-enable it when the system disables
    /// it (a callback that took too long).
    static TAP: RefCell<Option<CFRetained<CFMachPort>>> = const { RefCell::new(None) };
}

pub(crate) struct Backend;

impl Backend {
    pub(crate) fn new(_profile: &str) -> Self {
        Self
    }

    /// Creates the event tap on a thread of its own, running its run loop
    /// for as long as the process lives.
    pub(crate) fn start(&self, shared: &Arc<Shared>) -> Result<(), String> {
        crate::media_mac::ensure_accessibility_trust();
        // Since macOS 10.15 a tap that sees keyboard events also needs Input
        // Monitoring; this is the call that asks for it (the system's own
        // prompt the first time) and lists Continuity under it.
        if !CGPreflightListenEventAccess() {
            CGRequestListenEventAccess();
        }
        let shared = shared.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("input-share".to_string())
            .spawn(move || run_tap(shared, started_tx))
            .map_err(|e| format!("couldn't start its thread: {e}"))?;
        match started_rx.recv() {
            Ok(true) => Ok(()),
            _ => Err("Continuity needs the Accessibility and Input Monitoring permissions (System Settings > Privacy & Security)".to_string()),
        }
    }

    pub(crate) fn placements_changed(&self, _edges: &[continuity_proto::ScreenEdge]) {}

    pub(crate) fn screen(&self) -> Option<Bounds> {
        let bounds = CGDisplayBounds(CGMainDisplayID());
        Some(Bounds { x: bounds.origin.x, y: bounds.origin.y, width: bounds.size.width, height: bounds.size.height })
    }

    /// The cursor stays put, and hidden, while the mouse drives the other
    /// computer.
    pub(crate) fn grabbed(&self, _left_at: Point) {
        set_cursor_in_background();
        CGAssociateMouseAndMouseCursorPosition(false);
        CGDisplayHideCursor(CGMainDisplayID());
    }

    pub(crate) fn released(&self, at: Point) {
        CGAssociateMouseAndMouseCursorPosition(true);
        CGWarpMouseCursorPosition(CGPoint { x: at.x, y: at.y });
        CGDisplayShowCursor(CGMainDisplayID());
    }
}

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
    // Kept alive for the tap's whole life, which is the process's: never
    // released.
    let user_info = Arc::into_raw(shared) as *mut c_void;
    // SAFETY: `user_info` points at an `Arc<Shared>` that's never dropped.
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
    let Some(input) = local_input(kind, event_ref) else { return event.as_ptr() };
    if shared.on_input(input) {
        std::ptr::null_mut()
    } else {
        event.as_ptr()
    }
}

/// What a tapped event is to the shared core.
fn local_input(kind: CGEventType, event: &CGEvent) -> Option<LocalInput> {
    let field = |field: CGEventField| CGEvent::integer_value_field(Some(event), field);
    Some(match kind {
        CGEventType::MouseMoved | CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged | CGEventType::OtherMouseDragged => {
            let location = CGEvent::location(Some(event));
            LocalInput::Moved {
                at: Point { x: location.x, y: location.y },
                delta: (field(CGEventField::MouseEventDeltaX) as f64, field(CGEventField::MouseEventDeltaY) as f64),
            }
        }
        CGEventType::LeftMouseDown | CGEventType::LeftMouseUp => {
            LocalInput::Button { button: ProtoMouseButton::Left, down: kind == CGEventType::LeftMouseDown }
        }
        CGEventType::RightMouseDown | CGEventType::RightMouseUp => {
            LocalInput::Button { button: ProtoMouseButton::Right, down: kind == CGEventType::RightMouseDown }
        }
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            LocalInput::Button { button: ProtoMouseButton::Middle, down: kind == CGEventType::OtherMouseDown }
        }
        // Point deltas: pixels, positive scrolling up and left, as the
        // viewer sends them too.
        CGEventType::ScrollWheel => LocalInput::Scroll {
            dx: field(CGEventField::ScrollWheelEventPointDeltaAxis2) as f64,
            dy: field(CGEventField::ScrollWheelEventPointDeltaAxis1) as f64,
        },
        CGEventType::KeyDown | CGEventType::KeyUp => {
            let key = u16::try_from(field(CGEventField::KeyboardEventKeycode)).ok().and_then(|code| key_from_native_code(code, Platform::MacOs));
            LocalInput::Key { key, down: kind == CGEventType::KeyDown }
        }
        CGEventType::FlagsChanged => {
            let (key, down) = modifier_change(field(CGEventField::KeyboardEventKeycode), CGEvent::flags(Some(event)))?;
            LocalInput::Key { key: Some(key), down }
        }
        _ => return None,
    })
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
}

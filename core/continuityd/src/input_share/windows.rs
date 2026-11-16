//! Keyboard-and-mouse sharing's Windows backend: low-level mouse and
//! keyboard hooks (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`) on a thread of their own
//! with its own message loop. Each event is handed to the shared core
//! (`Shared::on_input`); returning 1 from the hook swallows it while the
//! mouse is over there.
//!
//! **The cursor while over there** waits in the middle of the primary
//! screen: a swallowed move leaves the cursor where it is, and the hook
//! reports where it *would* have gone, so each move reads as a delta from
//! that point — and, being far from every edge, none is lost to the screen
//! edge clipping it. Windows has no supported way for an app to hide the
//! cursor everywhere, so it stays visible there until it comes back.
//!
//! Events another program injected (`LLMHF_INJECTED`, `LLKHF_INJECTED`) —
//! including Continuity's own, when another computer is controlling this
//! one — pass through untouched, so a remote controller can't push the
//! cursor across an edge. Only the primary screen's edges are watched.

use super::{Bounds, LocalInput, Point, Shared, WHEEL_NOTCH};
use crate::remote_viewer::key_from_native_code;
use continuity_proto::{MouseButton as ProtoMouseButton, Platform, ScreenEdge};
use std::sync::{Arc, Mutex, OnceLock};
use tao::keyboard::KeyCode;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, GetSystemMetrics, SetCursorPos, SetWindowsHookExW, TranslateMessage, HHOOK,
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
};

/// What the hooks call into — set once, when the hooks start.
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Where moves are measured from: the last position while the mouse is
/// here, the parking point while it's over there.
struct Cursor {
    last: Option<Point>,
    park: Option<Point>,
}

static CURSOR: Mutex<Cursor> = Mutex::new(Cursor { last: None, park: None });

const VK_RETURN: u32 = 0x0D;

pub(crate) struct Backend;

impl Backend {
    pub(crate) fn new(_profile: &str) -> Self {
        Self
    }

    /// Installs the hooks on a thread of their own, which then runs the
    /// message loop low-level hooks are delivered through, for as long as
    /// the process lives.
    pub(crate) fn start(&self, shared: &Arc<Shared>) -> Result<(), String> {
        let _ = SHARED.set(shared.clone());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("input-share".to_string())
            .spawn(move || unsafe {
                let instance = GetModuleHandleW(PCWSTR::null()).map(|module| HINSTANCE(module.0)).unwrap_or_default();
                let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0);
                let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), instance, 0);
                let started = match (mouse, keyboard) {
                    (Ok(_), Ok(_)) => Ok(()),
                    (Err(e), _) | (_, Err(e)) => Err(format!("couldn't watch the mouse and keyboard: {e}")),
                };
                let ok = started.is_ok();
                let _ = started_tx.send(started);
                if !ok {
                    return;
                }
                let mut message = MSG::default();
                while GetMessageW(&mut message, HWND::default(), 0, 0).0 > 0 {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            })
            .map_err(|e| format!("couldn't start its thread: {e}"))?;
        started_rx.recv().unwrap_or_else(|_| Err("its thread stopped".to_string()))
    }

    pub(crate) fn placements_changed(&self, _edges: &[ScreenEdge]) {}

    /// The primary screen, in the physical pixels this per-monitor-DPI-aware
    /// process sees.
    pub(crate) fn screen(&self) -> Option<Bounds> {
        let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        (width > 0 && height > 0).then(|| Bounds { x: 0.0, y: 0.0, width: f64::from(width), height: f64::from(height) })
    }

    /// Parks the cursor in the middle of the screen (see the module docs).
    pub(crate) fn grabbed(&self, _left_at: Point) {
        let Some(screen) = self.screen() else { return };
        let park = Point { x: (screen.width / 2.0).round(), y: (screen.height / 2.0).round() };
        CURSOR.lock().unwrap().park = Some(park);
        if let Err(e) = unsafe { SetCursorPos(park.x as i32, park.y as i32) } {
            tracing::debug!("couldn't park the cursor: {e}");
        }
    }

    pub(crate) fn released(&self, at: Point) {
        {
            let mut cursor = CURSOR.lock().unwrap();
            cursor.park = None;
            cursor.last = Some(at);
        }
        if let Err(e) = unsafe { SetCursorPos(at.x.round() as i32, at.y.round() as i32) } {
            tracing::debug!("couldn't put the cursor back: {e}");
        }
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        if let Some(shared) = SHARED.get() {
            // SAFETY: for `WH_MOUSE_LL`, `lparam` is an `MSLLHOOKSTRUCT`.
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if info.flags & LLMHF_INJECTED == 0 {
                let swallow = match mouse_input(wparam.0 as u32, info) {
                    Some(input) => shared.on_input(input),
                    // Side buttons and the rest: nothing to forward, but not
                    // for this computer either while the mouse is over there.
                    None => shared.is_grabbed(),
                };
                if swallow {
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        if let Some(shared) = SHARED.get() {
            // SAFETY: for `WH_KEYBOARD_LL`, `lparam` is a `KBDLLHOOKSTRUCT`.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            if info.flags.0 & LLKHF_INJECTED.0 == 0 {
                let message = wparam.0 as u32;
                let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
                let key = key_for(info.vkCode, info.flags.0 & LLKHF_EXTENDED.0 != 0);
                if shared.on_input(LocalInput::Key { key, down }) {
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

/// What a low-level mouse message is to the shared core.
fn mouse_input(message: u32, info: &MSLLHOOKSTRUCT) -> Option<LocalInput> {
    let at = Point { x: f64::from(info.pt.x), y: f64::from(info.pt.y) };
    Some(match message {
        WM_MOUSEMOVE => {
            let mut cursor = CURSOR.lock().unwrap();
            let from = cursor.park.or(cursor.last).unwrap_or(at);
            if cursor.park.is_none() {
                cursor.last = Some(at);
            }
            LocalInput::Moved { at, delta: (at.x - from.x, at.y - from.y) }
        }
        WM_LBUTTONDOWN => LocalInput::Button { button: ProtoMouseButton::Left, down: true },
        WM_LBUTTONUP => LocalInput::Button { button: ProtoMouseButton::Left, down: false },
        WM_RBUTTONDOWN => LocalInput::Button { button: ProtoMouseButton::Right, down: true },
        WM_RBUTTONUP => LocalInput::Button { button: ProtoMouseButton::Right, down: false },
        WM_MBUTTONDOWN => LocalInput::Button { button: ProtoMouseButton::Middle, down: true },
        WM_MBUTTONUP => LocalInput::Button { button: ProtoMouseButton::Middle, down: false },
        // Wheel forward is up, as forwarded; a tilt right is a scroll right,
        // which the viewer's convention counts negative.
        WM_MOUSEWHEEL => LocalInput::Scroll { dx: 0.0, dy: wheel_notches(info.mouseData) * WHEEL_NOTCH },
        WM_MOUSEHWHEEL => LocalInput::Scroll { dx: -wheel_notches(info.mouseData) * WHEEL_NOTCH, dy: 0.0 },
        _ => return None,
    })
}

/// A wheel message's movement in notches: the signed high word of
/// `mouseData`, 120 to a notch.
fn wheel_notches(mouse_data: u32) -> f64 {
    f64::from((mouse_data >> 16) as u16 as i16) / 120.0
}

/// The key a hooked virtual key is. Enter and the numeric keypad's Enter
/// share `VK_RETURN`; the extended flag tells the keypad's apart.
fn key_for(vk: u32, extended: bool) -> Option<KeyCode> {
    if vk == VK_RETURN && extended {
        return Some(KeyCode::NumpadEnter);
    }
    key_from_native_code(u16::try_from(vk).ok()?, Platform::Windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_notches_are_the_signed_high_word() {
        assert_eq!(wheel_notches(120 << 16), 1.0);
        assert_eq!(wheel_notches(((-240i16) as u16 as u32) << 16), -2.0);
        assert_eq!(wheel_notches(60 << 16), 0.5, "a smooth wheel's partial notch");
    }

    #[test]
    fn hooked_keys_map_with_the_keypad_enter_told_apart() {
        assert_eq!(key_for(VK_RETURN, false), Some(KeyCode::Enter));
        assert_eq!(key_for(VK_RETURN, true), Some(KeyCode::NumpadEnter));
        assert_eq!(key_for(0xA2, false), Some(KeyCode::ControlLeft));
        assert_eq!(key_for(0x41, false), Some(KeyCode::KeyA));
        assert_eq!(key_for(0x1_0000, false), None);
    }
}

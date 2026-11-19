//! Keyboard-and-mouse sharing's Windows backend: low-level mouse and
//! keyboard hooks (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`) on a thread of their own
//! with its own message loop. Each event is handed to the shared core
//! (`Shared::on_input`); returning 1 from the hook swallows it while the
//! mouse is over there.
//!
//! **Measuring the mouse.** A low-level hook is told where each move would
//! take the cursor (`MSLLHOOKSTRUCT::pt`, not clipped to the screen), before
//! the cursor goes there. A move is measured from where the cursor really
//! is: here, where the last move let through left it; over there, where it
//! stopped when it left — every move is swallowed meanwhile, so it stays
//! put, and a thread timer re-reads it every so often outside the hook in
//! case something else moved it. Nothing here moves the cursor except to
//! put it back when the mouse returns, and that from a thread holding no
//! lock the hooks need. **Real bug fixed here**: the cursor used to be
//! parked in the middle of the screen from inside the hook, while the move
//! that crossed was let through after it — so the cursor really sat at the
//! edge while every move was measured from the middle, each one arriving
//! over there as half a screen's jump. The pointer there flew to the far
//! side whatever the mouse did, and couldn't be brought back.
//!
//! Windows has no supported way for an app to hide the cursor everywhere,
//! so it stays visible, at the edge it left through, until it comes back.
//!
//! Events another program injected (`LLMHF_INJECTED`, `LLKHF_INJECTED`) —
//! including Continuity's own, when another computer is controlling this
//! one — pass through untouched, so a remote controller can't push the
//! cursor across an edge. The edges watched are those of the whole desktop
//! (the bounding box of every monitor), so a second monitor beside the
//! first stays reachable.

use super::{Bounds, LocalInput, Point, Shared, WHEEL_NOTCH};
use crate::remote_viewer::key_from_native_code;
use continuity_proto::{MouseButton as ProtoMouseButton, Platform, ScreenEdge};
use std::sync::{Arc, Mutex, OnceLock};
use tao::keyboard::KeyCode;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, GetPhysicalCursorPos, GetSystemMetrics, SetCursorPos, SetTimer, SetWindowsHookExW,
    TranslateMessage, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_TIMER,
};

/// What the hooks call into — set once, when the hooks start.
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Where the cursor really is, for measuring moves (see the module docs).
/// Never held while calling into the core: the core calls `released` with
/// its own lock held, and that takes this one.
struct Cursor {
    /// Where the last move let through left the cursor, kept on the desktop
    /// the way Windows keeps the cursor on it.
    here: Option<Point>,
    /// While the mouse is over there: where the cursor stopped.
    frozen: Option<Point>,
}

static CURSOR: Mutex<Cursor> = Mutex::new(Cursor { here: None, frozen: None });

/// How often, while the mouse is over there, the cursor's real position is
/// read again — outside the hook, where nothing about it is in flux.
const RECHECK_MS: u32 = 50;

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
                // A thread timer: its WM_TIMER comes through this loop.
                SetTimer(HWND::default(), 0, RECHECK_MS, None);
                let mut message = MSG::default();
                while GetMessageW(&mut message, HWND::default(), 0, 0).0 > 0 {
                    if message.message == WM_TIMER {
                        recheck_frozen();
                        continue;
                    }
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            })
            .map_err(|e| format!("couldn't start its thread: {e}"))?;
        started_rx.recv().unwrap_or_else(|_| Err("its thread stopped".to_string()))
    }

    pub(crate) fn placements_changed(&self, _edges: &[ScreenEdge]) {}

    /// The whole desktop — every monitor's bounding box — in the physical
    /// pixels this per-monitor-DPI-aware process (tao makes it one) sees.
    pub(crate) fn screen(&self) -> Option<Bounds> {
        let (x, y, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        (width > 0 && height > 0).then(|| Bounds { x: f64::from(x), y: f64::from(y), width: f64::from(width), height: f64::from(height) })
    }

    /// Nothing to do: the hook swallowing every move keeps the cursor where
    /// it stopped (see the module docs).
    pub(crate) fn grabbed(&self, _left_at: Point) {}

    /// Puts the cursor back at `at`. From a thread of its own, holding
    /// nothing: the core calls this with its lock held, and `SetCursorPos`
    /// can wait on the hook thread, which may itself be waiting on that lock.
    pub(crate) fn released(&self, at: Point) {
        {
            let mut cursor = CURSOR.lock().unwrap();
            cursor.frozen = None;
            cursor.here = Some(at);
        }
        let (x, y) = (at.x.round() as i32, at.y.round() as i32);
        let spawned = std::thread::Builder::new().name("input-share-cursor".to_string()).spawn(move || {
            if let Err(e) = unsafe { SetCursorPos(x, y) } {
                tracing::debug!("couldn't put the cursor back: {e}");
            }
        });
        if let Err(e) = spawned {
            tracing::debug!("couldn't put the cursor back: {e}");
        }
    }
}

/// While the mouse is over there: reads where the cursor really is again,
/// on the hook thread between hook calls — nothing is moving it then.
fn recheck_frozen() {
    let mut cursor = CURSOR.lock().unwrap();
    if cursor.frozen.is_some() {
        if let Some(now) = physical_cursor_pos() {
            cursor.frozen = Some(now);
        }
    }
}

fn physical_cursor_pos() -> Option<Point> {
    let mut point = POINT::default();
    unsafe { GetPhysicalCursorPos(&mut point) }.ok()?;
    Some(Point { x: f64::from(point.x), y: f64::from(point.y) })
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        if let Some(shared) = SHARED.get() {
            // SAFETY: for `WH_MOUSE_LL`, `lparam` is an `MSLLHOOKSTRUCT`.
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if info.flags & LLMHF_INJECTED == 0 && on_mouse(shared, wparam.0 as u32, info) {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

/// One hooked mouse event: whether to swallow it.
fn on_mouse(shared: &Shared, message: u32, info: &MSLLHOOKSTRUCT) -> bool {
    let at = Point { x: f64::from(info.pt.x), y: f64::from(info.pt.y) };
    if message != WM_MOUSEMOVE {
        return match mouse_input(message, info) {
            Some(input) => shared.on_input(input),
            // Side buttons and the rest: nothing to forward, but not for
            // this computer either while the mouse is over there.
            None => shared.is_grabbed(),
        };
    }
    let from = {
        let cursor = CURSOR.lock().unwrap();
        // Here, where the cursor is before this move: read back, which
        // inside the hook is where it was; failing that (a reading equal
        // to where it's going would say it already moved), where the last
        // move left it.
        cursor.frozen.or_else(|| physical_cursor_pos().filter(|now| *now != at)).or(cursor.here).unwrap_or(at)
    };
    let swallowed = shared.on_input(LocalInput::Moved { at, delta: (at.x - from.x, at.y - from.y) });
    let mut cursor = CURSOR.lock().unwrap();
    if swallowed {
        // Over there (this move may have been the one that crossed): the
        // cursor stays where it was before it.
        cursor.frozen.get_or_insert(from);
    } else {
        cursor.frozen = None;
        cursor.here = Some(shared.backend.screen().map_or(at, |screen| on_desktop(at, screen)));
    }
    swallowed
}

/// `point` kept on `screen`, as Windows keeps the cursor on the desktop.
fn on_desktop(point: Point, screen: Bounds) -> Point {
    Point {
        x: point.x.clamp(screen.x, screen.x + screen.width - 1.0),
        y: point.y.clamp(screen.y, screen.y + screen.height - 1.0),
    }
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

/// What a low-level mouse button or wheel message is to the shared core.
fn mouse_input(message: u32, info: &MSLLHOOKSTRUCT) -> Option<LocalInput> {
    Some(match message {
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

    #[test]
    fn the_cursor_is_kept_on_the_desktop() {
        // A monitor left of the primary: the desktop starts at -1280.
        let desktop = Bounds { x: -1280.0, y: 0.0, width: 3200.0, height: 1080.0 };
        assert_eq!(on_desktop(Point { x: 1925.0, y: 500.0 }, desktop), Point { x: 1919.0, y: 500.0 });
        assert_eq!(on_desktop(Point { x: -1300.0, y: -4.0 }, desktop), Point { x: -1280.0, y: 0.0 });
        assert_eq!(on_desktop(Point { x: 10.0, y: 20.0 }, desktop), Point { x: 10.0, y: 20.0 });
    }
}

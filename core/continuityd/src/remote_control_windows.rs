//! Windows remote control. Both halves are public, documented Win32
//! APIs, same as `media_windows.rs`: keyboard/mouse injection through
//! `SendInput` (this file's `KEYBDINPUT`/`MOUSEINPUT` construction is a
//! direct extension of that file's media-key `send_key`, just for
//! arbitrary key codes and full mouse events instead of the fixed
//! `VK_MEDIA_*` set), and screen capture through classic GDI `BitBlt` —
//! deliberately *not* the newer Desktop Duplication API (DXGI), which
//! needs a real Direct3D device and adapter enumeration to set up. GDI's
//! screen-DC-to-memory-DC blit has worked unchanged since Windows 95 and
//! needs no device/adapter setup at all — the simpler, more universally
//! documented API is the safer choice for code that can only be verified
//! by reading Microsoft's own docs and CI's compile check, never run
//! against a real Windows session in this dev environment.
//!
//! **Not verified against a real Windows session** — same caveat as
//! every other Windows-only file in this codebase (see `media_windows.rs`'s
//! own header). Implemented from the documented Win32 API surface; flag
//! anything that misbehaves.

use continuity_daemon::RemoteControlHost;
use continuity_proto::{InputEventKind, MouseButton as ProtoMouseButton};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, ICONINFO, SM_CXSCREEN, SM_CYSCREEN,
};

const CAPTURE_FPS: u64 = 10;

pub struct WindowsRemoteControlHost {
    capturing: Arc<AtomicBool>,
    /// Virtual keys the controlling device currently holds down here, so
    /// a session that ends without their key-ups (the other computer
    /// slept, the network dropped) can still let go of them — an injected
    /// Ctrl or Shift left down would go on modifying the local person's
    /// own typing until they happened to press and release it themselves.
    held_keys: Mutex<Vec<u16>>,
    /// Mouse buttons it holds down here, for the same reason. **Real bug
    /// fixed here**: only keys were tracked, so a session that ended
    /// mid-drag (the connection dropping, say) left the button logically
    /// down, and the local person's own mouse went on dragging or
    /// selecting until they clicked.
    held_buttons: Mutex<Vec<ProtoMouseButton>>,
}

impl WindowsRemoteControlHost {
    pub fn new() -> Self {
        Self {
            capturing: Arc::new(AtomicBool::new(false)),
            held_keys: Mutex::new(Vec::new()),
            held_buttons: Mutex::new(Vec::new()),
        }
    }

    /// Lets go of everything the controlling device left held down —
    /// buttons first, so a Shift-drag ends as one.
    fn release_held_input(&self) {
        let buttons = std::mem::take(&mut *self.held_buttons.lock().unwrap());
        for button in buttons.into_iter().rev() {
            inject_event(InputEventKind::MouseButton { button, down: false });
        }
        let held = std::mem::take(&mut *self.held_keys.lock().unwrap());
        for vk in held.into_iter().rev() {
            send_key_event(vk, true);
        }
    }
}

impl Default for WindowsRemoteControlHost {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteControlHost for WindowsRemoteControlHost {
    fn inject(&self, event: InputEventKind) {
        match event {
            // Codes past `u16` can't be a virtual key; truncating one could
            // alias a real key, so it's dropped instead.
            InputEventKind::KeyDown { code } | InputEventKind::KeyUp { code } if u16::try_from(code).is_err() => {
                tracing::debug!("ignoring out-of-range key code {code:#x}");
                return;
            }
            InputEventKind::KeyDown { code } => {
                let mut held = self.held_keys.lock().unwrap();
                if !held.contains(&(code as u16)) {
                    held.push(code as u16);
                }
            }
            InputEventKind::KeyUp { code } => self.held_keys.lock().unwrap().retain(|&vk| vk != code as u16),
            InputEventKind::MouseButton { button, down: true } => {
                let mut held = self.held_buttons.lock().unwrap();
                if !held.contains(&button) {
                    held.push(button);
                }
            }
            InputEventKind::MouseButton { button, down: false } => self.held_buttons.lock().unwrap().retain(|&held| held != button),
            _ => {}
        }
        inject_event(event);
    }

    fn start_capture(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        // Nothing should be held at the start of a session; if a previous
        // one somehow left something, let go of it first.
        self.release_held_input();
        // Unlike macOS's Screen Recording permission, plain GDI
        // `BitBlt`-based capture needs no special OS permission grant at
        // all on Windows — this probe is purely "does capture actually
        // produce bytes," not a permission check.
        if capture_jpeg().is_none() {
            tracing::warn!("screen capture failed on first attempt");
            return None;
        }

        self.capturing.store(true, Ordering::Relaxed);
        let capturing = self.capturing.clone();
        let (tx, rx) = mpsc::channel(2);
        std::thread::spawn(move || {
            let target_interval = Duration::from_millis(1000 / CAPTURE_FPS);
            while capturing.load(Ordering::Relaxed) {
                let started_at = std::time::Instant::now();
                if let Some(jpeg) = capture_jpeg() {
                    let _ = tx.try_send(jpeg);
                } else {
                    tracing::debug!("a capture attempt failed mid-session");
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
        self.release_held_input();
    }
}

/// One screenshot of the primary display, JPEG-encoded via GDI `BitBlt`
/// into a memory device context, `GetDIBits` to pull the raw BGRA pixels
/// back out, then the pure-Rust `image` crate for the JPEG encode itself
/// (see this crate's Cargo.toml entry for why not Windows' own WIC API).
fn capture_jpeg() -> Option<Vec<u8>> {
    // SAFETY: every GDI handle obtained here (`screen_dc`, `mem_dc`,
    // `bitmap`) is released/deleted on every exit path below, including
    // the early-return ones — GDI handles are a strictly limited system
    // resource, unlike Rust's own memory, so nothing here can rely on
    // scope-based cleanup the way owned Rust values normally would.
    unsafe {
        let width = GetSystemMetrics(SM_CXSCREEN);
        let height = GetSystemMetrics(SM_CYSCREEN);
        if width <= 0 || height <= 0 {
            tracing::debug!("capture_jpeg: GetSystemMetrics gave non-positive dimensions ({width}x{height})");
            return None;
        }

        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            tracing::debug!("capture_jpeg: GetDC(None) returned an invalid HDC");
            return None;
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
        if mem_dc.is_invalid() || bitmap.is_invalid() {
            tracing::debug!(
                "capture_jpeg: CreateCompatibleDC/CreateCompatibleBitmap failed (mem_dc_invalid={}, bitmap_invalid={})",
                mem_dc.is_invalid(),
                bitmap.is_invalid()
            );
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            return None;
        }
        let previous: HGDIOBJ = SelectObject(mem_dc, bitmap);

        let blit_result = BitBlt(mem_dc, 0, 0, width, height, screen_dc, 0, 0, SRCCOPY);
        if let Err(e) = &blit_result {
            tracing::debug!("capture_jpeg: BitBlt failed: {e}");
        }
        let blit_ok = blit_result.is_ok();

        // Composited directly onto the same memory DC the screen was
        // just blitted into, before `GetDIBits` ever reads it back out —
        // GDI capture has no equivalent of macOS's Screen-Recording-gated
        // "no cursor at all" limitation; the real system cursor is right
        // there for the taking via two long-documented, ordinary calls.
        if blit_ok {
            draw_cursor_onto(mem_dc);
        }

        let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
        let mut bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative height requests a top-down DIB (row 0 first) —
                // GDI's default bottom-up layout would otherwise need the
                // rows reversed before handing pixels to `image`.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dibits_result = if blit_ok {
            GetDIBits(mem_dc, bitmap, 0, height as u32, Some(pixels.as_mut_ptr() as *mut _), &mut bitmap_info, DIB_RGB_COLORS)
        } else {
            0
        };
        if blit_ok && dibits_result == 0 {
            tracing::debug!("capture_jpeg: GetDIBits returned 0 — last error: {:?}", GetLastError());
        }
        let got_bits = blit_ok && dibits_result != 0;

        SelectObject(mem_dc, previous);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);

        if !got_bits {
            return None;
        }

        // GDI's 32-bit DIB is BGRA (actually BGRX — the fourth byte isn't
        // a meaningful alpha channel for a screen capture), and JPEG has
        // no alpha channel *at all* — `image`'s JPEG encoder correctly
        // refuses `Rgba8` outright rather than silently dropping the
        // channel for you (confirmed the hard way: this was the actual,
        // entire reason screen capture always failed on real Windows
        // hardware, unrelated to every earlier permission/address/DPI
        // theory). Converts straight to a packed RGB buffer here — no
        // point routing through `image::RgbaImage` first just to have it
        // reject the very shape it was given.
        let mut rgb_pixels = Vec::with_capacity(pixels.len() / 4 * 3);
        for px in pixels.chunks_exact(4) {
            rgb_pixels.push(px[2]); // R (was B)
            rgb_pixels.push(px[1]); // G
            rgb_pixels.push(px[0]); // B (was R)
        }

        let Some(image_buffer) = image::RgbImage::from_raw(width as u32, height as u32, rgb_pixels) else {
            tracing::debug!("capture_jpeg: RgbImage::from_raw failed — buffer/dimension mismatch ({width}x{height})");
            return None;
        };
        let mut jpeg_bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, 40);
        if let Err(e) = encoder.encode(image_buffer.as_raw(), image_buffer.width(), image_buffer.height(), image::ExtendedColorType::Rgb8) {
            tracing::debug!("capture_jpeg: JPEG encode failed: {e}");
            return None;
        }
        Some(jpeg_bytes)
    }
}

/// Draws the real system cursor onto `mem_dc` at its actual current
/// position — unlike macOS (see `remote_control_mac.rs`'s
/// `composite_cursor_marker`), this doesn't need a synthetic stand-in at
/// all: `GetCursorInfo` + `DrawIconEx` is the standard, long-documented
/// way to grab the literal cursor Windows itself is currently showing,
/// hotspot and all. A no-op (leaving the frame as a plain screenshot)
/// whenever any step fails or the cursor's hidden — a missing cursor is
/// a strictly smaller problem than a wrongly-placed fake one.
fn draw_cursor_onto(mem_dc: windows::Win32::Graphics::Gdi::HDC) {
    let mut info = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
    // SAFETY: `info` is a correctly-sized, stack-local `CURSORINFO` with
    // `cbSize` set as the API requires before the call.
    if unsafe { GetCursorInfo(&mut info) }.is_err() {
        return;
    }
    if info.flags.0 & CURSOR_SHOWING.0 == 0 {
        // Cursor is programmatically hidden (e.g. a fullscreen video
        // player, or a game capturing the mouse) — nothing to draw.
        return;
    }

    let mut icon_info = ICONINFO::default();
    // SAFETY: `info.hCursor` came from `GetCursorInfo` above, still valid
    // for the duration of this call; `icon_info` is a correctly-sized
    // stack-local out-parameter.
    if unsafe { GetIconInfo(info.hCursor, &mut icon_info) }.is_err() {
        return;
    }
    // `GetIconInfo` allocates two new bitmaps we now own — required
    // cleanup, not optional, or this leaks two GDI handles every single
    // captured frame (10 times a second for as long as a session runs).
    unsafe {
        if !icon_info.hbmMask.is_invalid() {
            let _ = DeleteObject(icon_info.hbmMask);
        }
        if !icon_info.hbmColor.is_invalid() {
            let _ = DeleteObject(icon_info.hbmColor);
        }
    }

    // The hotspot is the pixel *within* the cursor image that represents
    // its actual position (e.g. the tip of an arrow, not the icon's
    // corner) — `DrawIconEx`'s (x, y) is where the icon's top-left corner
    // goes, so the hotspot has to be subtracted back out to land the
    // right pixel of the cursor image on the real screen position.
    let x = info.ptScreenPos.x - icon_info.xHotspot as i32;
    let y = info.ptScreenPos.y - icon_info.yHotspot as i32;
    // SAFETY: `mem_dc` is a valid memory DC for the duration of this
    // call (owned by `capture_jpeg`, released after it returns);
    // `info.hCursor` is still valid, same as above. `0, 0` for
    // width/height means "use the cursor's own actual size."
    unsafe {
        let _ = DrawIconEx(mem_dc, x, y, info.hCursor, 0, 0, 0, None, DI_NORMAL);
    }
}

/// Injects one input event via `SendInput` — the exact same call
/// `media_windows.rs`'s `send_key` already uses for media keys, just
/// building `KEYBDINPUT`/`MOUSEINPUT` for arbitrary codes and full mouse
/// events instead of the fixed `VK_MEDIA_*`/`VK_VOLUME_*` set.
fn inject_event(event: InputEventKind) {
    match event {
        InputEventKind::KeyDown { code } => send_key_event(code as u16, false),
        InputEventKind::KeyUp { code } => send_key_event(code as u16, true),
        InputEventKind::MouseMove { x, y } => send_mouse_input(MOUSEINPUT {
            // `MOUSEEVENTF_ABSOLUTE` coordinates are normalized to
            // 0..=65535 regardless of actual screen resolution — this
            // protocol's own `MouseMove` is already normalized 0.0..=1.0
            // for exactly the same reason (the controller doesn't need
            // to know the controlled screen's real pixel dimensions), so
            // the two line up almost for free, just a different scale
            // factor.
            dx: (x * 65535.0) as i32,
            dy: (y * 65535.0) as i32,
            mouseData: 0,
            dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
            time: 0,
            dwExtraInfo: 0,
        }),
        InputEventKind::MouseButton { button, down } => {
            let flag = match (button, down) {
                (ProtoMouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                (ProtoMouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                (ProtoMouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                (ProtoMouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                (ProtoMouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                (ProtoMouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            };
            send_mouse_input(MOUSEINPUT { dx: 0, dy: 0, mouseData: 0, dwFlags: flag, time: 0, dwExtraInfo: 0 });
        }
        InputEventKind::MouseScroll { delta_x, delta_y } => {
            // `WHEEL_DELTA` (120) is one physical wheel "notch" by
            // Windows convention — scaling the continuous delta by it
            // rather than passing raw pixels keeps scroll speed roughly
            // consistent with an actual wheel/trackpad, rather than
            // needing the receiving app to already expect pixel-precision
            // input.
            if delta_y != 0.0 {
                send_mouse_input(MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: (delta_y * 120.0) as i32 as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                });
            }
            if delta_x != 0.0 {
                send_mouse_input(MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: (delta_x * 120.0) as i32 as u32,
                    dwFlags: MOUSEEVENTF_HWHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                });
            }
        }
    }
}

/// The scan code a real keyboard sends with `vk`, on the current layout —
/// `KEYEVENTF_EXTENDEDKEY` supplies the E0 prefix for the keys that have
/// one. **Real bug fixed here**: injected keys carried 0, and programs that
/// read the scan code rather than the virtual key (games, remote-desktop
/// and VM clients, anything that goes by a key's position) saw a key that
/// doesn't exist.
fn scan_code(vk: u16) -> u16 {
    // SAFETY: a plain lookup, no pointers involved.
    unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) as u16 }
}

fn send_key_event(vk: u16, key_up: bool) {
    let mut flags = KEYBD_EVENT_FLAGS::default();
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    if is_extended_key(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan_code(vk), dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    // SAFETY: `input` is a single, correctly-sized, stack-local `INPUT`
    // matching what `SendInput` expects — same justification as
    // `media_windows.rs::send_key`.
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

/// The keys a real keyboard sends with the "extended" (E0-prefixed) scan
/// code — Microsoft's list: the arrows and the Insert/Delete/Home/End/
/// Page Up/Page Down block beside them, the right-hand Ctrl and Alt, both
/// Windows keys and the Menu key, Num Lock, Print Screen, and the keypad's
/// divide (its Enter shares `VK_RETURN` with the main one, so it can't be
/// told apart here). Without the flag, apps that read a key's extended bit
/// rather than only its virtual key take an arrow or Home/End for the
/// keypad key in the same place — Shift-arrow text selection is the
/// well-known casualty — and can't tell right Ctrl/Alt from left.
fn is_extended_key(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 // VK_PRIOR, VK_NEXT, VK_END, VK_HOME, VK_LEFT, VK_UP, VK_RIGHT, VK_DOWN
            | 0x2C // VK_SNAPSHOT
            | 0x2D // VK_INSERT
            | 0x2E // VK_DELETE
            | 0x5B // VK_LWIN
            | 0x5C // VK_RWIN
            | 0x5D // VK_APPS
            | 0x6F // VK_DIVIDE
            | 0x90 // VK_NUMLOCK
            | 0xA3 // VK_RCONTROL
            | 0xA5 // VK_RMENU
    )
}

fn send_mouse_input(mi: MOUSEINPUT) {
    let input = INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi } };
    // SAFETY: same as `send_key_event` above, just the mouse variant of
    // the same `INPUT` union.
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set-1 scan codes, the same on every layout for these keys.
    #[test]
    fn injected_keys_carry_their_scan_codes() {
        assert_eq!(scan_code(0x41), 0x1E, "A");
        assert_eq!(scan_code(0x0D), 0x1C, "Return");
        assert_eq!(scan_code(0x20), 0x39, "Space");
        assert_eq!(scan_code(0x25), 0x4B, "Left arrow (E0 4B with the extended flag)");
    }

    #[test]
    fn extended_keys_are_the_documented_ones() {
        for vk in [0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2D, 0x2E, 0x5B, 0x5C, 0x6F, 0x90, 0xA3, 0xA5] {
            assert!(is_extended_key(vk), "{vk:#x}");
        }
        // Left-hand modifiers, Right Shift (its own scan code), the main
        // block and the rest of the keypad aren't.
        for vk in [0x08, 0x09, 0x0D, 0x14, 0x1B, 0x20, 0x41, 0x60, 0x6A, 0x6B, 0x6D, 0x6E, 0x70, 0xA0, 0xA1, 0xA2, 0xA4, 0xBA] {
            assert!(!is_extended_key(vk), "{vk:#x}");
        }
    }
}

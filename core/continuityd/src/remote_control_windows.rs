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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ, SRCCOPY,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

const CAPTURE_FPS: u64 = 10;

pub struct WindowsRemoteControlHost {
    capturing: Arc<AtomicBool>,
}

impl WindowsRemoteControlHost {
    pub fn new() -> Self {
        Self { capturing: Arc::new(AtomicBool::new(false)) }
    }
}

impl Default for WindowsRemoteControlHost {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteControlHost for WindowsRemoteControlHost {
    fn inject(&self, event: InputEventKind) {
        inject_event(event);
    }

    fn start_capture(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
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
            return None;
        }

        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return None;
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
        if mem_dc.is_invalid() || bitmap.is_invalid() {
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            return None;
        }
        let previous: HGDIOBJ = SelectObject(mem_dc, bitmap.into());

        let blit_ok = BitBlt(mem_dc, 0, 0, width, height, screen_dc, 0, 0, SRCCOPY).is_ok();

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
        let got_bits = blit_ok
            && GetDIBits(mem_dc, bitmap, 0, height as u32, Some(pixels.as_mut_ptr() as *mut _), &mut bitmap_info, DIB_RGB_COLORS) != 0;

        SelectObject(mem_dc, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);

        if !got_bits {
            return None;
        }

        // GDI's 32-bit DIB is BGRA (actually BGRX — the fourth byte isn't
        // a meaningful alpha channel for a screen capture), not RGBA —
        // swap the red/blue bytes in place before handing to `image`,
        // which expects RGBA.
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2);
        }

        let image_buffer = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
        let mut jpeg_bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, 40);
        encoder
            .encode(image_buffer.as_raw(), image_buffer.width(), image_buffer.height(), image::ExtendedColorType::Rgba8)
            .ok()?;
        Some(jpeg_bytes)
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

fn send_key_event(vk: u16, key_up: bool) {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if key_up { KEYEVENTF_KEYUP } else { Default::default() },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: `input` is a single, correctly-sized, stack-local `INPUT`
    // matching what `SendInput` expects — same justification as
    // `media_windows.rs::send_key`.
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn send_mouse_input(mi: MOUSEINPUT) {
    let input = INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi } };
    // SAFETY: same as `send_key_event` above, just the mouse variant of
    // the same `INPUT` union.
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

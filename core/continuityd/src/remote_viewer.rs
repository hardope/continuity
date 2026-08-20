//! The desktop half of remote control's *controlling* side: a real
//! window showing the peer's live screen, with local mouse/keyboard
//! forwarded back as input events. Exists on every desktop platform
//! (macOS/Windows/Linux can all initiate and view a session), unlike
//! `remote_control_mac.rs`/`remote_control_windows.rs`, which are the
//! OS-specific *controlled*-side capture/injection code and only exist
//! for the two platforms that implement it.
//!
//! Rendering is plain `softbuffer` (a CPU pixel buffer blitted straight
//! into the window, no GPU API setup) rather than anything wgpu-based —
//! this is a JPEG decoded and redrawn a handful of times a second, nowhere
//! near demanding enough to justify a real graphics pipeline. Keyboard
//! input reuses the same deliberately-small subset the Android app's
//! bridge covers (lowercase letters, digits, space, enter, backspace —
//! see `MainActivity.kt`'s `macKeyCodeFor`/`windowsKeyCodeFor`), mapped
//! from `tao`'s layout-independent `physical_key` instead of a typed
//! character, for the same reason `InputEventKind` itself uses raw
//! platform key codes: the receiving side's platform, not this one,
//! decides what a code means.
//!
//! **Unverified beyond compiling** — same caveat as the rest of this
//! session's remote-control work. There's no way in this environment to
//! actually open a real window, receive a real frame, and click on it.

use continuity_daemon::EngineCommand;
use continuity_proto::{InputEventKind, MouseButton as ProtoMouseButton, Platform};
use std::num::NonZeroU32;
use std::rc::Rc;
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

pub struct RemoteViewer {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    peer_id: String,
    peer_platform: Platform,
    /// `(width, height, RGBA bytes)` of the most recently decoded frame —
    /// `None` until the first one arrives, so `redraw` has something to
    /// show a placeholder for instead of drawing garbage.
    frame: Option<(u32, u32, Vec<u8>)>,
    /// Where the frame was last actually drawn within the window
    /// (letterboxed to preserve aspect ratio) — `handle_window_event`
    /// needs this to map a click's window-pixel position back to a
    /// normalized position *on the frame*, not the whole window.
    draw_rect: (u32, u32, u32, u32),
    last_move_sent: Instant,
}

/// Throttles outgoing `MouseMove` — a real desktop mouse reports position
/// far more often than the Android touch-drag path this protocol
/// message was originally sized around, and sending every single OS
/// mouse-move event would needlessly flood the connection that also
/// carries clipboard/ping traffic. ~120Hz is well past what's
/// perceptible as latency but a fraction of raw OS mouse-move rate.
const MOVE_THROTTLE: Duration = Duration::from_millis(8);

impl RemoteViewer {
    pub fn open(
        target: &EventLoopWindowTarget<continuity_daemon::SyncEvent>,
        peer_id: String,
        peer_name: &str,
        peer_platform: Platform,
    ) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_title(format!("Continuity — Controlling '{peer_name}'"))
            .with_inner_size(LogicalSize::new(1024.0, 640.0))
            .build(target)?;
        let window = Rc::new(window);
        let window_id = window.id();
        let context = softbuffer::Context::new(window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let surface = softbuffer::Surface::new(&context, window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        tracing::debug!("RemoteViewer::open: window {window_id:?} and softbuffer surface created for peer '{peer_name}'");
        Ok(Self {
            window,
            surface,
            peer_id,
            peer_platform,
            frame: None,
            draw_rect: (0, 0, 0, 0),
            last_move_sent: Instant::now() - MOVE_THROTTLE,
        })
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    /// Decodes one incoming JPEG frame and asks the window to repaint —
    /// the actual blit happens in `redraw`, on the OS's own schedule
    /// (`RedrawRequested`), not synchronously here.
    pub fn handle_frame(&mut self, jpeg: &[u8]) {
        match image::load_from_memory(jpeg) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                tracing::debug!("handle_frame: decoded {}x{} frame ({} JPEG bytes), requesting redraw", rgba.width(), rgba.height(), jpeg.len());
                self.frame = Some((rgba.width(), rgba.height(), rgba.into_raw()));
                self.window.request_redraw();
            }
            Err(e) => tracing::debug!("couldn't decode incoming remote-control frame ({} bytes): {e}", jpeg.len()),
        }
    }

    /// Blits the latest decoded frame into the window, letterboxed to
    /// preserve its aspect ratio (nearest-neighbor scaling — this is a
    /// live control view refreshed several times a second, not something
    /// worth a higher-quality resample for). No-ops gracefully if the
    /// window is momentarily zero-sized (e.g. mid-resize) rather than
    /// erroring — `softbuffer` can't resize to zero either way.
    pub fn redraw(&mut self) {
        let size = self.window.inner_size();
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
        // 0x00RRGGBB per softbuffer's pixel format — a dark neutral fill
        // so the letterbox bars read as intentional, not a rendering bug.
        buffer.fill(0x001c1c1e);

        if let Some((fw, fh, pixels)) = &self.frame {
            let (fw, fh) = (*fw, *fh);
            if fw > 0 && fh > 0 {
                let scale = (width.get() as f32 / fw as f32).min(height.get() as f32 / fh as f32);
                let draw_w = ((fw as f32 * scale) as u32).max(1).min(width.get());
                let draw_h = ((fh as f32 * scale) as u32).max(1).min(height.get());
                let off_x = (width.get() - draw_w) / 2;
                let off_y = (height.get() - draw_h) / 2;
                self.draw_rect = (off_x, off_y, draw_w, draw_h);

                for y in 0..draw_h {
                    let src_y = ((y as f32 / scale) as u32).min(fh - 1);
                    for x in 0..draw_w {
                        let src_x = ((x as f32 / scale) as u32).min(fw - 1);
                        let si = ((src_y * fw + src_x) * 4) as usize;
                        if si + 2 >= pixels.len() {
                            continue;
                        }
                        let (r, g, b) = (pixels[si] as u32, pixels[si + 1] as u32, pixels[si + 2] as u32);
                        let di = ((off_y + y) * width.get() + (off_x + x)) as usize;
                        if di < buffer.len() {
                            buffer[di] = (r << 16) | (g << 8) | b;
                        }
                    }
                }
            }
        }

        if let Err(e) = buffer.present() {
            tracing::warn!("redraw: softbuffer present failed: {e}");
        }
    }

    /// Handles one local window event: mouse/keyboard become
    /// `EngineCommand::SendInputEvent`s to the peer, `CloseRequested`
    /// ends the session. Returns `ViewerAction::Closed` when the caller
    /// should drop this viewer (the window is gone either way once tao
    /// processes the close).
    pub fn handle_window_event(&mut self, event: &WindowEvent<'_>, commands: &UnboundedSender<EngineCommand>) -> ViewerAction {
        match event {
            WindowEvent::CloseRequested => {
                let _ = commands.send(EngineCommand::EndRemoteControlSession { peer_crypto_id: self.peer_id.clone() });
                return ViewerAction::Closed;
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.last_move_sent.elapsed() >= MOVE_THROTTLE {
                    if let Some((x, y)) = self.normalize(*position) {
                        self.send(commands, InputEventKind::MouseMove { x, y });
                        self.last_move_sent = Instant::now();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = proto_mouse_button(*button) {
                    self.send(commands, InputEventKind::MouseButton { button, down: *state == ElementState::Pressed });
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
                    self.send(commands, InputEventKind::MouseScroll { delta_x, delta_y });
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
                            self.send(commands, kind);
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
            }
            _ => {}
        }
        ViewerAction::None
    }

    fn send(&self, commands: &UnboundedSender<EngineCommand>, event: InputEventKind) {
        let _ = commands.send(EngineCommand::SendInputEvent { peer_crypto_id: self.peer_id.clone(), event });
    }

    fn normalize(&self, position: PhysicalPosition<f64>) -> Option<(f64, f64)> {
        let (ox, oy, w, h) = self.draw_rect;
        if w == 0 || h == 0 {
            return None;
        }
        let x = ((position.x - ox as f64) / w as f64).clamp(0.0, 1.0);
        let y = ((position.y - oy as f64) / h as f64).clamp(0.0, 1.0);
        Some((x, y))
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

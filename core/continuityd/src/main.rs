// Without this, a normal Rust binary on Windows defaults to the "console"
// subsystem, so launching continuityd.exe opens a cmd window alongside
// the tray icon even though nothing is ever printed to it. Gated on
// release builds so `cargo run` locally still gives you a console for
// `tracing` output; in debug the attribute would just get in the way of
// day-to-day dev.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Desktop tray app (Phase 1) — macOS/Windows/Linux. A thin GUI shell over
//! `continuity-daemon`'s engine: the engine runs on a background OS thread
//! with its own tokio runtime (tao's event loop needs the real main
//! thread), and the two talk over a channel in each direction — engine
//! events wake the tray thread via an `EventLoopProxy`, and menu clicks
//! send `EngineCommand`s straight into the engine's command channel
//! (`UnboundedSender::send` is synchronous, so no proxy needed that way).

use continuity_crypto::{Identity, TrustStore};
use continuity_daemon::{ArboardClipboard, EngineCommand, EngineConfig, SyncEvent};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

/// Scopes the identity/trust store, like `continuityctl --profile` — real
/// usage never sets this. `CONTINUITY_PROFILE` rather than an argv flag
/// because a tray app has no terminal to pass one through conveniently.
fn profile() -> String {
    std::env::var("CONTINUITY_PROFILE").unwrap_or_else(|_| "default".to_string())
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let event_loop = EventLoopBuilder::<SyncEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let device_name = continuity_daemon::default_device_name();

    let title_item = MenuItem::new(format!("Continuity — {device_name}"), false, None);
    let send_item = MenuItem::new("Send File... (no device connected)", false, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let send_item_id = send_item.id().clone();
    let quit_item_id = quit_item.id().clone();

    let menu = Menu::new();
    menu.append(&title_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&send_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Continuity")
        .with_icon(build_icon())
        .build()?;

    let commands = start_engine_thread(profile(), device_name, proxy)?;
    let last_connected_peer: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));

    event_loop.run(move |event, _target, control_flow| {
        // Keep the tray icon (and its menu) alive for the app's lifetime —
        // dropping it removes the icon from the menu bar.
        let _tray_icon = &tray_icon;
        *control_flow = ControlFlow::Wait;

        if let tao::event::Event::UserEvent(sync_event) = event {
            handle_sync_event(sync_event, &send_item, &last_connected_peer, &commands);
        }

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == send_item_id {
                if let Some((peer_id, _)) = last_connected_peer.lock().unwrap().clone() {
                    if let Some(path) = rfd::FileDialog::new().pick_file() {
                        let _ = commands.send(EngineCommand::SendFile {
                            peer_crypto_id: peer_id,
                            path: path.display().to_string(),
                        });
                    }
                }
            } else if event.id == quit_item_id {
                *control_flow = ControlFlow::Exit;
            }
        }

        // Nothing to do with tray-icon-itself clicks (left-click opening
        // the menu is handled by the OS/tray-icon crate) — just drain the
        // channel so it doesn't build up.
        let _ = TrayIconEvent::receiver().try_recv();
    });
}

fn handle_sync_event(
    event: SyncEvent,
    send_item: &MenuItem,
    last_connected_peer: &Arc<Mutex<Option<(String, String)>>>,
    commands: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
) {
    match event {
        SyncEvent::Listening { port } => tracing::info!("listening on port {port}"),
        SyncEvent::PairingRequested { peer, code } => {
            let result = rfd::MessageDialog::new()
                .set_title("Continuity — Pairing Request")
                .set_description(format!(
                    "'{}' wants to pair.\n\nConfirmation code: {code}\n\nDoes this match the code shown on the other device?",
                    peer.name
                ))
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();
            let accepted = matches!(result, rfd::MessageDialogResult::Yes);
            let _ = commands.send(EngineCommand::ConfirmPairing {
                peer_crypto_id: peer.id,
                accept: accepted,
            });
        }
        SyncEvent::Paired { peer } => notify(&format!("Paired with {}", peer.name)),
        SyncEvent::PairingDeclined { peer_name } => {
            notify(&format!("Pairing with '{peer_name}' was declined"));
        }
        SyncEvent::Connected { peer } => {
            *last_connected_peer.lock().unwrap() = Some((peer.id.clone(), peer.name.clone()));
            send_item.set_text(format!("Send File to {}...", peer.name));
            send_item.set_enabled(true);
        }
        SyncEvent::Disconnected { peer_id, peer_name } => {
            let mut last = last_connected_peer.lock().unwrap();
            if last.as_ref().map(|(id, _)| id.as_str()) == Some(peer_id.as_str()) {
                *last = None;
                send_item.set_text("Send File... (no device connected)");
                send_item.set_enabled(false);
            }
            notify(&format!("'{peer_name}' disconnected"));
        }
        SyncEvent::ClipboardReceived { from_name } => {
            notify(&format!("Clipboard synced from '{from_name}'"));
        }
        SyncEvent::ClipboardBroadcast { .. } => {}
        SyncEvent::FileReceiving { from_name, file_name, .. } => {
            notify(&format!("Receiving '{file_name}' from '{from_name}'..."));
        }
        SyncEvent::FileReceived { file_name, .. } => notify(&format!("Received '{file_name}'")),
        SyncEvent::FileSent { file_name, to_name, .. } => {
            notify(&format!("Sent '{file_name}' to '{to_name}'"));
        }
        SyncEvent::FileTransferFailed { reason, .. } => {
            notify(&format!("File transfer failed: {reason}"));
        }
        SyncEvent::Error(e) => tracing::warn!("{e}"),
    }
}

/// Shows a native notification when possible. Unbundled dev binaries often
/// can't (macOS requires a proper .app bundle with an Info.plist for
/// reliable notification delivery), so this always logs too — the tray
/// menu/dialogs are the guaranteed-visible UI; notifications are a bonus.
fn notify(body: &str) {
    tracing::info!("{body}");
    if let Err(e) = notify_rust::Notification::new()
        .summary("Continuity")
        .body(body)
        .show()
    {
        tracing::debug!("notification not shown (expected for an unbundled binary): {e}");
    }
}

fn start_engine_thread(
    profile: String,
    device_name: String,
    proxy: tao::event_loop::EventLoopProxy<SyncEvent>,
) -> anyhow::Result<tokio::sync::mpsc::UnboundedSender<EngineCommand>> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
        };

        rt.block_on(async move {
            let setup = async {
                let identity = Identity::load_or_create(&profile)?;
                tracing::info!("device id: {}", identity.device_id());
                let trust_store = TrustStore::load_default(&profile)?;
                let config = EngineConfig {
                    identity,
                    device_name,
                    trust_store,
                    clipboard: Arc::new(ArboardClipboard),
                    received_files_dir: received_files_dir(&profile),
                };
                continuity_daemon::start(config).await
            };

            match setup.await {
                Ok(mut engine) => {
                    let _ = ready_tx.send(Ok(engine.command_sender()));
                    while let Some(event) = engine.events.recv().await {
                        if proxy.send_event(event).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                }
            }
        });
    });

    ready_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("engine thread exited before starting"))?
        .map_err(|e| anyhow::anyhow!(e))
}

fn received_files_dir(profile: &str) -> PathBuf {
    let base = directories::UserDirs::new()
        .and_then(|d| d.download_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    if profile == "default" {
        base.join("Continuity")
    } else {
        base.join(format!("Continuity-{profile}"))
    }
}

/// Same chain-link mark as `assets/logo.svg` (and the Android
/// VectorDrawables), computed per-pixel rather than rasterized from an
/// embedded asset — two rings, each a stadium/capsule outline (constant
/// distance from a central line segment), at ~90° to each other like a
/// real chain link. Two things that look obvious in hindsight but took a
/// couple of iterations to get right: the rings must be at *different*
/// angles (same-angle parallel rings reads as a slanted figure-8, a
/// different concept that was considered and passed over), and their
/// centers need enough separation relative to their size that each ring's
/// far end clears the other — too close and the two holes merge into an
/// unrecognizable blob. Keep the geometry in sync if the mark ever
/// changes: ring centers (28,40) at -25° and (66,58) at 65°, half-length
/// 20, radius 12, stroke width 9, all in the same nominal 100-unit space
/// as the SVG/VectorDrawables. 4x supersampling anti-aliases the edges —
/// without it this is visibly jaggy at menu-bar sizes, since the OS
/// doesn't smooth a raw RGBA buffer the way it smooths a vector drawable.
fn build_icon() -> Icon {
    let size: u32 = 64;
    let supersample: u32 = 4;
    let scale = size as f32 / 100.0;
    let half_length = 20.0 * scale;
    let outer_radius = 16.5 * scale; // radius 12, stroke width 9, centered
    let inner_radius = 7.5 * scale;
    let rings = [
        (28.0 * scale, 40.0 * scale, -25.0_f32.to_radians()),
        (66.0 * scale, 58.0 * scale, 65.0_f32.to_radians()),
    ];

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut coverage: u32 = 0;
            for sy in 0..supersample {
                for sx in 0..supersample {
                    let px = x as f32 + (sx as f32 + 0.5) / supersample as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / supersample as f32;

                    let in_ring = rings.iter().any(|&(cx, cy, rotation)| {
                        let dx = px - cx;
                        let dy = py - cy;
                        // Undo the ring's rotation to get pixel-local
                        // coordinates, then measure distance to the
                        // central segment (clamped along its length) —
                        // the stadium/capsule distance field.
                        let (sin_r, cos_r) = rotation.sin_cos();
                        let lx = dx * cos_r + dy * sin_r;
                        let ly = -dx * sin_r + dy * cos_r;
                        let clamped_x = lx.clamp(-half_length, half_length);
                        let dist = ((lx - clamped_x).powi(2) + ly * ly).sqrt();
                        dist >= inner_radius && dist <= outer_radius
                    });

                    if in_ring {
                        coverage += 1;
                    }
                }
            }
            let alpha = (255 * coverage / (supersample * supersample)) as u8;
            rgba.extend_from_slice(&[0x2f, 0x81, 0xf7, alpha]);
        }
    }
    Icon::from_rgba(rgba, size, size).expect("generated icon buffer is well-formed")
}

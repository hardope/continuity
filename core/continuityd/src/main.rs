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

#[cfg(target_os = "macos")]
mod media_mac;
#[cfg(target_os = "windows")]
mod media_windows;
#[cfg(target_os = "linux")]
mod media_linux;
#[cfg(all(target_os = "macos", feature = "remote-control"))]
mod remote_control_mac;
#[cfg(all(target_os = "windows", feature = "remote-control"))]
mod remote_control_windows;
#[cfg(all(target_os = "linux", feature = "remote-control"))]
mod remote_control_linux;
// The *viewer* (rendering a peer's screen + forwarding local input) is
// cross-platform — any desktop OS can initiate and view a session, not
// just the two that can be captured/injected into (see that module's own
// doc comment) — so it's gated only on the feature, not target_os.
#[cfg(feature = "remote-control")]
mod remote_viewer;

use continuity_crypto::{Identity, TrustStore};
use continuity_daemon::{ArboardClipboard, EngineCommand, EngineConfig, SyncEvent};
#[cfg(feature = "remote-control")]
use continuity_proto::Platform;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

/// Who a click on a dynamically-built "Send File" submenu entry should
/// send to — rebuilt every time the connected-device set changes, so the
/// menu always reflects who's actually connected instead of only ever
/// offering the single most-recently-connected device.
#[derive(Clone)]
enum SendTarget {
    Peer(String),
    All,
}

/// Scopes the identity/trust store, like `continuityctl --profile` — real
/// usage never sets this. `CONTINUITY_PROFILE` rather than an argv flag
/// because a tray app has no terminal to pass one through conveniently.
fn profile() -> String {
    std::env::var("CONTINUITY_PROFILE").unwrap_or_else(|_| "default".to_string())
}

fn main() -> anyhow::Result<()> {
    init_logging();

    // Without declaring DPI awareness, Windows silently DPI-virtualizes
    // this whole process on any scaled display (the large majority of
    // real machines) — `GetSystemMetrics`/`GetDC`/`BitBlt` in
    // `remote_control_windows.rs`'s screen capture all see a scaled,
    // virtualized view of the screen rather than the real one, which is
    // a well-documented source of GDI capture behaving inconsistently or
    // failing outright depending on the exact scale factor. Best set as
    // early as possible, before any window or device context is created.
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        if let Err(e) = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
            tracing::debug!("couldn't set per-monitor DPI awareness: {e}");
        }
    }

    let event_loop = EventLoopBuilder::<SyncEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let device_name = continuity_daemon::default_device_name();

    let title_item = MenuItem::new(format!("Continuity — {device_name}"), false, None);
    let nearby_submenu = Submenu::new("Nearby Devices", true);
    let refresh_item = MenuItem::new("Refresh Nearby Devices", true, None);
    let send_submenu = Submenu::new("Send File", true);
    let forget_submenu = Submenu::new("Forget Device", true);
    #[cfg(feature = "remote-control")]
    let remote_control_submenu = Submenu::new("Remote Control", true);
    let pause_item = MenuItem::new("Pause Syncing", true, None);
    let reset_item = MenuItem::new("Reset...", true, None);
    let about_item = MenuItem::new(format!("About Continuity ({})", env!("CARGO_PKG_VERSION")), true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let refresh_item_id = refresh_item.id().clone();
    let pause_item_id = pause_item.id().clone();
    let reset_item_id = reset_item.id().clone();
    let about_item_id = about_item.id().clone();
    let quit_item_id = quit_item.id().clone();

    let connected_peers: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let send_target_map: Arc<Mutex<HashMap<MenuId, SendTarget>>> = Arc::new(Mutex::new(HashMap::new()));
    rebuild_send_menu(&send_submenu, &connected_peers.lock().unwrap(), &send_target_map);

    // "Forget" only offers currently-connected peers, same scope as "Send
    // File" above — there's no UI surface yet for interacting with a
    // paired-but-offline peer individually (only bulk `Reset` reaches
    // those). Separate map/submenu rather than reusing `send_target_map`
    // since a click here means something structurally different
    // (`EngineCommand::RevokeDevice`, not `SendFile`).
    let forget_target_map: Arc<Mutex<HashMap<MenuId, String>>> = Arc::new(Mutex::new(HashMap::new()));
    rebuild_forget_menu(&forget_submenu, &connected_peers.lock().unwrap(), &forget_target_map);

    // A connected peer's platform isn't tracked anywhere else on desktop
    // (`connected_peers` above only keeps id->name) — needed both to
    // gate which peers even get a "Remote Control" entry (any platform
    // technically *can* be offered one; an Android/iOS/Linux peer just
    // auto-declines via its `NoopRemoteControlHost`, so this is really
    // about not offering a button that's guaranteed to do nothing) and,
    // once a session's open, so the viewer knows which platform's key
    // codes to translate into.
    #[cfg(feature = "remote-control")]
    let connected_peer_platforms: Arc<Mutex<HashMap<String, Platform>>> = Arc::new(Mutex::new(HashMap::new()));
    #[cfg(feature = "remote-control")]
    let remote_control_target_map: Arc<Mutex<HashMap<MenuId, String>>> = Arc::new(Mutex::new(HashMap::new()));
    #[cfg(feature = "remote-control")]
    rebuild_remote_control_menu(
        &remote_control_submenu,
        &connected_peers.lock().unwrap(),
        &connected_peer_platforms.lock().unwrap(),
        &remote_control_target_map,
    );

    // Untrusted devices seen on the network (see SyncEvent::PeerDiscovered)
    // — the engine deliberately doesn't auto-dial these anymore (that used
    // to mean a pairing prompt for every Continuity install anyone ever
    // shared a LAN with), so this is the only way to initiate pairing with
    // a new device from desktop at all now. Clicking one sends
    // EngineCommand::ReconnectPeer, which dials it and runs the normal
    // pairing handshake.
    let nearby_peers: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let nearby_target_map: Arc<Mutex<HashMap<MenuId, String>>> = Arc::new(Mutex::new(HashMap::new()));
    rebuild_nearby_menu(&nearby_submenu, &nearby_peers.lock().unwrap(), &nearby_target_map);

    let menu = Menu::new();
    menu.append(&title_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&nearby_submenu)?;
    menu.append(&refresh_item)?;
    menu.append(&send_submenu)?;
    menu.append(&forget_submenu)?;
    #[cfg(feature = "remote-control")]
    menu.append(&remote_control_submenu)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&pause_item)?;
    menu.append(&reset_item)?;
    menu.append(&about_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Continuity")
        .with_icon(build_icon(false))
        .build()?;

    let commands = start_engine_thread(profile(), device_name, proxy)?;
    let is_paused: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    // The open remote-control viewer window, if any — at most one at a
    // time (mirrors the engine's own one-Controlled-session-device-wide
    // rule; nothing stops *this* device from controlling several peers
    // at once in principle, but one viewer window is enough for a first
    // version and keeps the window/event bookkeeping here simple).
    // `Rc<RefCell<_>>`, not `Arc<Mutex<_>>` — this only ever touches the
    // tao event loop's own thread, same as everything else in `main()`.
    #[cfg(feature = "remote-control")]
    let viewer: std::rc::Rc<std::cell::RefCell<Option<remote_viewer::RemoteViewer>>> = std::rc::Rc::new(std::cell::RefCell::new(None));

    event_loop.run(move |event, target, control_flow| {
        // Keep the tray icon (and its menu) alive for the app's lifetime —
        // dropping it removes the icon from the menu bar.
        let _tray_icon = &tray_icon;
        let _target = target;
        *control_flow = ControlFlow::Wait;

        // These match on `&event` (reference) rather than `event` — the
        // `UserEvent` handling below moves `event` by value to hand
        // `sync_event` off to `handle_sync_event`, so anything needing
        // `event` afterward has to run *before* that move, not after.
        #[cfg(feature = "remote-control")]
        if let tao::event::Event::WindowEvent { window_id, event: window_event, .. } = &event {
            let mut should_close = false;
            if let Some(v) = viewer.borrow_mut().as_mut() {
                if v.window_id() == *window_id {
                    if matches!(v.handle_window_event(window_event, &commands), remote_viewer::ViewerAction::Closed) {
                        should_close = true;
                    }
                }
            }
            if should_close {
                *viewer.borrow_mut() = None;
            }
        }
        #[cfg(feature = "remote-control")]
        if let tao::event::Event::RedrawRequested(window_id) = &event {
            if let Some(v) = viewer.borrow_mut().as_mut() {
                if v.window_id() == *window_id {
                    v.redraw();
                }
            }
        }

        if let tao::event::Event::UserEvent(sync_event) = event {
            #[cfg(feature = "remote-control")]
            handle_remote_control_sync_event(&sync_event, target, &viewer, &connected_peer_platforms, &commands);
            handle_sync_event(
                sync_event,
                &send_submenu,
                &forget_submenu,
                &nearby_submenu,
                &pause_item,
                &connected_peers,
                &send_target_map,
                &forget_target_map,
                &nearby_peers,
                &nearby_target_map,
                &is_paused,
                &commands,
                &tray_icon,
            );
            #[cfg(feature = "remote-control")]
            rebuild_remote_control_menu(
                &remote_control_submenu,
                &connected_peers.lock().unwrap(),
                &connected_peer_platforms.lock().unwrap(),
                &remote_control_target_map,
            );
        }

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == pause_item_id {
                let currently_paused = *is_paused.lock().unwrap();
                let _ = commands.send(EngineCommand::SetPaused(!currently_paused));
            } else if event.id == refresh_item_id {
                let _ = commands.send(EngineCommand::RefreshDiscovery);
            } else if event.id == reset_item_id {
                confirm(
                    "Continuity — Reset",
                    "This disconnects every paired device and forgets them all. Each one will need to be paired again from scratch.\n\nAre you sure?",
                    commands.clone(),
                    |commands| { let _ = commands.send(EngineCommand::Reset); },
                );
            } else if event.id == about_item_id {
                notify(&format!(
                    "Continuity {}\n\nClipboard, file, and remote-control continuity across your devices — no cloud relay, no account.\n\nhttps://github.com/hardope/continuity",
                    env!("CARGO_PKG_VERSION")
                ));
            } else if event.id == quit_item_id {
                *control_flow = ControlFlow::Exit;
            } else if let Some(target) = send_target_map.lock().unwrap().get(&event.id).cloned() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    let path = path.display().to_string();
                    match target {
                        SendTarget::Peer(peer_id) => {
                            let _ = commands.send(EngineCommand::SendFile { peer_crypto_id: peer_id, path });
                        }
                        SendTarget::All => {
                            let peers: Vec<String> = connected_peers.lock().unwrap().keys().cloned().collect();
                            for peer_id in peers {
                                let _ = commands.send(EngineCommand::SendFile {
                                    peer_crypto_id: peer_id,
                                    path: path.clone(),
                                });
                            }
                        }
                    }
                }
            } else if let Some(peer_id) = nearby_target_map.lock().unwrap().get(&event.id).cloned() {
                let _ = commands.send(EngineCommand::ReconnectPeer { peer_crypto_id: peer_id });
            } else if let Some(peer_id) = forget_target_map.lock().unwrap().get(&event.id).cloned() {
                let peer_name = connected_peers.lock().unwrap().get(&peer_id).cloned().unwrap_or_else(|| peer_id.clone());
                confirm(
                    "Continuity — Forget Device",
                    &format!("Forget '{peer_name}'? This closes the connection now and it'll need to be paired again from scratch to reconnect.\n\nAre you sure?"),
                    commands.clone(),
                    move |commands| { let _ = commands.send(EngineCommand::RevokeDevice { peer_crypto_id: peer_id }); },
                );
            } else {
                #[cfg(feature = "remote-control")]
                if let Some(peer_id) = remote_control_target_map.lock().unwrap().get(&event.id).cloned() {
                    let _ = commands.send(EngineCommand::RequestRemoteControl { peer_crypto_id: peer_id });
                }
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
    send_submenu: &Submenu,
    forget_submenu: &Submenu,
    nearby_submenu: &Submenu,
    pause_item: &MenuItem,
    connected_peers: &Arc<Mutex<HashMap<String, String>>>,
    send_target_map: &Arc<Mutex<HashMap<MenuId, SendTarget>>>,
    forget_target_map: &Arc<Mutex<HashMap<MenuId, String>>>,
    nearby_peers: &Arc<Mutex<HashMap<String, String>>>,
    nearby_target_map: &Arc<Mutex<HashMap<MenuId, String>>>,
    is_paused: &Arc<Mutex<bool>>,
    commands: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    tray_icon: &tray_icon::TrayIcon,
) {
    match event {
        SyncEvent::Listening { port } => tracing::info!("listening on port {port}"),
        SyncEvent::PairingRequested { peer, code } => {
            // A modal `rfd::MessageDialog` is a brand-new top-level GTK
            // window with no parent, created from a background daemon
            // rather than in direct response to a user click — GNOME/
            // Mutter's Wayland focus-stealing prevention can simply
            // never present a window like that at all. Confirmed for
            // real on a live GNOME 50/Wayland session: the dialog's own
            // `.show()` call never returned, leaving the pairing request
            // stuck forever with no visible prompt. A desktop
            // notification with actions goes through the OS's own
            // notification server instead of a raw app window,
            // sidestepping that restriction — same approach
            // `notify_file_received` above already uses successfully.
            // macOS/Windows keep the plain dialog, which testing this
            // session confirmed works fine there.
            #[cfg(target_os = "linux")]
            {
                tracing::debug!("PairingRequested: showing the confirm notification for '{}' (code {code})", peer.name);
                let peer_id = peer.id.clone();
                let commands = commands.clone();
                match notify_rust::Notification::new()
                    .summary("Continuity — Pairing Request")
                    .body(&format!(
                        "'{}' wants to pair.\n\nConfirmation code: {code}\n\nDoes this match the code shown on the other device?",
                        peer.name
                    ))
                    .action("accept", "Yes, pair")
                    .action("decline", "No")
                    // Critical + Never: this needs a real answer, not
                    // something that silently vanishes after a few
                    // seconds while the user's still reading the code.
                    .urgency(notify_rust::Urgency::Critical)
                    .timeout(notify_rust::Timeout::Never)
                    .show()
                {
                    Ok(handle) => {
                        std::thread::spawn(move || {
                            let mut accepted = false;
                            handle.wait_for_action(|action| {
                                accepted = action == "accept" || action == "default";
                            });
                            tracing::debug!("PairingRequested: notification action resolved, accepted={accepted}");
                            let _ = commands.send(EngineCommand::ConfirmPairing { peer_crypto_id: peer_id, accept: accepted });
                        });
                    }
                    Err(e) => {
                        tracing::warn!("couldn't show the pairing-confirmation notification, falling back to a dialog: {e}");
                        let result = rfd::MessageDialog::new()
                            .set_title("Continuity — Pairing Request")
                            .set_description(format!(
                                "'{}' wants to pair.\n\nConfirmation code: {code}\n\nDoes this match the code shown on the other device?",
                                peer.name
                            ))
                            .set_buttons(rfd::MessageButtons::YesNo)
                            .show();
                        let accepted = matches!(result, rfd::MessageDialogResult::Yes);
                        let _ = commands.send(EngineCommand::ConfirmPairing { peer_crypto_id: peer_id, accept: accepted });
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
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
        }
        // Paired/Disconnected/ClipboardReceived are routine — they'd fire
        // constantly on an active mesh and a popup for each one is just
        // noise. Still logged for debugging, just not shown.
        SyncEvent::Paired { peer } => tracing::info!("paired with '{}'", peer.name),
        SyncEvent::PairingDeclined { peer_name } => {
            notify(&format!("Pairing with '{peer_name}' was declined"));
        }
        SyncEvent::Connected { peer } => {
            // No longer "nearby, not yet connected" once it's connected.
            nearby_peers.lock().unwrap().remove(&peer.id);
            rebuild_nearby_menu(nearby_submenu, &nearby_peers.lock().unwrap(), nearby_target_map);
            connected_peers.lock().unwrap().insert(peer.id, peer.name);
            rebuild_send_menu(send_submenu, &connected_peers.lock().unwrap(), send_target_map);
            rebuild_forget_menu(forget_submenu, &connected_peers.lock().unwrap(), forget_target_map);
        }
        SyncEvent::PeerDiscovered { device } => {
            nearby_peers.lock().unwrap().insert(device.id, device.name);
            rebuild_nearby_menu(nearby_submenu, &nearby_peers.lock().unwrap(), nearby_target_map);
        }
        SyncEvent::Disconnected { peer_id, peer_name } => {
            connected_peers.lock().unwrap().remove(&peer_id);
            rebuild_send_menu(send_submenu, &connected_peers.lock().unwrap(), send_target_map);
            rebuild_forget_menu(forget_submenu, &connected_peers.lock().unwrap(), forget_target_map);
            tracing::info!("'{peer_name}' disconnected");
        }
        SyncEvent::ClipboardReceived { from_name } => {
            tracing::info!("clipboard synced from '{from_name}'");
        }
        SyncEvent::ClipboardBroadcast { .. } => {}
        SyncEvent::FileReceiving { from_name, file_name, .. } => {
            notify(&format!("Receiving '{file_name}' from '{from_name}'..."));
        }
        SyncEvent::FileReceived { file_name, path, .. } => notify_file_received(&file_name, &path),
        SyncEvent::FileSent { file_name, to_name, .. } => {
            notify(&format!("Sent '{file_name}' to '{to_name}'"));
        }
        SyncEvent::FileTransferFailed { reason, .. } => {
            notify(&format!("File transfer failed: {reason}"));
        }
        SyncEvent::Error(e) => tracing::warn!("{e}"),
        SyncEvent::WasReset => {
            connected_peers.lock().unwrap().clear();
            rebuild_send_menu(send_submenu, &connected_peers.lock().unwrap(), send_target_map);
            rebuild_forget_menu(forget_submenu, &connected_peers.lock().unwrap(), forget_target_map);
            notify("All paired devices have been forgotten");
        }
        SyncEvent::WasRevoked { peer_id, peer_name } => {
            // Unlike `Disconnected` (which keeps the entry, just marked
            // offline, so it can be reconnected with one click) this
            // removes it outright — the device is forgotten, not just
            // temporarily unreachable, and reconnecting now means pairing
            // again from scratch.
            connected_peers.lock().unwrap().remove(&peer_id);
            rebuild_send_menu(send_submenu, &connected_peers.lock().unwrap(), send_target_map);
            rebuild_forget_menu(forget_submenu, &connected_peers.lock().unwrap(), forget_target_map);
            notify(&format!("Forgot '{peer_name}'"));
        }
        SyncEvent::RevokedByPeer { peer_name, .. } => {
            notify(&format!("'{peer_name}' removed this device — pair again to reconnect"));
        }
        SyncEvent::PausedStateChanged { paused } => {
            *is_paused.lock().unwrap() = paused;
            pause_item.set_text(if paused { "Resume Syncing" } else { "Pause Syncing" });
        }
        SyncEvent::ReconnectFailed { peer_id } => {
            tracing::debug!("reconnect to '{peer_id}' failed: no known address");
        }
        SyncEvent::NowPlayingChanged { peer_name, info, .. } => {
            // No UI surface for this on desktop yet (tray-only, no window)
            // — logged for now. The interesting direction is mobile
            // displaying *this* device's now-playing info, not the tray
            // app displaying a peer's.
            tracing::debug!("'{peer_name}' now playing: {info:?}");
        }
        // No tray UI surface for this yet (see task #44 — Android-only so
        // far); fires once per connection per ping interval (30s), too
        // frequent to be worth a debug log line on its own.
        SyncEvent::PeerActivity { .. } => {}
        SyncEvent::RemoteControlRequested { peer_id, peer_name, .. } => {
            // Same shape as the pairing-request prompt above — a fresh,
            // explicit accept every time, deliberately not a "remember
            // this device" checkbox (see `Message::RemoteControlRequest`'s
            // doc comment in continuity-proto) — and the same Linux/
            // Wayland dialog-never-presented risk, since this is also
            // triggered by a network event, not a user click. See the
            // `PairingRequested` handler's comment for the full
            // explanation of why Linux gets a notification instead.
            #[cfg(target_os = "linux")]
            {
                let commands = commands.clone();
                match notify_rust::Notification::new()
                    .summary("Continuity — Remote Control Request")
                    .body(&format!("'{peer_name}' wants to control this device's keyboard, mouse, and screen."))
                    .action("accept", "Allow")
                    .action("decline", "Deny")
                    .urgency(notify_rust::Urgency::Critical)
                    .timeout(notify_rust::Timeout::Never)
                    .show()
                {
                    Ok(handle) => {
                        std::thread::spawn(move || {
                            let mut accept = false;
                            handle.wait_for_action(|action| {
                                accept = action == "accept" || action == "default";
                            });
                            let _ = commands.send(EngineCommand::RespondToRemoteControlRequest { peer_crypto_id: peer_id, accept });
                        });
                    }
                    Err(e) => {
                        tracing::warn!("couldn't show the remote-control-request notification, falling back to a dialog: {e}");
                        let result = rfd::MessageDialog::new()
                            .set_title("Continuity — Remote Control Request")
                            .set_description(format!("'{peer_name}' wants to control this device's keyboard, mouse, and screen.\n\nAllow it?"))
                            .set_buttons(rfd::MessageButtons::YesNo)
                            .show();
                        let accept = matches!(result, rfd::MessageDialogResult::Yes);
                        let _ = commands.send(EngineCommand::RespondToRemoteControlRequest { peer_crypto_id: peer_id, accept });
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let result = rfd::MessageDialog::new()
                    .set_title("Continuity — Remote Control Request")
                    .set_description(format!(
                        "'{peer_name}' wants to control this device's keyboard, mouse, and screen.\n\nAllow it?"
                    ))
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show();
                let accept = matches!(result, rfd::MessageDialogResult::Yes);
                let _ = commands.send(EngineCommand::RespondToRemoteControlRequest { peer_crypto_id: peer_id, accept });
            }
        }
        SyncEvent::RemoteControlDeclined { peer_name, .. } => {
            notify(&format!("'{peer_name}' declined the remote control request"));
        }
        SyncEvent::RemoteControlSessionStarted { peer_name, role, .. } => {
            let text = match role {
                continuity_daemon::RemoteControlRole::Controlled => {
                    format!("'{peer_name}' is now controlling this device — click here or use the peer's app to end it")
                }
                continuity_daemon::RemoteControlRole::Controlling => {
                    format!("remote control session with '{peer_name}' started")
                }
            };
            notify(&text);
            // The tray icon/taskbar is the one place that's visible
            // without hovering or opening the menu — a small red dot
            // makes an active session (in either role) obvious at a
            // glance, since it's easy to forget one's still running,
            // especially the Controlled side where nothing else on this
            // device changes at all.
            let _ = tray_icon.set_icon(Some(build_icon(true)));
            let _ = tray_icon.set_tooltip(Some(format!("Continuity — {text}")));
        }
        SyncEvent::RemoteControlSessionEnded { peer_name, reason, .. } => {
            let text = match reason {
                Some(reason) => format!("Remote control session with '{peer_name}' ended: {reason}"),
                None => format!("Remote control session with '{peer_name}' ended"),
            };
            notify(&text);
            let _ = tray_icon.set_icon(Some(build_icon(false)));
            let _ = tray_icon.set_tooltip(Some("Continuity"));
        }
        // The real routing to an open viewer window happens in
        // `handle_remote_control_sync_event` (feature-gated, needs the
        // window/softbuffer machinery) — this arm always compiles
        // regardless of the `remote-control` feature, so it can only
        // ever log, never touch a viewer.
        SyncEvent::ScreenFrameReceived { frame, .. } => {
            tracing::debug!("screen frame received ({} bytes)", frame.len());
        }
    }
}

/// Rebuilds the "Send File" submenu from scratch to match the current
/// connected-device set — simpler and less error-prone than trying to
/// incrementally patch individual entries in and out as devices connect
/// and disconnect in arbitrary order.
fn rebuild_send_menu(
    submenu: &Submenu,
    connected: &HashMap<String, String>,
    target_map: &Arc<Mutex<HashMap<MenuId, SendTarget>>>,
) {
    while submenu.remove_at(0).is_some() {}
    let mut map = target_map.lock().unwrap();
    map.clear();

    if connected.is_empty() {
        let _ = submenu.append(&MenuItem::new("No device connected", false, None));
        return;
    }

    let mut peers: Vec<(&String, &String)> = connected.iter().collect();
    peers.sort_by(|a, b| a.1.cmp(b.1));
    for (id, name) in &peers {
        let item = MenuItem::new(format!("Send to {name}..."), true, None);
        map.insert(item.id().clone(), SendTarget::Peer((*id).clone()));
        let _ = submenu.append(&item);
    }
    if peers.len() > 1 {
        let _ = submenu.append(&PredefinedMenuItem::separator());
        let broadcast = MenuItem::new(format!("Broadcast to All ({} devices)...", peers.len()), true, None);
        map.insert(broadcast.id().clone(), SendTarget::All);
        let _ = submenu.append(&broadcast);
    }
}

/// Same rebuild-from-scratch approach as `rebuild_send_menu`, same
/// currently-connected-only scope — no broadcast option, since forgetting
/// is inherently per-device and clicking one always shows its own
/// confirmation dialog before `EngineCommand::RevokeDevice` is sent.
fn rebuild_forget_menu(
    submenu: &Submenu,
    connected: &HashMap<String, String>,
    target_map: &Arc<Mutex<HashMap<MenuId, String>>>,
) {
    while submenu.remove_at(0).is_some() {}
    let mut map = target_map.lock().unwrap();
    map.clear();

    if connected.is_empty() {
        let _ = submenu.append(&MenuItem::new("No device connected", false, None));
        return;
    }

    let mut peers: Vec<(&String, &String)> = connected.iter().collect();
    peers.sort_by(|a, b| a.1.cmp(b.1));
    for (id, name) in &peers {
        let item = MenuItem::new(format!("Forget {name}..."), true, None);
        map.insert(item.id().clone(), (*id).clone());
        let _ = submenu.append(&item);
    }
}

/// Same rebuild-from-scratch approach as `rebuild_send_menu` — simpler
/// content (just "Connect to X", no broadcast option) since this is about
/// initiating a first connection, not fanning an action out to several
/// already-connected ones.
fn rebuild_nearby_menu(
    submenu: &Submenu,
    nearby: &HashMap<String, String>,
    target_map: &Arc<Mutex<HashMap<MenuId, String>>>,
) {
    while submenu.remove_at(0).is_some() {}
    let mut map = target_map.lock().unwrap();
    map.clear();

    if nearby.is_empty() {
        let _ = submenu.append(&MenuItem::new("No new devices nearby", false, None));
        return;
    }

    let mut peers: Vec<(&String, &String)> = nearby.iter().collect();
    peers.sort_by(|a, b| a.1.cmp(b.1));
    for (id, name) in &peers {
        let item = MenuItem::new(format!("Connect to {name}..."), true, None);
        map.insert(item.id().clone(), (*id).clone());
        let _ = submenu.append(&item);
    }
}

/// Same rebuild-from-scratch approach as `rebuild_send_menu`, same
/// currently-connected-only scope. Every connected peer gets an entry
/// regardless of platform (not just macOS/Windows) — an Android/iOS/Linux
/// peer's `NoopRemoteControlHost` auto-declines with a normal
/// `RemoteControlDeclined` notification rather than needing to be
/// filtered out here, and platform isn't even known for certain until a
/// peer's first `Connected` event carries it in.
#[cfg(feature = "remote-control")]
fn rebuild_remote_control_menu(
    submenu: &Submenu,
    connected: &HashMap<String, String>,
    _platforms: &HashMap<String, Platform>,
    target_map: &Arc<Mutex<HashMap<MenuId, String>>>,
) {
    while submenu.remove_at(0).is_some() {}
    let mut map = target_map.lock().unwrap();
    map.clear();

    if connected.is_empty() {
        let _ = submenu.append(&MenuItem::new("No device connected", false, None));
        return;
    }

    let mut peers: Vec<(&String, &String)> = connected.iter().collect();
    peers.sort_by(|a, b| a.1.cmp(b.1));
    for (id, name) in &peers {
        let item = MenuItem::new(format!("Remote Control {name}..."), true, None);
        map.insert(item.id().clone(), (*id).clone());
        let _ = submenu.append(&item);
    }
}

/// The subset of `SyncEvent` handling specific to the viewer window —
/// kept separate from `handle_sync_event` (which every build compiles,
/// remote-control or not) rather than threading `#[cfg]` through that
/// function's existing match arms.
#[cfg(feature = "remote-control")]
fn handle_remote_control_sync_event(
    event: &SyncEvent,
    target: &tao::event_loop::EventLoopWindowTarget<SyncEvent>,
    viewer: &std::rc::Rc<std::cell::RefCell<Option<remote_viewer::RemoteViewer>>>,
    connected_peer_platforms: &Arc<Mutex<HashMap<String, Platform>>>,
    commands: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
) {
    match event {
        SyncEvent::Connected { peer } => {
            connected_peer_platforms.lock().unwrap().insert(peer.id.clone(), peer.platform);
        }
        SyncEvent::Disconnected { peer_id, .. } => {
            connected_peer_platforms.lock().unwrap().remove(peer_id);
        }
        SyncEvent::WasRevoked { peer_id, .. } => {
            connected_peer_platforms.lock().unwrap().remove(peer_id);
        }
        SyncEvent::WasReset => {
            connected_peer_platforms.lock().unwrap().clear();
        }
        SyncEvent::RemoteControlSessionStarted { peer_id, peer_name, role, .. } => {
            if *role != continuity_daemon::RemoteControlRole::Controlling {
                return;
            }
            let platform = connected_peer_platforms.lock().unwrap().get(peer_id).copied().unwrap_or_else(|| {
                tracing::warn!("no known platform for peer '{peer_name}' ({peer_id}) — defaulting to macOS key codes, which will be wrong if it isn't one");
                Platform::MacOs
            });
            match remote_viewer::RemoteViewer::open(target, peer_id.clone(), peer_name, platform) {
                Ok(new_viewer) => *viewer.borrow_mut() = Some(new_viewer),
                Err(e) => {
                    tracing::warn!("couldn't open remote control viewer window: {e}");
                    let _ = commands.send(EngineCommand::EndRemoteControlSession { peer_crypto_id: peer_id.clone() });
                }
            }
        }
        SyncEvent::RemoteControlSessionEnded { peer_id, .. } => {
            let matches = viewer.borrow().as_ref().is_some_and(|v| v.peer_id() == peer_id);
            if matches {
                *viewer.borrow_mut() = None;
            }
        }
        SyncEvent::ScreenFrameReceived { peer_id, frame, .. } => {
            let mut viewer_ref = viewer.borrow_mut();
            let stuck = match viewer_ref.as_mut() {
                Some(v) if v.peer_id() == peer_id => {
                    v.handle_frame(frame);
                    v.is_stuck()
                }
                Some(v) => {
                    tracing::debug!(
                        "ScreenFrameReceived for peer '{peer_id}' ({} bytes) but the open viewer is for a different peer ('{}') — dropped",
                        frame.len(),
                        v.peer_id()
                    );
                    false
                }
                None => {
                    tracing::debug!("ScreenFrameReceived for peer '{peer_id}' ({} bytes) but no viewer window is open — dropped", frame.len());
                    false
                }
            };
            if stuck {
                tracing::warn!("remote-control viewer for '{peer_id}' never rendered a frame — giving up and ending the session");
                *viewer_ref = None;
                drop(viewer_ref);
                let _ = commands.send(EngineCommand::EndRemoteControlSession { peer_crypto_id: peer_id.clone() });
                notify("Couldn't display the remote screen — ending the session");
            }
        }
        _ => {}
    }
}

/// Asks a yes/no question and runs `on_yes` if the answer is yes.
///
/// On Linux this goes through a critical, non-expiring desktop
/// notification with Yes/No actions rather than a blocking
/// `rfd::MessageDialog` — confirmed for real on a live GNOME 50/Wayland
/// session that the plain dialog's `.show()` call can simply never
/// return at all (GNOME/Mutter's Wayland focus-stealing prevention can
/// refuse to present a brand-new top-level window), which doesn't just
/// break that one confirmation: since every menu click and engine event
/// is handled on the same single thread, one permanently-blocked dialog
/// call freezes the *entire* tray app, including unrelated things like
/// Quit. macOS/Windows keep the plain, synchronous dialog, which testing
/// this session confirmed works fine there.
fn confirm(
    title: &str,
    description: &str,
    commands: tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    on_yes: impl FnOnce(&tokio::sync::mpsc::UnboundedSender<EngineCommand>) + Send + 'static,
) {
    #[cfg(target_os = "linux")]
    {
        match notify_rust::Notification::new()
            .summary(title)
            .body(description)
            .action("accept", "Yes")
            .action("decline", "No")
            .urgency(notify_rust::Urgency::Critical)
            .timeout(notify_rust::Timeout::Never)
            .show()
        {
            Ok(handle) => {
                std::thread::spawn(move || {
                    let mut accepted = false;
                    handle.wait_for_action(|action| {
                        accepted = action == "accept" || action == "default";
                    });
                    if accepted {
                        on_yes(&commands);
                    }
                });
            }
            Err(e) => {
                tracing::warn!("couldn't show the '{title}' confirmation notification, falling back to a dialog: {e}");
                let result = rfd::MessageDialog::new().set_title(title).set_description(description).set_buttons(rfd::MessageButtons::YesNo).show();
                if matches!(result, rfd::MessageDialogResult::Yes) {
                    on_yes(&commands);
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let result = rfd::MessageDialog::new().set_title(title).set_description(description).set_buttons(rfd::MessageButtons::YesNo).show();
        if matches!(result, rfd::MessageDialogResult::Yes) {
            on_yes(&commands);
        }
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

/// A file-received notification is worth acting on immediately, so it gets
/// "Open" / "Show in Folder" actions instead of just announcing itself.
/// `wait_for_action` blocks until the user interacts (or the notification
/// times out/closes), so it runs on its own thread rather than the tao
/// event loop's thread — blocking that would freeze the whole tray app
/// until the notification was dismissed.
fn notify_file_received(file_name: &str, path: &str) {
    tracing::info!("received '{file_name}' at {path}");
    let path = path.to_string();
    match notify_rust::Notification::new()
        .summary("Continuity")
        .body(&format!("Received '{file_name}'"))
        .action("default", "Open")
        .action("show-folder", "Show in Folder")
        .show()
    {
        Ok(handle) => {
            std::thread::spawn(move || {
                handle.wait_for_action(|action| match action {
                    "default" => {
                        if let Err(e) = open_path(&path) {
                            tracing::debug!("couldn't open '{path}': {e}");
                        }
                    }
                    "show-folder" => {
                        if let Err(e) = reveal_in_folder(&path) {
                            tracing::debug!("couldn't reveal '{path}': {e}");
                        }
                    }
                    _ => {}
                });
            });
        }
        Err(e) => tracing::debug!("notification not shown (expected for an unbundled binary): {e}"),
    }
}

fn open_path(path: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd").args(["/C", "start", "", path]).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
    }
    Ok(())
}

fn reveal_in_folder(path: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").args(["-R", path]).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(format!("/select,{path}")).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        // No universal "reveal and select" on Linux — opening the
        // containing folder is the practical equivalent.
        let dir = std::path::Path::new(path).parent().unwrap_or_else(|| std::path::Path::new("."));
        std::process::Command::new("xdg-open").arg(dir).spawn()?;
    }
    Ok(())
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
                #[cfg(target_os = "macos")]
                let media: Arc<dyn continuity_daemon::MediaController> = Arc::new(media_mac::MacMediaController);
                #[cfg(target_os = "windows")]
                let media: Arc<dyn continuity_daemon::MediaController> = Arc::new(media_windows::WindowsMediaController);
                #[cfg(target_os = "linux")]
                let media: Arc<dyn continuity_daemon::MediaController> = Arc::new(media_linux::LinuxMediaController);
                #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
                let media: Arc<dyn continuity_daemon::MediaController> = Arc::new(continuity_daemon::NoopMediaController);

                // Real remote-control hosts only exist behind the
                // `remote-control` Cargo feature (see Cargo.toml) — off,
                // and every unsupported platform wires in the `Noop` host,
                // which is how a "lite" build stays lite: the heavy
                // platform capture/injection code and the permissions it
                // triggers (Screen Recording, Accessibility/Input
                // Monitoring, the xdg-desktop-portal dialog) are compiled
                // out entirely, not just hidden behind a disabled UI
                // toggle.
                #[cfg(all(target_os = "macos", feature = "remote-control"))]
                let remote_control: Arc<dyn continuity_daemon::RemoteControlHost> = Arc::new(remote_control_mac::MacRemoteControlHost::new());
                #[cfg(all(target_os = "windows", feature = "remote-control"))]
                let remote_control: Arc<dyn continuity_daemon::RemoteControlHost> =
                    Arc::new(remote_control_windows::WindowsRemoteControlHost::new());
                #[cfg(all(target_os = "linux", feature = "remote-control"))]
                let remote_control: Arc<dyn continuity_daemon::RemoteControlHost> =
                    Arc::new(remote_control_linux::LinuxRemoteControlHost::new());
                #[cfg(not(all(feature = "remote-control", any(target_os = "macos", target_os = "windows", target_os = "linux"))))]
                let remote_control: Arc<dyn continuity_daemon::RemoteControlHost> = Arc::new(continuity_daemon::NoopRemoteControlHost);

                let config = EngineConfig {
                    identity,
                    device_name,
                    trust_store,
                    clipboard: Arc::new(ArboardClipboard),
                    media,
                    remote_control,
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

/// Every previous version of this only ever logged to stdout — useless
/// on a release build, which sets `windows_subsystem = "windows"` (see
/// the top of this file) specifically so launching doesn't pop a console
/// window; a GUI-subsystem process has no console attached to write to
/// regardless of how it's launched, terminal or not. Diagnosing a real
/// report meant guessing blind — no way to ask "what does the log say."
/// This always *also* writes to a plain file, independent of subsystem
/// or platform, so a user can be asked to just find and share that
/// file's content instead.
fn init_logging() {
    use tracing_subscriber::prelude::*;

    // Deliberately its *own* filter, independent of `RUST_LOG` — getting
    // a user to correctly thread an env var through a manual relaunch
    // turned out to be a real, repeated source of friction in practice
    // (shell quoting, launching from the wrong window, a fresh terminal
    // not picking up a variable set in another one — every variant of
    // "did the env var actually reach the process" showed up at least
    // once). The file is the diagnostic path specifically, so it always
    // captures this app's own debug-level detail regardless of whether
    // anyone remembered to set anything; third-party crates stay at
    // `info` so rustls/mdns_sd's own considerable chattiness doesn't
    // drown it out.
    let file_layer = log_file_path(&profile())
        .and_then(|path| {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()
        })
        .map(|file| {
            tracing_subscriber::fmt::layer()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_filter(tracing_subscriber::EnvFilter::new("info,continuity_daemon=debug,continuityd=debug"))
        });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(tracing_subscriber::EnvFilter::from_default_env()))
        .with(file_layer)
        .init();
}

/// Same directory `continuity-crypto::TrustStore` already uses for its
/// own config, so logs live right next to it rather than inventing a
/// second, unrelated location a user would have to be told about
/// separately.
fn log_file_path(profile: &str) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("app", "continuity", "continuity")?;
    let file_name = if profile == "default" { "continuityd.log".to_string() } else { format!("continuityd.{profile}.log") };
    Some(dirs.config_dir().join(file_name))
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

/// Same mark as `assets/logo.svg` (and the Android VectorDrawables),
/// computed per-pixel rather than rasterized from an embedded asset —
/// transparent background (a tray icon needs to sit on whatever the OS
/// menu bar/taskbar looks like, not carry its own backdrop), two 160° arcs
/// orbiting a shared center with a 20° gap on either side, each capped
/// with a dot at its leading edge. The second arc is the first rotated
/// 180°, so together they read as one broken ring. Angles follow the same
/// convention the SVG's `<circle>` traces implicitly: 0° at the rightmost
/// point, increasing clockwise (natural here since screen-space y already
/// points down, so `atan2(dy, dx)` sweeps clockwise without needing to
/// flip anything). Keep the geometry in sync if the mark ever changes:
/// center (50,50), radius 30, stroke width 9, dots radius 8.5 at (21.81,
/// 60.26) and (78.19,39.74), all in the same nominal 100-unit space as
/// the SVG/VectorDrawables. 4x supersampling anti-aliases the edges —
/// without it this is visibly jaggy at menu-bar sizes, since the OS
/// doesn't smooth a raw RGBA buffer the way it smooths a vector drawable.
fn build_icon(remote_control_active: bool) -> Icon {
    let size: u32 = 64;
    let mut rgba = render_icon_rgba(size);
    if remote_control_active {
        overlay_active_dot(&mut rgba, size);
    }
    Icon::from_rgba(rgba, size, size).expect("generated icon buffer is well-formed")
}

/// A solid red dot in the bottom-right corner — the "remote control is
/// live" indicator asked for on the tray icon, since neither Windows'
/// taskbar nor macOS's menu bar has any other always-visible (not
/// requiring a hover or a click) place to show that without it. Drawn as
/// a post-processing pass over the already-rendered base icon rather than
/// threading an `active` flag through `render_icon_rgba`'s own
/// supersampling math — simpler, and the two concerns (draw the mark,
/// decide whether to show it) don't need to be tangled together.
fn overlay_active_dot(rgba: &mut [u8], size: u32) {
    let cx = size as f32 * 0.82;
    let cy = size as f32 * 0.82;
    let radius = size as f32 * 0.16;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if (dx * dx + dy * dy).sqrt() <= radius {
                let i = ((y * size + x) * 4) as usize;
                rgba[i] = 0xef;
                rgba[i + 1] = 0x44;
                rgba[i + 2] = 0x44;
                rgba[i + 3] = 0xff;
            }
        }
    }
}

fn render_icon_rgba(size: u32) -> Vec<u8> {
    let supersample: u32 = 4;
    let scale = size as f32 / 100.0;
    let cx = 50.0 * scale;
    let cy = 50.0 * scale;
    let ring_radius = 30.0 * scale;
    let stroke_half = 4.5 * scale;
    let dot_radius = 8.5 * scale;
    let arc_span_deg = 160.0_f32;
    let dots = [
        (21.81 * scale, 60.26 * scale, (0x38, 0xbd, 0xf8)),
        (78.19 * scale, 39.74 * scale, (0xa8, 0x55, 0xf7)),
    ];
    // Diagonal gradient per arc, approximated over the icon's own bounds
    // rather than each arc's individual bounding box — close enough at
    // this size, and much simpler than replicating SVG's objectBoundingBox
    // gradient units exactly.
    let arcs = [
        (0.0_f32, (0x38, 0xbd, 0xf8), (0x25, 0x63, 0xeb)), // blue: 0°-160°
        (180.0_f32, (0x6d, 0x28, 0xd9), (0xa8, 0x55, 0xf7)), // purple: 180°-340°
    ];

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut coverage: u32 = 0;
            let mut color_sum = (0u32, 0u32, 0u32);
            for sy in 0..supersample {
                for sx in 0..supersample {
                    let px = x as f32 + (sx as f32 + 0.5) / supersample as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / supersample as f32;
                    let dx = px - cx;
                    let dy = py - cy;
                    let dist_from_ring = ((dx * dx + dy * dy).sqrt() - ring_radius).abs();
                    let angle = dy.atan2(dx).to_degrees().rem_euclid(360.0);

                    let in_arc = dist_from_ring <= stroke_half
                        && arcs.iter().any(|&(start, _, _)| {
                            let rel = (angle - start).rem_euclid(360.0);
                            rel <= arc_span_deg
                        });
                    let in_dot = dots.iter().any(|&(dcx, dcy, _)| {
                        let ddx = px - dcx;
                        let ddy = py - dcy;
                        (ddx * ddx + ddy * ddy).sqrt() <= dot_radius
                    });

                    if in_arc || in_dot {
                        coverage += 1;
                        let t = ((px + py) / (2.0 * size as f32)).clamp(0.0, 1.0);
                        let (r, g, b) = if in_dot {
                            dots.iter()
                                .find(|&&(dcx, dcy, _)| {
                                    let ddx = px - dcx;
                                    let ddy = py - dcy;
                                    (ddx * ddx + ddy * ddy).sqrt() <= dot_radius
                                })
                                .map(|&(_, _, c)| c)
                                .unwrap()
                        } else {
                            let (_, start_c, end_c) = arcs
                                .iter()
                                .find(|&&(start, _, _)| {
                                    let rel = (angle - start).rem_euclid(360.0);
                                    rel <= arc_span_deg
                                })
                                .copied()
                                .unwrap();
                            lerp_color(start_c, end_c, t)
                        };
                        color_sum.0 += r as u32;
                        color_sum.1 += g as u32;
                        color_sum.2 += b as u32;
                    }
                }
            }
            let total = supersample * supersample;
            let alpha = (255 * coverage / total) as u8;
            let (r, g, b) = if coverage > 0 {
                ((color_sum.0 / coverage) as u8, (color_sum.1 / coverage) as u8, (color_sum.2 / coverage) as u8)
            } else {
                (0, 0, 0)
            };
            rgba.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    rgba
}

fn lerp_color(start: (u8, u8, u8), end: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let lerp = |a: u8, b: u8| -> u8 { (a as f32 + (b as f32 - a as f32) * t) as u8 };
    (lerp(start.0, end.0), lerp(start.1, end.1), lerp(start.2, end.2))
}

#[cfg(test)]
mod icon_preview {
    // Not a real test — a manual visual-verification aid. Run with
    // `cargo test -p continuityd render_icon_to_ppm -- --ignored --nocapture`
    // to dump the rendered tray icon to a .ppm (composited onto a mid-gray
    // background, since PPM has no alpha channel) for eyeballing before
    // trusting the geometry math. Deliberately `#[ignore]`d so it never
    // runs as part of a normal test pass.
    #[test]
    #[ignore]
    fn render_icon_to_ppm() {
        let size = 256u32;
        let rgba = super::render_icon_rgba(size);
        let path = std::env::var("ICON_PREVIEW_OUT").unwrap_or_else(|_| "/tmp/continuity_icon_preview.ppm".to_string());
        let mut out = format!("P6\n{size} {size}\n255\n").into_bytes();
        for chunk in rgba.chunks(4) {
            let (r, g, b, a) = (chunk[0] as f32, chunk[1] as f32, chunk[2] as f32, chunk[3] as f32 / 255.0);
            let bg = 128.0;
            out.push((r * a + bg * (1.0 - a)) as u8);
            out.push((g * a + bg * (1.0 - a)) as u8);
            out.push((b * a + bg * (1.0 - a)) as u8);
        }
        std::fs::write(&path, out).unwrap();
        println!("wrote {path}");
    }
}

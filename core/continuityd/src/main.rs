// Without this, a normal Rust binary on Windows defaults to the "console"
// subsystem, so launching continuityd.exe opens a cmd window alongside
// the tray icon even though nothing is ever printed to it. Gated on
// release builds so `cargo run` locally still gives you a console for
// `tracing` output; in debug the attribute would just get in the way of
// day-to-day dev.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Desktop app — macOS/Windows/Linux. A thin GUI shell over
//! `continuity-daemon`'s engine: a tray icon with a few quick actions, and
//! the Continuity window for everything else (settings_window.rs on Linux
//! and Windows, a SwiftUI app of its own on macOS). The engine runs on a
//! background OS thread with its own tokio runtime (tao's event loop needs
//! the real main thread), and the two talk over a channel in each
//! direction — engine events wake the tray thread via an `EventLoopProxy`,
//! and menu clicks send `EngineCommand`s straight into the engine's command
//! channel (`UnboundedSender::send` is synchronous, so no proxy needed that
//! way).

#[cfg(target_os = "macos")]
mod media_mac;
#[cfg(target_os = "windows")]
mod media_windows;
#[cfg(target_os = "linux")]
mod media_linux;
#[cfg(target_os = "linux")]
mod file_chooser_linux;
#[cfg(target_os = "macos")]
mod notifications_mac;
#[cfg(target_os = "macos")]
mod permissions_mac;
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
mod frame_encoding;
#[cfg(feature = "remote-control")]
mod remote_viewer;
#[cfg(target_os = "linux")]
mod screen_lock_linux;
#[cfg(target_os = "macos")]
mod screen_lock_mac;
#[cfg(target_os = "windows")]
mod screen_lock_windows;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod settings_window;
mod control;
mod log_file;
mod share;

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

/// What wakes the tray's event loop, besides the OS itself.
enum AppEvent {
    Engine(SyncEvent),
    /// From the Continuity window's page (Linux and Windows — macOS has an
    /// app of its own for the window).
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Settings(settings_window::Message),
    /// Time to make the Continuity window, hidden (see
    /// `SETTINGS_PREPARE_DELAY`).
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    PrepareSettings,
    /// Show the Continuity window: the very first launch, or Continuity
    /// launched again while it's already running (see share.rs's
    /// `hand_off_while_starting`).
    OpenWindow,
    /// The remote-control viewer's decoder thread has a frame ready to
    /// show — at most one of these is ever in flight (see remote_viewer.rs).
    /// Screen frames themselves never come through here.
    #[cfg(feature = "remote-control")]
    ViewerFrameReady,
}

/// Where the engine thread sends screen frames instead of the event loop
/// (see `start_engine_thread`): the viewer's frame router, or nowhere in a
/// build without the viewer.
#[cfg(feature = "remote-control")]
type FrameSink = Arc<remote_viewer::FrameRouter>;
#[cfg(not(feature = "remote-control"))]
type FrameSink = ();

/// A webview takes a few seconds to start, so on Linux and Windows the
/// Continuity window is made, hidden, this long after Continuity starts —
/// late enough not to slow its own start (or a login) down — and Open
/// Continuity then only has to show it.
#[cfg(any(target_os = "linux", target_os = "windows"))]
const SETTINGS_PREPARE_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

/// Who Continuity is to Windows's notifications: the AppUserModelID it
/// files toasts under (and the installer gives the Start-menu shortcut — see
/// installers/windows/continuity.iss). The same string as the macOS bundle
/// identifier in installers/macos/Info.plist, which is what macOS goes by.
#[cfg(target_os = "windows")]
const APP_ID: &str = "app.continuity.desktop";

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
    // What the file manager's "Send with Continuity" entries run (see
    // share.rs): hand the files to the already-running instance and exit,
    // without ever starting a second tray app or touching its log file.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(share::SHARE_TO_FLAG) {
        std::process::exit(share::run_share_client(&profile(), &args[2..]));
    }
    // One Continuity per profile, settled before anything that takes time
    // (see `share::claim_instance`). Launched again while already running —
    // from the app menu or the Start menu, say — the running one shows its
    // window instead, once it's far enough along to, and this one goes
    // away. (macOS reactivates a running app by itself, see
    // `Event::Reopen`; this only comes up there for a terminal launch.)
    let instance = loop {
        match share::claim_instance(&profile()) {
            share::InstanceClaim::First(lock) => break lock,
            share::InstanceClaim::Running => match share::hand_off_while_starting(&profile()) {
                share::HandOff::Done => std::process::exit(0),
                share::HandOff::NotAnswering => {
                    eprintln!("Continuity is already running, but didn't answer — try again in a moment");
                    std::process::exit(1);
                }
                // It quit meanwhile: this launch takes over.
                share::HandOff::Gone => continue,
            },
        }
    };

    init_logging();
    init_notifications();

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

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let settings_route = settings_window::Route { proxy: event_loop.create_proxy(), wrap: AppEvent::Settings };
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        let prepare = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(SETTINGS_PREPARE_DELAY);
            let _ = prepare.send_event(AppEvent::PrepareSettings);
        });
    }

    let device_name = continuity_daemon::default_device_name();

    // Only quick actions — everything else (pairing, nearby devices,
    // forgetting one, its permissions, forgetting all of them, remote
    // control, about) is in the Continuity window, which the first entry
    // opens: on macOS the SwiftUI app bundled with this one
    // (apps/macos/ContinuitySettings), on Linux and Windows a webview
    // window of this process (settings_window.rs).
    let open_item = MenuItem::new("Open Continuity", true, None);
    let send_submenu = Submenu::new("Send File", true);
    let pause_item = MenuItem::new("Pause Syncing", true, None);
    let quit_item = MenuItem::new("Quit Continuity", true, None);
    let open_item_id = open_item.id().clone();
    let pause_item_id = pause_item.id().clone();
    let quit_item_id = quit_item.id().clone();

    let connected_peers: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let send_target_map: Arc<Mutex<HashMap<MenuId, SendTarget>>> = Arc::new(Mutex::new(HashMap::new()));
    rebuild_send_menu(&send_submenu, &connected_peers.lock().unwrap(), &send_target_map);

    // A connected peer's platform, which `connected_peers` above doesn't
    // keep (only id->name) — once a remote-control session's open, the
    // viewer needs it to know which platform's key codes to translate into.
    #[cfg(feature = "remote-control")]
    let connected_peer_platforms: Arc<Mutex<HashMap<String, Platform>>> = Arc::new(Mutex::new(HashMap::new()));

    let menu = Menu::new();
    menu.append(&open_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&send_submenu)?;
    menu.append(&pause_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    // While files are on their way, a line about them heads the menu
    // (clicking it opens the window) — see `refresh_tray_transfers`.
    let transfer_item = MenuItem::new("", true, None);
    let transfer_item_id = transfer_item.id().clone();
    let mut tray_transfers = TrayTransfers::default();

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip("Continuity")
        .with_icon(build_icon(false));
    // Windows convention: a left click on a tray icon opens the app's
    // window, a right click shows its menu. macOS's menu-bar convention is
    // the menu on any click, and Linux's AppIndicator tray can't report
    // clicks at all — both keep the menu.
    #[cfg(target_os = "windows")]
    let tray_icon = tray_icon.with_menu_on_left_click(false);
    let tray_icon = tray_icon.build()?;

    // Shared by the engine thread, which hands it every screen frame, and
    // the viewer window below, which it feeds — see remote_viewer.rs.
    #[cfg(feature = "remote-control")]
    let frame_router = remote_viewer::FrameRouter::new();
    #[cfg(feature = "remote-control")]
    let frame_sink: FrameSink = frame_router.clone();
    #[cfg(not(feature = "remote-control"))]
    let frame_sink: FrameSink = ();
    // How the viewer's decoder thread wakes this loop when a frame is ready.
    #[cfg(feature = "remote-control")]
    let viewer_proxy = event_loop.create_proxy();
    // Where received files go, as chosen in the window (Contract C's
    // settings.json) — or the default when nothing was, or what was chosen
    // can't be used right now.
    let settings_path = control::settings_path(&profile());
    let app_settings = settings_path.as_deref().map(control::AppSettings::load).unwrap_or_default();
    let received_dir = received_files_dir_for_this_run(&profile(), &app_settings);
    let (commands, device_id) = start_engine_thread(profile(), device_name.clone(), proxy, frame_sink, received_dir.clone())?;
    // What the Continuity window reads and changes, over the same local
    // socket as "Send with Continuity" (see control.rs).
    let control = control::ControlState::new(
        control::ThisDevice {
            name: device_name,
            id: device_id,
            platform: control::this_platform(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        TrustStore::default_path(&profile()).unwrap_or_default(),
        received_dir,
        log_file_path(&profile()),
        settings_path,
        app_settings,
    );
    control.spawn_trust_watcher();
    control.spawn_received_dir_watcher(received_files_dir(&profile()), commands.clone());
    // Entries left behind by a previous run that didn't quit cleanly would
    // point at devices that may not be connected now — start from none and
    // let `Connected` events add them back.
    share::remove_menu_entries();
    // A second launch's "show the window", from the socket's own thread.
    let open_window_proxy = Mutex::new(event_loop.create_proxy());
    let open_window: share::OpenWindow = Arc::new(move || {
        let _ = open_window_proxy.lock().unwrap().send_event(AppEvent::OpenWindow);
    });
    let share_server = share::start_server(&profile(), commands.clone(), connected_peers.clone(), control.clone(), open_window);
    // The very first launch opens the window, so a new install doesn't look
    // like nothing happened (a tray icon is easy to miss).
    if control.first_window_showing() {
        let _ = event_loop.create_proxy().send_event(AppEvent::OpenWindow);
    }
    let is_paused: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    // Remote-control sessions in either role, as session id and the tray
    // tooltip that announced each, oldest first — the tray icon's red dot
    // stays while any is left.
    let mut remote_sessions: Vec<(String, String)> = Vec::new();
    // The open remote-control viewer window, if any — at most one at a
    // time (mirrors the engine's own one-Controlled-session-device-wide
    // rule; nothing stops *this* device from controlling several peers
    // at once in principle, but one viewer window is enough for a first
    // version and keeps the window/event bookkeeping here simple).
    // `Rc<RefCell<_>>`, not `Arc<Mutex<_>>` — this only ever touches the
    // tao event loop's own thread, same as everything else in `main()`.
    #[cfg(feature = "remote-control")]
    let viewer: std::rc::Rc<std::cell::RefCell<Option<remote_viewer::RemoteViewer>>> = std::rc::Rc::new(std::cell::RefCell::new(None));
    // Made hidden in the background, then shown by Open Continuity and
    // hidden again when closed.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let mut settings: Option<settings_window::SettingsWindow> = None;

    event_loop.run(move |event, target, control_flow| {
        // Keep the tray icon (and its menu) alive for the app's lifetime —
        // dropping it removes the icon from the menu bar — and the instance
        // lock, which a second launch looks for.
        let _tray_icon = &tray_icon;
        let _instance = &instance;
        let _target = target;
        *control_flow = ControlFlow::Wait;
        // Set by anything this tick that should bring the Continuity window
        // up — shown once, at the end.
        let mut open_window = false;
        // Reactive check: catches a viewer that was already open and
        // idle on the *previous* tick. Doesn't by itself guarantee a
        // next tick ever happens, though — see the `WaitUntil` set at
        // the bottom of this closure for that half.
        #[cfg(feature = "remote-control")]
        check_viewer_watchdog(&viewer, &commands);

        // These match on `&event` (reference) rather than `event` — the
        // `UserEvent` handling below moves `event` by value to hand
        // `sync_event` off to `handle_sync_event`, so anything needing
        // `event` afterward has to run *before* that move, not after.
        #[cfg(feature = "remote-control")]
        if let tao::event::Event::WindowEvent { window_id, event: window_event, .. } = &event {
            let mut should_close = false;
            if let Some(v) = viewer.borrow_mut().as_mut() {
                if v.window_id() == *window_id {
                    if matches!(v.handle_window_event(window_event), remote_viewer::ViewerAction::Closed) {
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

        #[cfg(any(target_os = "linux", target_os = "windows"))]
        if let tao::event::Event::WindowEvent { window_id, event: tao::event::WindowEvent::CloseRequested, .. } = &event {
            if let Some(window) = settings.as_ref().filter(|window| window.window_id() == *window_id) {
                window.hide();
            }
        }

        // Finder's "Open With > Continuity" (see share.rs and the
        // CFBundleDocumentTypes entry in installers/macos/Info.plist).
        #[cfg(target_os = "macos")]
        if let tao::event::Event::Opened { urls } = &event {
            let paths: Vec<PathBuf> = urls.iter().filter_map(|url| url.to_file_path().ok()).collect();
            share::handle_opened_files(paths, connected_peers.clone(), commands.clone());
        }
        // Continuity.app opened again while it's running — from Finder,
        // Launchpad or Spotlight. Nothing else would happen at all: it has
        // no Dock icon or window of its own to bring forward.
        #[cfg(target_os = "macos")]
        if let tao::event::Event::Reopen { .. } = &event {
            open_window = true;
        }

        match event {
            tao::event::Event::UserEvent(AppEvent::Engine(sync_event)) => {
                control.observe(&sync_event);
                let tray_line = match &sync_event {
                    SyncEvent::FileTransferProgress { .. } => Some(false),
                    SyncEvent::FileSending { .. }
                    | SyncEvent::FileReceiving { .. }
                    | SyncEvent::FileTransferStalled { .. }
                    | SyncEvent::FileSent { .. }
                    | SyncEvent::FileReceived { .. }
                    | SyncEvent::FileTransferFailed { .. }
                    | SyncEvent::RemoteControlSessionStarted { .. }
                    | SyncEvent::RemoteControlSessionEnded { .. } => Some(true),
                    _ => None,
                };
                #[cfg(feature = "remote-control")]
                handle_remote_control_sync_event(
                    &sync_event,
                    target,
                    &viewer,
                    &connected_peer_platforms,
                    &commands,
                    &frame_router,
                    &viewer_proxy,
                );
                handle_sync_event(
                    sync_event,
                    &send_submenu,
                    &pause_item,
                    &connected_peers,
                    &send_target_map,
                    &is_paused,
                    &commands,
                    &tray_icon,
                    &control,
                    &mut remote_sessions,
                );
                if let Some(changed) = tray_line {
                    refresh_tray_transfers(&control, &menu, &transfer_item, &tray_icon, &remote_sessions, &mut tray_transfers, changed);
                }
            }
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            tao::event::Event::UserEvent(AppEvent::Settings(message)) => {
                if let Some(window) = settings.as_mut() {
                    let context = settings_window::Context { control: &control, commands: &commands, connected: &connected_peers };
                    window.handle(message, &context, &settings_route);
                }
            }
            // Only ever picks up a frame the decoder thread already sized
            // for the window — the decoding never happens on this thread.
            #[cfg(feature = "remote-control")]
            tao::event::Event::UserEvent(AppEvent::ViewerFrameReady) => {
                if let Some(v) = viewer.borrow_mut().as_mut() {
                    v.show_ready_frame();
                }
            }
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            tao::event::Event::UserEvent(AppEvent::PrepareSettings) => {
                if settings.is_none() {
                    match settings_window::SettingsWindow::open(target, settings_route.clone(), settings_window_icon(), false) {
                        Ok(window) => settings = Some(window),
                        // Open Continuity tries again, and says so if that
                        // fails too.
                        Err(e) => tracing::warn!("couldn't prepare the Continuity window: {e}"),
                    }
                }
            }
            tao::event::Event::UserEvent(AppEvent::OpenWindow) => open_window = true,
            _ => {}
        }

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == open_item_id || event.id == transfer_item_id {
                open_window = true;
            } else if event.id == pause_item_id {
                let currently_paused = *is_paused.lock().unwrap();
                let _ = commands.send(EngineCommand::SetPaused(!currently_paused));
            } else if event.id == quit_item_id {
                share::remove_menu_entries();
                if let Some(server) = &share_server {
                    server.shutdown();
                }
                *control_flow = ControlFlow::Exit;
            } else if let Some(target) = send_target_map.lock().unwrap().get(&event.id).cloned() {
                // Linux: GTK's chooser, which answers later (see
                // file_chooser_linux.rs for why not rfd's).
                #[cfg(target_os = "linux")]
                {
                    let commands = commands.clone();
                    let connected_peers = connected_peers.clone();
                    file_chooser_linux::choose_files(None, "Send File", false, move |paths| {
                        for path in &paths {
                            send_file_to(&target, &path.display().to_string(), &commands, &connected_peers);
                        }
                    });
                }
                #[cfg(not(target_os = "linux"))]
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    send_file_to(&target, &path.display().to_string(), &commands, &connected_peers);
                }
            }
        }

        // Every tray-icon event waiting, not just one — hovering the icon
        // queues a stream of Enter/Move/Leave events nothing here needs.
        // Windows: a left click opens the window (see `with_menu_on_left_
        // click` above); acted on when the button comes back up, as a
        // click on a taskbar button is.
        while let Ok(tray_event) = TrayIconEvent::receiver().try_recv() {
            #[cfg(target_os = "windows")]
            if let TrayIconEvent::Click { button: tray_icon::MouseButton::Left, button_state: tray_icon::MouseButtonState::Up, .. } = tray_event {
                open_window = true;
            }
            let _ = tray_event;
        }

        if open_window {
            #[cfg(target_os = "macos")]
            open_settings(&profile());
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            show_window(&mut settings, target, &settings_route);
        }

        // Evaluated last, after everything above (including a viewer
        // window possibly just opened this very tick by the `UserEvent`
        // handling above) — a not-yet-rendered viewer needs this loop to
        // keep waking itself on a short timer, since a session whose
        // screen-stream connection never arrives produces neither a real
        // OS event nor a `UserEvent` to wake `ControlFlow::Wait` on its
        // own; without this, `check_viewer_watchdog` above would never
        // get a second chance to run and a permanently blank window
        // could sit open forever. Cleared back to plain `Wait` once a
        // frame lands (`is_rendered()` true) so this doesn't needlessly
        // keep polling for the rest of an otherwise-healthy session.
        #[cfg(feature = "remote-control")]
        if viewer.borrow().as_ref().is_some_and(|v| !v.is_rendered()) {
            *control_flow = ControlFlow::WaitUntil(std::time::Instant::now() + std::time::Duration::from_millis(500));
        }
    });
}

/// Shows the Continuity window on Linux and Windows — made now if it isn't
/// ready yet (see `SETTINGS_PREPARE_DELAY`), brought to the front if it is.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn show_window(
    window: &mut Option<settings_window::SettingsWindow>,
    target: &tao::event_loop::EventLoopWindowTarget<AppEvent>,
    route: &settings_window::Route<AppEvent>,
) {
    match window {
        Some(window) => window.show(),
        None => match settings_window::SettingsWindow::open(target, route.clone(), settings_window_icon(), true) {
            Ok(opened) => *window = Some(opened),
            Err(e) => {
                tracing::warn!("couldn't open the Continuity window: {e}");
                notify(&settings_window_failed(&e));
            }
        },
    }
}

fn handle_sync_event(
    event: SyncEvent,
    send_submenu: &Submenu,
    pause_item: &MenuItem,
    connected_peers: &Arc<Mutex<HashMap<String, String>>>,
    send_target_map: &Arc<Mutex<HashMap<MenuId, SendTarget>>>,
    is_paused: &Arc<Mutex<bool>>,
    commands: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    tray_icon: &tray_icon::TrayIcon,
    control: &control::ControlState,
    remote_sessions: &mut Vec<(String, String)>,
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
            connected_peers.lock().unwrap().insert(peer.id, peer.name);
            rebuild_send_menu(send_submenu, &connected_peers.lock().unwrap(), send_target_map);
            share::sync_menu_entries(&connected_peers.lock().unwrap());
        }
        // Listed under Nearby in the Continuity window (see control.rs).
        SyncEvent::PeerDiscovered { .. } => {}
        SyncEvent::Disconnected { peer_id, peer_name } => {
            connected_peers.lock().unwrap().remove(&peer_id);
            rebuild_send_menu(send_submenu, &connected_peers.lock().unwrap(), send_target_map);
            share::sync_menu_entries(&connected_peers.lock().unwrap());
            tracing::info!("'{peer_name}' disconnected");
        }
        SyncEvent::ClipboardReceived { from_name } => {
            tracing::info!("clipboard synced from '{from_name}'");
        }
        SyncEvent::ClipboardBroadcast { .. } => {}
        SyncEvent::FileReceiving { from_name, file_name, size_bytes, .. } => {
            notify(&format!("Receiving '{file_name}' ({}) from '{from_name}'", format_size(size_bytes)));
        }
        SyncEvent::FileSending { to_name, file_name, .. } => {
            notify(&format!("Sending '{file_name}' to '{to_name}'..."));
        }
        // The Continuity window shows progress (see control.rs's
        // transfers); a notification per throttled tick would just be
        // noise on top of the start/end ones shown here.
        SyncEvent::FileTransferProgress { transfer_id, bytes_transferred, total_bytes, .. } => {
            tracing::debug!("file transfer {transfer_id}: {bytes_transferred}/{total_bytes} bytes");
        }
        // Who it came from is only in `FileReceiving`, which `control`
        // keeps by transfer id (it's already seen this event too, so the
        // transfer is listed as done by now).
        SyncEvent::FileReceived { transfer_id, file_name, path } => {
            let from = control.transfer(&transfer_id).map(|transfer| transfer.peer_name);
            notify_file_received(&file_name, from.as_deref(), &path);
        }
        SyncEvent::FileSent { file_name, to_name, .. } => {
            notify(&format!("Sent '{file_name}' to '{to_name}'"));
        }
        // A transfer cancelled here was the user's own doing: the window
        // shows it ended, without a notification about it.
        SyncEvent::FileTransferFailed { reason, .. } if reason == continuity_daemon::TRANSFER_CANCELLED => {}
        SyncEvent::FileTransferFailed { reason, .. } => {
            notify(&format!("File transfer failed: {reason}"));
        }
        // Not announced: it usually picks up again by itself within
        // seconds, and the Continuity window shows it waiting. If it
        // can't, `FileTransferFailed` is.
        SyncEvent::FileTransferStalled { transfer_id, reason } => {
            tracing::info!("file transfer {transfer_id} stalled: {reason}");
        }
        SyncEvent::Error(e) => tracing::warn!("{e}"),
        SyncEvent::WasReset => {
            connected_peers.lock().unwrap().clear();
            rebuild_send_menu(send_submenu, &connected_peers.lock().unwrap(), send_target_map);
            share::sync_menu_entries(&connected_peers.lock().unwrap());
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
            share::sync_menu_entries(&connected_peers.lock().unwrap());
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
            // No UI surface for this on desktop yet — logged for now. The
            // interesting direction is mobile displaying *this* device's
            // now-playing info, not the desktop displaying a peer's.
            tracing::debug!("'{peer_name}' now playing: {info:?}");
        }
        // No desktop UI surface for this yet (see task #44 — Android-only
        // so far); fires once per connection per ping interval (30s), too
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
        SyncEvent::RemoteControlSessionStarted { peer_name, role, session_id, .. } => {
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
            let tooltip = format!("Continuity — {text}");
            let _ = tray_icon.set_icon(Some(build_icon(true)));
            let _ = tray_icon.set_tooltip(Some(&tooltip));
            remote_sessions.push((session_id, tooltip));
        }
        // The dot stays while any session is left: a second controlling
        // session ends the first only after it has started (see
        // `handle_remote_control_sync_event`), and this device can be
        // controlled while it controls another.
        SyncEvent::RemoteControlSessionEnded { peer_name, reason, session_id, .. } => {
            let text = match reason {
                Some(reason) => format!("Remote control session with '{peer_name}' ended: {reason}"),
                None => format!("Remote control session with '{peer_name}' ended"),
            };
            notify(&text);
            remote_sessions.retain(|(id, _)| *id != session_id);
            match remote_sessions.last() {
                Some((_, tooltip)) => {
                    let _ = tray_icon.set_tooltip(Some(tooltip));
                }
                None => {
                    let _ = tray_icon.set_icon(Some(build_icon(false)));
                    let _ = tray_icon.set_tooltip(Some("Continuity"));
                }
            }
        }
        // Never arrives here: `start_engine_thread` sends screen frames
        // straight to the viewer's decoder (see `route_screen_frame`)
        // instead of through the event loop.
        SyncEvent::ScreenFrameReceived { .. } => {}
        // Always announced — an unlock is the one thing a peer can do here
        // without anyone at this computer saying yes at that moment, and a
        // refused attempt is either a device that needs allowing first or
        // someone trying something they shouldn't. (Shown after the fact
        // for an unlock, naturally — the notification is waiting on the
        // unlocked desktop.)
        SyncEvent::ScreenLockRequested { peer_name, action, outcome, .. } => {
            use continuity_proto::{ScreenLockAction, ScreenLockOutcome};
            let verb = match action {
                ScreenLockAction::Lock => "lock",
                ScreenLockAction::Unlock => "unlock",
            };
            let text = match outcome {
                ScreenLockOutcome::Done => format!("'{peer_name}' {verb}ed this computer"),
                ScreenLockOutcome::NotAllowed => format!(
                    "'{peer_name}' tried to unlock this computer, but remote unlock isn't turned on for it. To allow it, open Continuity, choose '{peer_name}' and turn on Unlock this computer."
                ),
                ScreenLockOutcome::Unsupported => format!("'{peer_name}' asked to {verb} this computer, which isn't supported here"),
                ScreenLockOutcome::Failed { reason } => format!("Couldn't {verb} this computer for '{peer_name}': {reason}"),
            };
            notify(&text);
        }
        // No desktop UI sends `RequestScreenLock` (only Android does today).
        SyncEvent::ScreenLockResult { peer_name, action, outcome, .. } => {
            tracing::info!("screen {action:?} on '{peer_name}': {outcome:?}");
        }
        // Shown by the Continuity window, which reads the permission from
        // the trust store (see control.rs).
        SyncEvent::UnlockPermissionChanged { .. } => {}
    }
}

/// A size the way people read it — "1.3 GB", "820 KB" — in the same
/// decimal units the Continuity window and the engine's free-space message
/// use.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    match unit {
        0 if bytes == 1 => "1 byte".to_string(),
        0 => format!("{bytes} bytes"),
        _ if value < 10.0 => format!("{value:.1} {}", UNITS[unit]),
        _ => format!("{value:.0} {}", UNITS[unit]),
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

/// Queues one file for a "Send File" entry's device, or for every connected
/// device.
fn send_file_to(
    target: &SendTarget,
    path: &str,
    commands: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    connected_peers: &Arc<Mutex<HashMap<String, String>>>,
) {
    let peers: Vec<String> = match target {
        SendTarget::Peer(peer_id) => vec![peer_id.clone()],
        SendTarget::All => connected_peers.lock().unwrap().keys().cloned().collect(),
    };
    for peer_id in peers {
        let _ = commands.send(EngineCommand::SendFile { peer_crypto_id: peer_id, path: path.to_string() });
    }
}

/// The tray's line about files on their way (see `refresh_tray_transfers`).
#[derive(Default)]
struct TrayTransfers {
    shown: bool,
    text: String,
    last: Option<std::time::Instant>,
}

/// Progress alone updates the tray's transfer line at most this often.
const TRAY_PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Keeps the tray in step with the files on their way: while there are
/// any, a line heads its menu ("Sending 'talk.mov' · 45%", opening the
/// window when clicked) and ends its tooltip, under any remote-control
/// session's line. `changed` is false for a progress update alone, which
/// only refreshes it once a second.
fn refresh_tray_transfers(
    control: &control::ControlState,
    menu: &Menu,
    item: &MenuItem,
    tray_icon: &tray_icon::TrayIcon,
    remote_sessions: &[(String, String)],
    state: &mut TrayTransfers,
    changed: bool,
) {
    if !changed && state.last.is_some_and(|last| last.elapsed() < TRAY_PROGRESS_INTERVAL) {
        return;
    }
    state.last = Some(std::time::Instant::now());
    let summary = control.transfer_summary();
    match (&summary, state.shown) {
        (Some(text), false) => {
            item.set_text(text);
            state.shown = menu.insert(item, 0).is_ok();
        }
        (Some(text), true) if *text != state.text => item.set_text(text),
        (None, true) => {
            let _ = menu.remove(item);
            state.shown = false;
        }
        _ => {}
    }
    state.text = summary.unwrap_or_default();
    let base = remote_sessions.last().map_or("Continuity", |(_, tooltip)| tooltip.as_str());
    let tooltip = if state.text.is_empty() { base.to_string() } else { format!("{base}\n{}", state.text) };
    let _ = tray_icon.set_tooltip(Some(tooltip));
}

/// The subset of `SyncEvent` handling specific to the viewer window —
/// kept separate from `handle_sync_event` (which every build compiles,
/// remote-control or not) rather than threading `#[cfg]` through that
/// function's existing match arms.
#[cfg(feature = "remote-control")]
fn handle_remote_control_sync_event(
    event: &SyncEvent,
    target: &tao::event_loop::EventLoopWindowTarget<AppEvent>,
    viewer: &std::rc::Rc<std::cell::RefCell<Option<remote_viewer::RemoteViewer>>>,
    connected_peer_platforms: &Arc<Mutex<HashMap<String, Platform>>>,
    commands: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    frame_router: &Arc<remote_viewer::FrameRouter>,
    viewer_proxy: &tao::event_loop::EventLoopProxy<AppEvent>,
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
        SyncEvent::RemoteControlSessionStarted { peer_id, peer_name, session_id, role } => {
            if *role != continuity_daemon::RemoteControlRole::Controlling {
                return;
            }
            let platform = connected_peer_platforms.lock().unwrap().get(peer_id).copied().unwrap_or_else(|| {
                tracing::warn!("no known platform for peer '{peer_name}' ({peer_id}) — defaulting to macOS key codes, which will be wrong if it isn't one");
                Platform::MacOs
            });
            let peer = remote_viewer::ViewerPeer { id: peer_id, name: peer_name, session_id, platform };
            let wake_proxy = viewer_proxy.clone();
            let wake: remote_viewer::Wake = Box::new(move || wake_proxy.send_event(AppEvent::ViewerFrameReady).is_ok());
            match remote_viewer::RemoteViewer::open(target, peer, frame_router, wake, commands.clone()) {
                Ok(new_viewer) => {
                    // One viewer at a time, so a session that starts while
                    // another's viewer is open — Control clicked on a second
                    // computer before the first one answered, say — replaces
                    // it, and the earlier session is ended. Just dropping its
                    // viewer used to leave that computer capturing and
                    // streaming its screen to nobody and saying it's being
                    // controlled, with nothing here left to end it. The new
                    // viewer is opened first so it claims any of its frames
                    // that arrived before it (see `FrameRouter`); dropping
                    // the old one releases whatever it held down before the
                    // end goes out.
                    let previous = viewer.borrow_mut().replace(new_viewer);
                    if let Some(previous) = previous.filter(|previous| previous.peer_id() != peer_id) {
                        let previous_peer = previous.peer_id().to_string();
                        drop(previous);
                        tracing::info!("a session with '{peer_name}' replaced the viewer for '{previous_peer}' — ending that one");
                        let _ = commands.send(EngineCommand::EndRemoteControlSession { peer_crypto_id: previous_peer });
                    }
                }
                Err(e) => {
                    tracing::warn!("couldn't open remote control viewer window: {e}");
                    let _ = commands.send(EngineCommand::EndRemoteControlSession { peer_crypto_id: peer_id.clone() });
                }
            }
        }
        SyncEvent::RemoteControlSessionEnded { peer_id, session_id, .. } => {
            // Only this viewer's own session. **Real bug fixed here**: it
            // used to close on the peer alone, so the same peer's request
            // to control *this* device, declined (this device being
            // controlled by someone else already, say), closed the window
            // for the session controlling it — which stayed live, its
            // screen still streaming here to nobody, and a later Control
            // for it was ignored since the engine already had a session.
            // Dropping the viewer releases whatever it held down, closes
            // its window and stops its decoder thread.
            let matches = viewer.borrow().as_ref().is_some_and(|v| v.shows(peer_id, session_id));
            if matches {
                *viewer.borrow_mut() = None;
            }
        }
        // `ScreenFrameReceived` never reaches the event loop — see
        // `route_screen_frame`.
        _ => {}
    }
}

/// Engine thread: where a screen frame goes instead of the event loop. It
/// used to be queued there like every other event and decoded on the UI
/// thread, which froze the whole app once frames came faster than they
/// could be decoded (see remote_viewer.rs's module doc comment). Now the
/// frame router drops it straight into the open viewer's latest-frame
/// slot, replacing any older frame not yet decoded.
#[cfg(feature = "remote-control")]
fn route_screen_frame(frames: &FrameSink, peer_id: &str, session_id: &str, frame: Vec<u8>) {
    frames.offer(peer_id, session_id, frame);
}

/// A build without the viewer can't be the controlling side, so frames
/// shouldn't arrive at all — but if one does, it mustn't pile up anywhere.
#[cfg(not(feature = "remote-control"))]
fn route_screen_frame(_frames: &FrameSink, peer_id: &str, _session_id: &str, frame: Vec<u8>) {
    tracing::trace!("dropped a {}-byte screen frame from '{peer_id}': this build has no remote-control viewer", frame.len());
}

/// Tears the session down if the open viewer window (if any) has gone
/// `RENDER_WATCHDOG` without painting a single real frame — see
/// `RemoteViewer::is_stuck`'s doc comment for why that state is treated
/// as a fatal viewer failure rather than something worth leaving open
/// indefinitely. Called on every event-loop tick (see the `WaitUntil`
/// polling next to `check_viewer_watchdog`'s call sites) so a session that
/// never receives *any* frame at all still gets caught, not just one that
/// receives some but fails to render them.
#[cfg(feature = "remote-control")]
fn check_viewer_watchdog(
    viewer: &std::rc::Rc<std::cell::RefCell<Option<remote_viewer::RemoteViewer>>>,
    commands: &tokio::sync::mpsc::UnboundedSender<EngineCommand>,
) {
    let stuck_peer_id = viewer.borrow().as_ref().filter(|v| v.is_stuck()).map(|v| v.peer_id().to_string());
    if let Some(peer_id) = stuck_peer_id {
        tracing::warn!("remote-control viewer for '{peer_id}' never rendered a frame — giving up and ending the session");
        *viewer.borrow_mut() = None;
        let _ = commands.send(EngineCommand::EndRemoteControlSession { peer_crypto_id: peer_id });
        notify("Couldn't display the remote screen — ending the session");
    }
}

/// Opens the Continuity window on macOS: the SwiftUI app bundled inside
/// Continuity.app (`Contents/Helpers/Continuity.app`, built from
/// apps/macos/ContinuitySettings), which talks to this process over the
/// local control socket. `CONTINUITY_SETTINGS_APP` points a development
/// build (no bundle around it) at a locally built copy.
#[cfg(target_os = "macos")]
fn open_settings(profile: &str) {
    let bundled = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.join("Helpers/Continuity.app")));
    let app = std::env::var_os("CONTINUITY_SETTINGS_APP").map(PathBuf::from).or(bundled).filter(|app| app.exists());
    let Some(app) = app else {
        notify("The Continuity window isn't included in this build of Continuity.");
        return;
    };
    let mut open = std::process::Command::new("/usr/bin/open");
    open.arg("-a").arg(&app);
    if profile != "default" {
        open.args(["--args", "--profile", profile]);
    }
    std::thread::spawn(move || match open.status() {
        Ok(status) if status.success() => {}
        // LaunchServices refuses an app whose minimum macOS is newer.
        Ok(_) => notify("Couldn't open the Continuity window — it needs macOS 12 or newer."),
        Err(e) => tracing::warn!("couldn't run open for the Continuity window: {e}"),
    });
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn settings_window_icon() -> Option<tao::window::Icon> {
    tao::window::Icon::from_rgba(render_icon_rgba(64), 64, 64).ok()
}

/// What to tell someone whose Continuity window didn't open.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn settings_window_failed(error: &anyhow::Error) -> String {
    if cfg!(target_os = "windows") {
        format!(
            "Couldn't open the Continuity window ({error}). It needs the Microsoft Edge WebView2 Runtime, which comes with Windows 11 — on Windows 10, install it from https://go.microsoft.com/fwlink/p/?LinkId=2124703"
        )
    } else {
        format!("Couldn't open the Continuity window ({error}). It needs WebKitGTK (libwebkit2gtk-4.1).")
    }
}

/// Gets the system's notifications ready, before the first one goes out.
///
/// macOS: asks to notify — macOS's own prompt, the first time (see
/// notifications_mac.rs for why that's needed).
///
/// Windows: a toast is filed under an AppUserModelID, and Windows drops one
/// silently unless that id belongs to something it knows — notify-rust
/// falls back to PowerShell's, which is why they used to say "Windows
/// PowerShell". Continuity's is registered for this user here (a registry
/// key with its name and icon, which is all Windows needs for an app that
/// isn't packaged), every notification is filed under it (see
/// `notification`), and the installer's Start-menu shortcut carries the
/// same id.
fn init_notifications() {
    #[cfg(target_os = "macos")]
    notifications_mac::request_permission();
    #[cfg(target_os = "windows")]
    register_app_user_model_id();
}

/// See `init_notifications`. Registered on every start rather than only at
/// install — it's two registry values and a small icon file, and this way a
/// copy that was never installed (or whose key was cleaned away) still gets
/// its notifications shown.
#[cfg(target_os = "windows")]
fn register_app_user_model_id() {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

    // This process's windows (the Continuity window, a remote-control
    // viewer) then group with the Start-menu shortcut on the taskbar too.
    if let Err(e) = unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID)) } {
        tracing::debug!("couldn't set this process's AppUserModelID: {e}");
    }
    // The icon toasts show, written next to the trust store (and so removed
    // with it at uninstall): the tray icon's mark — the executable has no
    // icon resource to point at instead (see installers/windows/continuity.iss).
    let icon = directories::ProjectDirs::from("app", "continuity", "continuity")
        .map(|dirs| dirs.config_dir().join("notification-icon.png"))
        .filter(|path| {
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(path, encode_png(&render_icon_rgba(128), 128, 128)));
            if let Err(e) = &written {
                tracing::debug!("couldn't write {}: {e}", path.display());
            }
            written.is_ok()
        });
    let key = HSTRING::from(format!(r"Software\Classes\AppUserModelId\{APP_ID}"));
    let mut values = vec![("DisplayName", "Continuity".to_string())];
    if let Some(icon) = icon {
        values.push(("IconUri", icon.display().to_string()));
    }
    for (name, value) in values {
        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        // Creates the key as well, the first time.
        let result = unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, &key, &HSTRING::from(name), REG_SZ.0, Some(wide.as_ptr().cast()), (wide.len() * 2) as u32)
        };
        if let Err(e) = result.ok() {
            tracing::warn!("couldn't register Continuity for notifications ({name}): {e}");
        }
    }
}

/// A new notification, filed under Continuity's own AppUserModelID on
/// Windows (see `init_notifications`). macOS's are in notifications_mac.rs.
#[cfg(not(target_os = "macos"))]
fn notification() -> notify_rust::Notification {
    #[allow(unused_mut)]
    let mut notification = notify_rust::Notification::new();
    #[cfg(target_os = "windows")]
    notification.app_id(APP_ID);
    notification
}

/// Shows a native notification when possible. Unbundled dev binaries often
/// can't (macOS requires a proper .app bundle with an Info.plist for
/// reliable notification delivery), so this always logs too — the tray
/// menu/dialogs are the guaranteed-visible UI; notifications are a bonus.
/// Never waits: this is often called from the event loop.
fn notify(body: &str) {
    tracing::info!("{body}");
    #[cfg(target_os = "macos")]
    notifications_mac::show("Continuity", body);
    #[cfg(not(target_os = "macos"))]
    if let Err(e) = notification().summary("Continuity").body(body).show() {
        tracing::debug!("notification not shown (expected for an unbundled binary): {e}");
    }
}

/// A file-received notification is worth acting on immediately, so it gets
/// "Open" / "Show in Folder" actions instead of just announcing itself.
/// `wait_for_action` blocks until the user interacts (or the notification
/// times out/closes), so it runs on its own thread rather than the tao
/// event loop's thread — blocking that would freeze the whole tray app
/// until the notification was dismissed. (macOS's does the same in
/// notifications_mac.rs.)
fn notify_file_received(file_name: &str, from: Option<&str>, path: &str) {
    tracing::info!("received '{file_name}' at {path}");
    let body = match from {
        Some(from) => format!("Received '{file_name}' from '{from}'"),
        None => format!("Received '{file_name}'"),
    };
    let path = path.to_string();
    let on_action = move |action: &str| match action {
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
    };
    #[cfg(target_os = "macos")]
    notifications_mac::show_with_actions("Continuity", &body, &[("default", "Open"), ("show-folder", "Show in Folder")], on_action);
    #[cfg(not(target_os = "macos"))]
    match notification().summary("Continuity").body(&body).action("default", "Open").action("show-folder", "Show in Folder").show() {
        Ok(handle) => {
            std::thread::spawn(move || handle.wait_for_action(on_action));
        }
        Err(e) => tracing::debug!("notification not shown (expected for an unbundled binary): {e}"),
    }
}

fn open_path(path: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
    }
    // **Real bug fixed here**: this used to run `cmd /C start "" <path>`.
    // cmd re-parses its whole command line, and Rust only quotes an
    // argument that contains a space — so a received file a peer had named
    // `notes&calc` (no space, nothing the name check refuses) ran `calc`
    // the moment someone clicked Open. ShellExecuteW opens the file with
    // its default app directly, with no shell parsing in between.
    #[cfg(target_os = "windows")]
    {
        use windows::core::{HSTRING, PCWSTR};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        // Values above 32 mean success (the documented, if odd, contract).
        let result = unsafe { ShellExecuteW(HWND::default(), &HSTRING::from("open"), &HSTRING::from(path), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
        if result.0 as isize <= 32 {
            return Err(std::io::Error::other(format!("ShellExecuteW failed ({})", result.0 as isize)));
        }
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
    proxy: tao::event_loop::EventLoopProxy<AppEvent>,
    frames: FrameSink,
    received_files_dir: PathBuf,
) -> anyhow::Result<(tokio::sync::mpsc::UnboundedSender<EngineCommand>, String)> {
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
                let device_id = identity.device_id();
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
                    Arc::new(remote_control_linux::LinuxRemoteControlHost::new(remote_control_linux::restore_token_path(&profile)));
                #[cfg(not(all(feature = "remote-control", any(target_os = "macos", target_os = "windows", target_os = "linux"))))]
                let remote_control: Arc<dyn continuity_daemon::RemoteControlHost> = Arc::new(continuity_daemon::NoopRemoteControlHost);

                // Remote lock everywhere; remote unlock on Linux only —
                // macOS and Windows have no supported API for an ordinary
                // app to dismiss their lock screens, so they answer an
                // unlock `Unsupported` (see `ScreenLockController::can_unlock`).
                #[cfg(target_os = "linux")]
                let screen_lock: Arc<dyn continuity_daemon::ScreenLockController> = Arc::new(screen_lock_linux::LinuxScreenLock);
                #[cfg(target_os = "macos")]
                let screen_lock: Arc<dyn continuity_daemon::ScreenLockController> = Arc::new(screen_lock_mac::MacScreenLock);
                #[cfg(target_os = "windows")]
                let screen_lock: Arc<dyn continuity_daemon::ScreenLockController> = Arc::new(screen_lock_windows::WindowsScreenLock);
                #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
                let screen_lock: Arc<dyn continuity_daemon::ScreenLockController> = Arc::new(continuity_daemon::NoopScreenLockController);

                let config = EngineConfig {
                    identity,
                    device_name,
                    trust_store,
                    clipboard: Arc::new(ArboardClipboard),
                    media,
                    remote_control,
                    screen_lock,
                    received_files_dir,
                };
                continuity_daemon::start(config).await.map(|engine| (engine, device_id))
            };

            match setup.await {
                Ok((mut engine, device_id)) => {
                    let _ = ready_tx.send(Ok((engine.command_sender(), device_id)));
                    while let Some(event) = engine.events.recv().await {
                        // Screen frames (~10 a second during a session)
                        // bypass the event loop entirely — nothing there
                        // needs them but the viewer, and queued there they
                        // froze the app (see `route_screen_frame`).
                        let event = match event {
                            SyncEvent::ScreenFrameReceived { peer_id, session_id, frame } => {
                                route_screen_frame(&frames, &peer_id, &session_id, frame);
                                continue;
                            }
                            event => event,
                        };
                        if proxy.send_event(AppEvent::Engine(event)).is_err() {
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
    // Size-limited: see log_file.rs.
    let file_layer = log_file_path(&profile())
        .and_then(|path| log_file::LogFile::open(&path).ok())
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

/// Where received files go this run: the folder chosen in the Continuity
/// window, if it can still be used — or, if it can't (a USB drive that
/// isn't plugged in, a network share that's gone), the default, saying so
/// in the log. The choice itself stays saved, so it applies again once the
/// folder is back.
fn received_files_dir_for_this_run(profile: &str, settings: &control::AppSettings) -> PathBuf {
    let default = received_files_dir(profile);
    let Some(chosen) = &settings.received_files_dir else {
        return default;
    };
    match control::check_usable_dir(chosen) {
        Ok(()) => chosen.clone(),
        Err(e) => {
            tracing::warn!(
                "received files can't go in {} ({e}) — saving them in {} instead until it's back",
                chosen.display(),
                default.display()
            );
            default
        }
    }
}

/// A PNG of an RGBA image — the notification icon Windows shows for
/// Continuity (see `register_app_user_model_id`). Uncompressed (deflate's
/// "stored" blocks): a small icon written once a run doesn't need an image
/// crate's encoder, and every PNG reader accepts it.
#[cfg(any(target_os = "windows", test))]
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = png.len();
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let crc = crc32(&png[start..]);
        png.extend_from_slice(&crc.to_be_bytes());
    }

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // 8 bits per channel, RGBA, deflate, no filter method, no interlacing.
    header.extend_from_slice(&[8, 6, 0, 0, 0]);

    // Each row starts with its filter type: 0, none.
    let row = width as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgba.chunks(row).take(height as usize) {
        raw.push(0);
        raw.extend_from_slice(line);
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(u16::MAX as usize).collect();
    for (i, block) in blocks.iter().enumerate() {
        zlib.push(u8::from(i == blocks.len() - 1));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

#[cfg(any(target_os = "windows", test))]
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(any(target_os = "windows", test))]
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_the_way_people_say_them() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1), "1 byte");
        assert_eq!(format_size(999), "999 bytes");
        assert_eq!(format_size(820_000), "820 KB");
        assert_eq!(format_size(1_300_000_000), "1.3 GB");
        assert_eq!(format_size(5_000_000), "5.0 MB");
    }

    #[test]
    fn a_chosen_received_files_folder_is_used_only_while_it_works() {
        let dir = tempfile::tempdir().unwrap();
        let default = received_files_dir("default");
        assert_eq!(received_files_dir_for_this_run("default", &control::AppSettings::default()), default, "nothing chosen");

        let chosen = dir.path().join("Incoming");
        let settings = control::AppSettings { received_files_dir: Some(chosen.clone()), ..Default::default() };
        assert_eq!(received_files_dir_for_this_run("default", &settings), chosen);
        assert!(chosen.is_dir(), "made if it was missing");

        // Somewhere no folder can be made: under a file.
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let gone = control::AppSettings { received_files_dir: Some(file.join("Incoming")), ..Default::default() };
        assert_eq!(received_files_dir_for_this_run("default", &gone), default, "falls back to the default");
    }

    #[test]
    fn checksums_match_their_reference_values() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b"IEND"), 0xae42_6082, "the CRC every PNG ends with");
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    /// Walks the PNG's chunks, checks each CRC, and undoes the stored
    /// deflate blocks by hand — there's no PNG decoder in this build to
    /// hand it to.
    #[test]
    fn the_notification_icon_is_a_well_formed_png() {
        for size in [2u32, 128] {
            let rgba: Vec<u8> = (0..size * size * 4).map(|i| (i % 251) as u8).collect();
            let png = encode_png(&rgba, size, size);
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            let mut chunks = Vec::new();
            let mut at = 8;
            while at < png.len() {
                let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
                let kind = &png[at + 4..at + 8];
                let data = &png[at + 8..at + 8 + len];
                let crc = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
                assert_eq!(crc32(&png[at + 4..at + 8 + len]), crc, "{}", String::from_utf8_lossy(kind));
                chunks.push((kind.to_vec(), data.to_vec()));
                at += 12 + len;
            }
            let kinds: Vec<&[u8]> = chunks.iter().map(|(kind, _)| kind.as_slice()).collect();
            assert_eq!(kinds, [b"IHDR".as_slice(), b"IDAT", b"IEND"]);
            let header = &chunks[0].1;
            assert_eq!(&header[..8], [size.to_be_bytes(), size.to_be_bytes()].concat());
            assert_eq!(&header[8..], [8, 6, 0, 0, 0]);

            let zlib = &chunks[1].1;
            assert_eq!(&zlib[..2], [0x78, 0x01]);
            let mut raw = Vec::new();
            let mut at = 2;
            loop {
                let last = zlib[at] & 1 == 1;
                assert_eq!(zlib[at] >> 1, 0, "stored blocks");
                let len = u16::from_le_bytes([zlib[at + 1], zlib[at + 2]]);
                assert_eq!(!len, u16::from_le_bytes([zlib[at + 3], zlib[at + 4]]));
                raw.extend_from_slice(&zlib[at + 5..at + 5 + len as usize]);
                at += 5 + len as usize;
                if last {
                    break;
                }
            }
            assert_eq!(u32::from_be_bytes(zlib[at..at + 4].try_into().unwrap()), adler32(&raw));
            assert_eq!(at + 4, zlib.len());
            let rows: Vec<&[u8]> = raw.chunks(size as usize * 4 + 1).collect();
            assert_eq!(rows.len(), size as usize);
            for (y, row) in rows.iter().enumerate() {
                assert_eq!(row[0], 0, "no filter");
                let start = y * size as usize * 4;
                assert_eq!(&row[1..], &rgba[start..start + size as usize * 4]);
            }
        }
    }
}

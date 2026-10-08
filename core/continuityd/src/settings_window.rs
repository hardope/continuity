//! The settings window on Linux and Windows: the same pages as the macOS app
//! (apps/macos/ContinuitySettings), written once as an HTML page
//! (`settings_window/page.html`) and shown by the system's own webview —
//! WebKitGTK on Linux, WebView2 on Windows — in a window of this process.
//!
//! The page talks to `control.rs` through the webview rather than the local
//! socket the macOS app uses. It posts requests — JSON with an `id` and an
//! `op`, most of them `ControlOp`s — which reach the event loop as
//! `Message::Request`; the answers, and a fresh status whenever anything
//! changes (from a `ControlState::watch` thread), go back into the page as
//! calls to `window.continuity.receive`.
//!
//! A webview takes a few seconds to start, so the window is made hidden a
//! few seconds after Continuity starts, and closing it only hides it again:
//! Settings… then just shows it.

use crate::control::{ControlOp, ControlState};
use crate::share::ConnectedPeers;
use continuity_daemon::EngineCommand;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tao::dpi::LogicalSize;
use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Icon, Theme, Window, WindowBuilder, WindowId};
use tokio::sync::mpsc::UnboundedSender;
use wry::{NewWindowResponse, WebContext, WebView, WebViewBuilder};

const PAGE: &str = include_str!("settings_window/page.html");

/// The only place the page's links go — in the browser, never in this
/// window.
const PROJECT_URL: &str = "https://github.com/hardope/continuity";

pub enum Message {
    /// A request the page posted.
    Request(String),
    /// A status line for the page, from its watch thread.
    Status(String),
    /// What Send Files… chose, once its dialog closes (see
    /// file_chooser_linux.rs for why that's later, not in the request).
    #[cfg(target_os = "linux")]
    FilesChosen { device: String, paths: Vec<PathBuf> },
}

/// What the event loop needs to route the page's messages back to it.
pub struct Route<T: 'static> {
    pub proxy: EventLoopProxy<T>,
    pub wrap: fn(Message) -> T,
}

impl<T: 'static> Clone for Route<T> {
    fn clone(&self) -> Self {
        Self { proxy: self.proxy.clone(), wrap: self.wrap }
    }
}

/// What a request can change.
pub struct Context<'a> {
    pub control: &'a Arc<ControlState>,
    pub commands: &'a UnboundedSender<EngineCommand>,
    pub connected: &'a ConnectedPeers,
}

pub struct SettingsWindow {
    // Fields drop in order: the webview before the window it's in, and the
    // webview's context last.
    webview: WebView,
    window: Window,
    _context: WebContext,
    /// Bumped when the window closes or the page loads again, which stops
    /// the watch thread feeding the page that was there before.
    watch_generation: Arc<AtomicU64>,
}

impl SettingsWindow {
    /// `visible: false` prepares the window without showing it.
    pub fn open<T: Send + 'static>(target: &EventLoopWindowTarget<T>, route: Route<T>, icon: Option<Icon>, visible: bool) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_title("Continuity")
            .with_inner_size(LogicalSize::new(880.0, 620.0))
            .with_min_inner_size(LogicalSize::new(680.0, 460.0))
            .with_window_icon(icon)
            .with_visible(visible)
            .build(target)?;
        // The page paints its own background; this is the color before it
        // does, so a dark desktop doesn't get a white flash.
        let background = if window.theme() == Theme::Dark { (32, 32, 32, 255) } else { (243, 243, 243, 255) };
        // WebView2's default place for its data is beside the executable,
        // which in Program Files can't be written to.
        let mut context = WebContext::new(webview_data_dir());
        let builder = WebViewBuilder::new_with_web_context(&mut context)
            .with_html(PAGE.replace("__PLATFORM__", platform_name()))
            .with_background_color(background)
            .with_devtools(cfg!(debug_assertions))
            .with_hotkeys_zoom(false)
            .with_navigation_handler(|url| !(url.starts_with("http:") || url.starts_with("https:")))
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
            .with_ipc_handler(move |request| {
                let _ = route.proxy.send_event((route.wrap)(Message::Request(request.into_body())));
            });
        #[cfg(target_os = "linux")]
        let webview = {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            let container = window.default_vbox().ok_or_else(|| anyhow::anyhow!("the window has no container for the webview"))?;
            builder.build_gtk(container)?
        };
        #[cfg(not(target_os = "linux"))]
        let webview = builder.build(&window)?;
        Ok(Self { webview, window, _context: context, watch_generation: Arc::new(AtomicU64::new(0)) })
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// Shows the window, or brings it back to the front.
    pub fn show(&self) {
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.set_focus();
    }

    /// What closing the window does: it stays ready for next time. The page
    /// drops any dialog it had open, so it doesn't come back with it.
    pub fn hide(&self) {
        self.window.set_visible(false);
        if let Err(e) = self.webview.evaluate_script("window.continuity && window.continuity.hidden && window.continuity.hidden()") {
            tracing::debug!("couldn't tell the settings page it was closed: {e}");
        }
    }

    pub fn handle<T: Send + 'static>(&mut self, message: Message, context: &Context, route: &Route<T>) {
        match message {
            Message::Status(line) => self.deliver(&line),
            // No `id`: the page shows it as a notice of its own.
            #[cfg(target_os = "linux")]
            Message::FilesChosen { device, paths } => {
                let sent = crate::share::dispatch(&device, &paths, context.commands, context.connected);
                self.deliver(&json!({ "ok": sent.ok, "message": sent.message }).to_string());
            }
            Message::Request(body) => {
                let request: Value = match serde_json::from_str(&body) {
                    Ok(request) => request,
                    Err(e) => {
                        tracing::warn!("the settings page sent something that isn't JSON: {e}");
                        return;
                    }
                };
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let answer = self.answer(request, context, route);
                let reply = match answer {
                    Ok(message) => json!({ "id": id, "ok": true, "message": message }),
                    Err(message) => json!({ "id": id, "ok": false, "message": message }),
                };
                self.deliver(&reply.to_string());
            }
        }
    }

    /// Carries out one request. `Ok` may carry a line for the page to show.
    fn answer<T: Send + 'static>(&mut self, request: Value, context: &Context, route: &Route<T>) -> Result<Option<String>, String> {
        let text = |key: &str| request.get(key).and_then(Value::as_str).map(str::to_string);
        match request.get("op").and_then(Value::as_str) {
            Some("watch") => {
                self.watch(context.control, route.clone());
                Ok(None)
            }
            // The page can't see file paths, so the file dialog is ours.
            Some("send_files") => {
                let device = text("device").ok_or("Which device?")?;
                #[cfg(target_os = "linux")]
                {
                    use gtk::prelude::*;
                    use tao::platform::unix::WindowExtUnix;
                    let route = route.clone();
                    let parent: &gtk::Window = self.window.gtk_window().upcast_ref();
                    crate::file_chooser_linux::choose_files(Some(parent), "Send Files", true, move |paths| {
                        let _ = route.proxy.send_event((route.wrap)(Message::FilesChosen { device: device.clone(), paths }));
                    });
                    Ok(None)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let Some(paths) = rfd::FileDialog::new().set_title("Send Files").set_parent(&self.window).pick_files() else {
                        return Ok(None);
                    };
                    let sent = crate::share::dispatch(&device, &paths, context.commands, context.connected);
                    if sent.ok {
                        Ok(Some(sent.message))
                    } else {
                        Err(sent.message)
                    }
                }
            }
            Some("show_received_files") => {
                let dir = context.control.status().received_files_dir;
                // Created on the first file received; make sure there's
                // something to show before that.
                let _ = std::fs::create_dir_all(&dir);
                crate::open_path(&dir).map(|_| None).map_err(|e| format!("Couldn't open {dir}: {e}"))
            }
            Some("show_log") => {
                let log = context.control.status().about.log_file.ok_or("There's no log file.")?;
                crate::reveal_in_folder(&log).map(|_| None).map_err(|e| format!("Couldn't show {log}: {e}"))
            }
            Some("open_link") => {
                let page = match text("page").as_deref() {
                    Some("issues") => format!("{PROJECT_URL}/issues/new"),
                    Some("license") => format!("{PROJECT_URL}/blob/master/LICENSE"),
                    _ => PROJECT_URL.to_string(),
                };
                crate::open_path(&page).map(|_| None).map_err(|e| format!("Couldn't open the browser: {e}"))
            }
            _ => match serde_json::from_value::<ControlOp>(request) {
                // Only for the socket: the page sends neither paths nor a
                // second watch through here.
                Ok(ControlOp::Status | ControlOp::Watch | ControlOp::SendFiles { .. }) => Err("Not available here.".to_string()),
                Ok(op) => context.control.apply(op, context.commands).map(|_| None),
                Err(e) => Err(format!("Unknown request: {e}")),
            },
        }
    }

    /// Feeds the page a status now and on every change, for as long as this
    /// page is the one in the window.
    fn watch<T: Send + 'static>(&self, control: &Arc<ControlState>, route: Route<T>) {
        let generation = self.watch_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let current = Arc::clone(&self.watch_generation);
        let control = Arc::clone(control);
        std::thread::spawn(move || {
            control.watch(|status| {
                if current.load(Ordering::SeqCst) != generation {
                    return Err(std::io::Error::other("the page went away"));
                }
                let line = json!({ "ok": true, "status": status }).to_string();
                route.proxy.send_event((route.wrap)(Message::Status(line))).map_err(|_| std::io::Error::other("Continuity is quitting"))
            });
        });
    }

    fn deliver(&self, line: &str) {
        // JSON is a JavaScript expression, except that these two may appear
        // raw in a JSON string but not in an older engine's string literal.
        let line = line.replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029");
        if let Err(e) = self.webview.evaluate_script(&format!("window.continuity && window.continuity.receive({line})")) {
            tracing::warn!("couldn't update the settings window: {e}");
        }
    }
}

impl Drop for SettingsWindow {
    fn drop(&mut self) {
        self.watch_generation.fetch_add(1, Ordering::SeqCst);
    }
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

/// Next to the log and settings, in the user's local data folder.
fn webview_data_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("app", "continuity", "continuity")?;
    Some(dirs.data_local_dir().join("webview"))
}


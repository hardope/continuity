//! "Send with Continuity" from the desktop's own file manager — the
//! desktop half of sharing a file to a device (Android's half is its system
//! share sheet).
//!
//! None of the desktops give an unpackaged tray app a real "Share" menu
//! slot: macOS needs a signed Share *extension* bundle, Windows 11's Share
//! button needs a packaged (MSIX) app, and Linux has no common share menu at
//! all. So each platform gets its closest native equivalent instead:
//!
//! - **Linux / Windows**: one right-click entry *per connected device* (plus
//!   "All connected devices" once there's more than one), kept in sync by
//!   the running app — they appear as devices connect and disappear when
//!   they drop or when Continuity quits, so the menu itself is the device
//!   picker and only ever offers something that can actually work right
//!   now. Linux: Files/Nautilus (Scripts submenu), Dolphin (service menu),
//!   and Nemo (actions). Windows: Explorer's "Send to" menu.
//! - **macOS**: Finder's "Open With > Continuity" (declared statically in
//!   the app's Info.plist — macOS has no per-device equivalent), which
//!   reaches the running app as a tao `Event::Opened` and then asks which
//!   device with a native list dialog.
//!
//! Each Linux/Windows entry just runs `continuityd --share-to <device id>
//! <files...>`, which hands the files to the already-running instance over a
//! loopback TCP socket guarded by a random token in a user-private file (see
//! `start_server`) and exits — it never starts a second tray app. The same
//! socket also serves the Continuity window's requests (see `control.rs`),
//! and, on Linux and Windows, a plain second launch of Continuity: it asks
//! the running one to show its window and exits (see
//! `hand_off_to_running_instance`).

use crate::control::{ControlOp, ControlState};
use continuity_daemon::EngineCommand;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

pub const SHARE_TO_FLAG: &str = "--share-to";
/// What the server answers a request without the right token.
const WRONG_TOKEN: &str = "rejected: wrong token";
/// `--share-to` target meaning every currently-connected device.
const ALL_DEVICES: &str = "all";

pub(crate) type ConnectedPeers = Arc<Mutex<HashMap<String, String>>>;

/// Written by the running instance, read by `--share-to`.
#[derive(Serialize, Deserialize)]
struct Endpoint {
    port: u16,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct ShareRequest {
    token: String,
    target: String,
    paths: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ShareResponse {
    pub(crate) ok: bool,
    pub(crate) message: String,
}

/// One right-click entry: what it's labeled, and what `--share-to` gets.
struct MenuTarget {
    label: String,
    target: String,
}

/// Connected devices sorted by name, plus "All connected devices" when
/// there's more than one. Two devices with the same name get a short id
/// suffix — a file name (and a menu entry) has to be unique.
fn menu_targets(connected: &HashMap<String, String>) -> Vec<MenuTarget> {
    let mut peers: Vec<(&String, &String)> = connected.iter().collect();
    peers.sort_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(b.0)));
    let mut targets: Vec<MenuTarget> = peers
        .iter()
        .map(|(id, name)| {
            let duplicate = peers.iter().filter(|(_, other)| other == name).count() > 1;
            let label = if duplicate { format!("{name} ({})", &id[..id.len().min(6)]) } else { (*name).clone() };
            MenuTarget { label, target: (*id).clone() }
        })
        .collect();
    if peers.len() > 1 {
        targets.push(MenuTarget { label: "All connected devices".to_string(), target: ALL_DEVICES.to_string() });
    }
    targets
}

/// Queues every file for every device `target` names. Folders are skipped —
/// the engine only sends regular files. Also what the Linux and Windows
/// Continuity window's Send Files… does (see settings_window.rs).
pub(crate) fn dispatch(target: &str, paths: &[PathBuf], commands: &UnboundedSender<EngineCommand>, connected: &ConnectedPeers) -> ShareResponse {
    let peers: Vec<String> = {
        let connected = connected.lock().unwrap();
        if target == ALL_DEVICES {
            connected.keys().cloned().collect()
        } else {
            connected.contains_key(target).then(|| target.to_string()).into_iter().collect()
        }
    };
    if peers.is_empty() {
        let message = if target == ALL_DEVICES { "No devices are connected right now." } else { "That device isn't connected right now." };
        return ShareResponse { ok: false, message: message.to_string() };
    }
    let files: Vec<&PathBuf> = paths.iter().filter(|p| p.is_file()).collect();
    if files.is_empty() {
        return ShareResponse { ok: false, message: "Only files can be sent — folders aren't supported yet.".to_string() };
    }
    for file in &files {
        for peer in &peers {
            let _ = commands.send(EngineCommand::SendFile { peer_crypto_id: peer.clone(), path: file.display().to_string() });
        }
    }
    let skipped = paths.len() - files.len();
    let message = if skipped > 0 { format!("Sending {} file(s); skipped {skipped} folder(s)", files.len()) } else { format!("Sending {} file(s)", files.len()) };
    ShareResponse { ok: true, message }
}

fn endpoint_path(profile: &str) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("app", "continuity", "continuity")?;
    Some(dirs.config_dir().join(format!("share-endpoint.{profile}.json")))
}

/// The running instance's side of `--share-to`. Removes its endpoint file
/// on `shutdown` — tao's event loop exits the process directly, so this
/// can't rely on `Drop`.
pub struct ShareServer {
    endpoint: PathBuf,
}

impl ShareServer {
    pub fn shutdown(&self) {
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

/// What the server calls when a second launch asks for the window — it
/// wakes the event loop, which is the only thing that can show it.
pub type OpenWindow = Arc<dyn Fn() + Send + Sync>;

/// Listens on a random loopback port and publishes `{port, token}` in a
/// file only this user can read. Every request has to carry that token, so
/// another local account can't push files out through this device (or
/// change its settings) — the same boundary the trust store and identity
/// files already rely on. Each connection gets its own thread, since a
/// window's `watch` keeps one open.
pub fn start_server(
    profile: &str,
    commands: UnboundedSender<EngineCommand>,
    connected: ConnectedPeers,
    control: Arc<ControlState>,
    open_window: OpenWindow,
) -> Option<ShareServer> {
    let endpoint = endpoint_path(profile)?;
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("couldn't start the share listener, 'Send with Continuity' won't work: {e}");
            return None;
        }
    };
    let port = listener.local_addr().ok()?.port();
    let token = format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>());
    if let Err(e) = write_private_file(&endpoint, &serde_json::to_vec(&Endpoint { port, token: token.clone() }).ok()?) {
        tracing::warn!("couldn't write {}: {e}", endpoint.display());
        return None;
    }

    let token = Arc::new(token);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (token, commands, connected, control, open_window) =
                (token.clone(), commands.clone(), connected.clone(), control.clone(), open_window.clone());
            std::thread::spawn(move || {
                if let Err(e) = serve_one(stream, &token, &commands, &connected, &control, &open_window) {
                    tracing::debug!("local request failed: {e}");
                }
            });
        }
    });
    Some(ShareServer { endpoint })
}

/// One request: `--share-to`'s (no `op` — a target and files), or a
/// window's or a second launch's (tagged by `op`, see `ControlOp`).
fn serve_one(
    stream: TcpStream,
    token: &str,
    commands: &UnboundedSender<EngineCommand>,
    connected: &ConnectedPeers,
    control: &ControlState,
    open_window: &OpenWindow,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let _ = stream.set_nodelay(true);
    let mut line = String::new();
    BufReader::new((&stream).take(1024 * 1024)).read_line(&mut line)?;
    let mut request: serde_json::Value = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(e) => return send_line(&stream, &ShareResponse { ok: false, message: format!("malformed request: {e}") }),
    };
    if request.get("token").and_then(|t| t.as_str()) != Some(token) {
        return send_line(&stream, &ShareResponse { ok: false, message: WRONG_TOKEN.to_string() });
    }
    if request.get("op").is_none() {
        let response = match serde_json::from_value::<ShareRequest>(request) {
            Ok(share) => dispatch(&share.target, &share.paths, commands, connected),
            Err(e) => ShareResponse { ok: false, message: format!("malformed request: {e}") },
        };
        return send_line(&stream, &response);
    }
    if let Some(fields) = request.as_object_mut() {
        fields.remove("token");
    }
    match serde_json::from_value::<ControlOp>(request) {
        Ok(ControlOp::Status) => send_line(&stream, &serde_json::json!({ "ok": true, "status": control.status() })),
        Ok(ControlOp::Watch) => {
            control.watch(|status| send_line(&stream, &serde_json::json!({ "ok": true, "status": status })));
            Ok(())
        }
        Ok(ControlOp::SendFiles { device, paths }) => send_line(&stream, &dispatch(&device, &paths, commands, connected)),
        Ok(ControlOp::OpenWindow) => {
            open_window();
            send_line(&stream, &ShareResponse { ok: true, message: String::new() })
        }
        Ok(op) => {
            let response = match control.apply(op, commands) {
                Ok(()) => ShareResponse { ok: true, message: String::new() },
                Err(message) => ShareResponse { ok: false, message },
            };
            send_line(&stream, &response)
        }
        Err(e) => send_line(&stream, &ShareResponse { ok: false, message: format!("unknown request: {e}") }),
    }
}

fn send_line(stream: &TcpStream, value: &impl Serialize) -> std::io::Result<()> {
    let mut out = serde_json::to_vec(value)?;
    out.push(b'\n');
    let mut writer = stream;
    writer.write_all(&out)
}

/// Writes `contents` to a freshly created file only this user can read —
/// also used for the Linux remote-desktop restore token.
pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `mode` below only applies to a newly created file — start from none,
    // so the token can never land in an existing file with looser
    // permissions.
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents)
}

/// `continuityd --share-to <device id | all> <files...>` — what each
/// right-click entry runs. Returns the process exit code. Problems are
/// shown as a notification, since a file manager launches this with no
/// terminal attached.
pub fn run_share_client(profile: &str, args: &[String]) -> i32 {
    let Some((target, paths)) = args.split_first() else {
        eprintln!("usage: continuityd {SHARE_TO_FLAG} <device id | {ALL_DEVICES}> <file>...");
        return 2;
    };
    let paths: Vec<PathBuf> = paths.iter().map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p))).collect();
    if paths.is_empty() {
        crate::notify("Select at least one file to send.");
        return 2;
    }
    match request_share(profile, target, paths) {
        Ok(response) if response.ok => 0,
        Ok(response) => {
            crate::notify(&response.message);
            1
        }
        Err(e) => {
            eprintln!("couldn't reach the running Continuity: {e}");
            crate::notify("Continuity isn't running — start it, then try again.");
            1
        }
    }
}

fn request_share(profile: &str, target: &str, paths: Vec<PathBuf>) -> std::io::Result<ShareResponse> {
    request_running_instance(profile, Duration::from_secs(10), |token| {
        Ok(serde_json::to_value(ShareRequest { token, target: target.to_string(), paths })?)
    })
}

/// One request to the running instance, if there is one: the endpoint file
/// names its port and token. `Err` when nothing answers — no endpoint file,
/// or one left behind by an instance that's gone.
fn request_running_instance(
    profile: &str,
    read_timeout: Duration,
    request: impl FnOnce(String) -> std::io::Result<serde_json::Value>,
) -> std::io::Result<ShareResponse> {
    let path = endpoint_path(profile).ok_or_else(|| std::io::Error::other("no config directory"))?;
    let endpoint: Endpoint = serde_json::from_slice(&std::fs::read(path)?)?;
    let stream = TcpStream::connect_timeout(&([127, 0, 0, 1], endpoint.port).into(), Duration::from_secs(2))?;
    stream.set_read_timeout(Some(read_timeout))?;
    let mut request = serde_json::to_vec(&request(endpoint.token)?)?;
    request.push(b'\n');
    (&stream).write_all(&request)?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// A plain second launch of Continuity on Linux or Windows — from the app
/// menu, the Start menu, or the installer's "Launch Continuity now" while
/// it's already running: asks the running instance to show its window, so
/// the launch does what someone clicking the icon expects instead of
/// starting a second tray app on the same identity. True when an instance
/// answered (even to say no, as a version without this would), and this
/// one should exit; false when none is running and this one should start.
/// An answer refusing the token isn't this profile's instance: the endpoint
/// file was left behind, and its port has since gone to another one.
/// (macOS doesn't need this: launching an app that's already running just
/// reactivates it — see `Event::Reopen` in main.rs.)
#[cfg(any(target_os = "linux", target_os = "windows", test))]
pub fn hand_off_to_running_instance(profile: &str) -> bool {
    let response = request_running_instance(profile, Duration::from_secs(3), |token| {
        Ok(serde_json::json!({ "op": "open_window", "token": token }))
    });
    match response {
        Ok(response) if response.ok => true,
        Ok(response) if response.message == WRONG_TOKEN => false,
        Ok(response) => {
            eprintln!("Continuity is already running, but couldn't show its window: {}", response.message);
            true
        }
        Err(_) => false,
    }
}

/// Brings the right-click entries in line with who's connected right now.
/// Called on every connect/disconnect; cheap (a handful of tiny files).
pub fn sync_menu_entries(connected: &HashMap<String, String>) {
    let targets = menu_targets(connected);
    let Ok(exe) = std::env::current_exe() else { return };
    #[cfg(target_os = "linux")]
    linux::sync(&targets, &exe);
    #[cfg(target_os = "windows")]
    windows_send_to::sync(&targets, &exe);
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let _ = (targets, exe);
}

/// Removes every entry this app created — at quit, and at startup in case
/// the previous run didn't get to quit cleanly.
pub fn remove_menu_entries() {
    #[cfg(target_os = "linux")]
    linux::remove();
    #[cfg(target_os = "windows")]
    windows_send_to::remove();
}

#[cfg(target_os = "linux")]
mod linux {
    use super::MenuTarget;
    use std::path::{Path, PathBuf};

    const NAUTILUS_SUBMENU: &str = "Send with Continuity";
    const DOLPHIN_FILE: &str = "continuity-send.desktop";
    const NEMO_PREFIX: &str = "continuity-send-";

    fn data_home() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| d.data_dir().to_path_buf())
    }

    fn on_path(binary: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(binary).is_file()))
    }

    pub fn remove() {
        let Some(data) = data_home() else { return };
        let _ = std::fs::remove_dir_all(data.join("nautilus/scripts").join(NAUTILUS_SUBMENU));
        let _ = std::fs::remove_file(data.join("kio/servicemenus").join(DOLPHIN_FILE));
        let _ = std::fs::remove_file(data.join("kservices5/ServiceMenus").join(DOLPHIN_FILE));
        if let Ok(entries) = std::fs::read_dir(data.join("nemo/actions")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(NEMO_PREFIX) && name.ends_with(".nemo_action") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    pub fn sync(targets: &[MenuTarget], exe: &Path) {
        remove();
        if targets.is_empty() {
            return;
        }
        let Some(data) = data_home() else { return };
        if on_path("nautilus") {
            write_nautilus(&data, targets, exe);
        }
        if on_path("dolphin") {
            write_dolphin(&data, targets, exe);
        }
        if on_path("nemo") {
            write_nemo(&data, targets, exe);
        }
    }

    /// Files (Nautilus) shows every executable in its scripts folder under
    /// right-click > Scripts, with subfolders as submenus, and passes the
    /// selected files as arguments.
    fn write_nautilus(data: &Path, targets: &[MenuTarget], exe: &Path) {
        let dir = data.join("nautilus/scripts").join(NAUTILUS_SUBMENU);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        for target in targets {
            let script = format!(
                "#!/bin/sh\n# Created by Continuity while it's running; removed again when it quits.\nexec {} {} {} \"$@\"\n",
                shell_quote(&exe.to_string_lossy()),
                super::SHARE_TO_FLAG,
                shell_quote(&target.target),
            );
            write_executable(&dir.join(file_name_safe(&target.label)), &script);
        }
    }

    /// One service menu with an action per device, grouped under a submenu.
    /// Written for both KDE Frameworks 6 (`kio/servicemenus`) and 5
    /// (`kservices5/ServiceMenus`); both require the file to be executable
    /// when it lives in the user's own data directory.
    fn write_dolphin(data: &Path, targets: &[MenuTarget], exe: &Path) {
        let ids: Vec<String> = (0..targets.len()).map(|i| format!("continuity{i}")).collect();
        let mut file = format!(
            "[Desktop Entry]\nType=Service\nServiceTypes=KonqPopupMenu/Plugin\nX-KDE-ServiceTypes=KonqPopupMenu/Plugin\nMimeType=all/allfiles;\nActions={};\nX-KDE-Submenu={NAUTILUS_SUBMENU}\nIcon=continuity\n",
            ids.join(";")
        );
        for (id, target) in ids.iter().zip(targets) {
            file.push_str(&format!(
                "\n[Desktop Action {id}]\nName={}\nIcon=continuity\nExec={} {} {} %F\n",
                target.label.replace('\n', " "),
                desktop_exec_quote(&exe.to_string_lossy()),
                super::SHARE_TO_FLAG,
                desktop_exec_quote(&target.target),
            ));
        }
        for dir in ["kio/servicemenus", "kservices5/ServiceMenus"] {
            let dir = data.join(dir);
            if std::fs::create_dir_all(&dir).is_ok() {
                write_executable(&dir.join(DOLPHIN_FILE), &file);
            }
        }
    }

    fn write_nemo(data: &Path, targets: &[MenuTarget], exe: &Path) {
        let dir = data.join("nemo/actions");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        for (i, target) in targets.iter().enumerate() {
            let action = format!(
                "[Nemo Action]\nName=Send to {} with Continuity\nComment=Send the selected files with Continuity\nExec={} {} {} %F\nIcon-Name=continuity\nSelection=notnone\nExtensions=nodirs;\n",
                target.label.replace('\n', " "),
                desktop_exec_quote(&exe.to_string_lossy()),
                super::SHARE_TO_FLAG,
                desktop_exec_quote(&target.target),
            );
            let _ = std::fs::write(dir.join(format!("{NEMO_PREFIX}{i}.nemo_action")), action);
        }
    }

    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::write(path, contents).is_ok() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
        }
    }

    fn file_name_safe(label: &str) -> String {
        let cleaned: String = label.chars().map(|c| if c == '/' || c.is_control() { '-' } else { c }).collect();
        let cleaned = cleaned.trim().trim_start_matches('.').to_string();
        if cleaned.is_empty() { "Device".to_string() } else { cleaned }
    }

    /// POSIX single-quoting — safe for any string.
    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    /// Quoting for a `.desktop` `Exec=` argument, per the Desktop Entry spec:
    /// double quotes, with `"`, `` ` ``, `$` and `\` backslash-escaped. Field
    /// codes like `%F` stay outside the quotes, so a literal `%` is doubled.
    fn desktop_exec_quote(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' | '`' | '$' | '\\' => {
                    out.push('\\');
                    out.push(c);
                }
                '%' => out.push_str("%%"),
                _ => out.push(c),
            }
        }
        out.push('"');
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quoting_survives_hostile_names() {
            assert_eq!(shell_quote("it's"), r"'it'\''s'");
            assert_eq!(desktop_exec_quote("/opt/My Apps/$x\"%"), "\"/opt/My Apps/\\$x\\\"%%\"");
            assert_eq!(file_name_safe("../evil/name"), "-evil-name");
            assert_eq!(file_name_safe("  "), "Device");
        }
    }
}

/// Explorer's "Send to" menu lists whatever shortcuts are in the user's
/// SendTo folder, and appends the selected files to the shortcut's
/// arguments when one is picked. Entries are named "Continuity - <device>"
/// so they sort together, and so `remove` can find exactly its own.
#[cfg(target_os = "windows")]
mod windows_send_to {
    use super::MenuTarget;
    use std::path::{Path, PathBuf};

    const PREFIX: &str = "Continuity - ";

    fn send_to_dir() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join(r"Microsoft\Windows\SendTo"))
    }

    pub fn remove() {
        let Some(dir) = send_to_dir() else { return };
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(PREFIX) && name.ends_with(".lnk") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    pub fn sync(targets: &[MenuTarget], exe: &Path) {
        remove();
        let Some(dir) = send_to_dir() else { return };
        let links: Vec<(PathBuf, String, String)> = targets
            .iter()
            .map(|t| {
                (
                    dir.join(format!("{PREFIX}{}.lnk", file_name_safe(&t.label))),
                    format!("{} {}", super::SHARE_TO_FLAG, t.target),
                    format!("Send the selected files to {} with Continuity", t.label),
                )
            })
            .collect();
        let exe = exe.to_path_buf();
        // COM wants to be initialized on whichever thread uses it; a
        // short-lived thread of its own sidesteps whatever apartment state
        // the tray's event-loop thread already has. Joined, so the
        // entries are in place (or gone) by the time this returns.
        let _ = std::thread::spawn(move || unsafe {
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
            let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            for (path, args, description) in &links {
                if let Err(e) = create_shortcut(path, &exe, args, description) {
                    tracing::debug!("couldn't create {}: {e}", path.display());
                }
            }
            if init.is_ok() {
                CoUninitialize();
            }
        })
        .join();
    }

    unsafe fn create_shortcut(path: &Path, exe: &Path, args: &str, description: &str) -> windows::core::Result<()> {
        use windows::core::{Interface, HSTRING};
        use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

        let exe = HSTRING::from(exe.to_string_lossy().as_ref());
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&exe)?;
        link.SetArguments(&HSTRING::from(args))?;
        link.SetDescription(&HSTRING::from(description))?;
        link.SetIconLocation(&exe, 0)?;
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(path.to_string_lossy().as_ref()), true)
    }

    fn file_name_safe(label: &str) -> String {
        let cleaned: String =
            label.chars().map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '-' } else { c }).collect();
        let cleaned = cleaned.trim().trim_end_matches('.').to_string();
        if cleaned.is_empty() { "Device".to_string() } else { cleaned }
    }
}

/// Finder's "Open With > Continuity" (or dropping files onto the app): asks
/// which device with a native list dialog, then sends. Runs on its own
/// thread — the dialog blocks until answered. If opening the file is what
/// launched Continuity, trusted devices haven't reconnected yet, so this
/// gives them a few seconds before giving up.
#[cfg(target_os = "macos")]
pub fn handle_opened_files(paths: Vec<PathBuf>, connected: ConnectedPeers, commands: UnboundedSender<EngineCommand>) {
    std::thread::spawn(move || {
        if paths.is_empty() {
            return;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let targets = loop {
            let targets = menu_targets(&connected.lock().unwrap());
            if !targets.is_empty() || std::time::Instant::now() >= deadline {
                break targets;
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        if targets.is_empty() {
            crate::notify("No devices are connected — connect one, then try again.");
            return;
        }
        let prompt = match paths.as_slice() {
            [one] => format!("Send “{}” to:", one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            many => format!("Send {} items to:", many.len()),
        };
        let Some(target) = pick_target(&prompt, &targets) else { return };
        let response = dispatch(&target, &paths, &commands, &connected);
        if !response.ok {
            crate::notify(&response.message);
        }
    });
}

/// AppleScript's `choose from list`, run through `osascript` with the
/// labels passed as arguments rather than spliced into the script, so a
/// device name can't break (or inject into) the script no matter what it
/// contains. `None` if the user cancels.
#[cfg(target_os = "macos")]
fn pick_target(prompt: &str, targets: &[MenuTarget]) -> Option<String> {
    const SCRIPT: &str = r#"on run argv
    set promptText to item 1 of argv
    set choices to rest of argv
    set picked to choose from list choices with title "Continuity" with prompt promptText OK button name "Send" default items {item 1 of choices}
    if picked is false then return ""
    return item 1 of picked
end run"#;
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(SCRIPT)
        .arg(prompt)
        .args(targets.iter().map(|t| t.label.as_str()))
        .output()
        .map_err(|e| tracing::warn!("couldn't show the device picker: {e}"))
        .ok()?;
    let picked = String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_string();
    targets.iter().find(|t| t.label == picked).map(|t| t.target.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(list: &[(&str, &str)]) -> HashMap<String, String> {
        list.iter().map(|(id, name)| (id.to_string(), name.to_string())).collect()
    }

    #[test]
    fn menu_targets_are_sorted_disambiguated_and_offer_all_only_for_several() {
        assert!(menu_targets(&HashMap::new()).is_empty());

        let one = menu_targets(&peers(&[("aaaaaaaa", "Pixel")]));
        assert_eq!(one.iter().map(|t| t.label.as_str()).collect::<Vec<_>>(), ["Pixel"]);

        let several = menu_targets(&peers(&[("bbbbbbbb", "Pixel"), ("cccccccc", "MacBook"), ("dddddddd", "Pixel")]));
        let labels: Vec<&str> = several.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["MacBook", "Pixel (bbbbbb)", "Pixel (dddddd)", "All connected devices"]);
        assert_eq!(several.last().unwrap().target, ALL_DEVICES);
    }

    fn control_state(dir: &tempfile::TempDir) -> Arc<ControlState> {
        let device = crate::control::ThisDevice {
            name: "Desk".into(),
            id: "me".into(),
            platform: continuity_proto::Platform::Linux,
            version: "test".into(),
        };
        ControlState::new(device, dir.path().join("trust.json"), dir.path().join("Received"), None, None, Default::default())
    }

    fn no_window() -> OpenWindow {
        Arc::new(|| {})
    }

    /// Sends one request line to the running server and returns its first
    /// response line, plus the connection (for reading further lines).
    fn request(profile: &str, request: serde_json::Value) -> (serde_json::Value, BufReader<TcpStream>) {
        let endpoint: Endpoint = serde_json::from_slice(&std::fs::read(endpoint_path(profile).unwrap()).unwrap()).unwrap();
        let mut request = request;
        if request.get("token").is_none() {
            request["token"] = serde_json::Value::String(endpoint.token);
        }
        let stream = TcpStream::connect(("127.0.0.1", endpoint.port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut line = serde_json::to_vec(&request).unwrap();
        line.push(b'\n');
        (&stream).write_all(&line).unwrap();
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).unwrap();
        (serde_json::from_str(&response).unwrap(), reader)
    }

    #[test]
    fn share_requests_go_through_the_loopback_listener_and_need_the_token() {
        let (commands, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let connected: ConnectedPeers = Arc::new(Mutex::new(peers(&[("peer1", "Pixel")])));
        let profile = format!("share-test-{}", rand::random::<u32>());
        let dir = tempfile::tempdir().unwrap();
        let server = start_server(&profile, commands, connected, control_state(&dir), no_window()).expect("server starts");

        let file = std::env::temp_dir().join(format!("continuity-share-test-{}.txt", rand::random::<u32>()));
        std::fs::write(&file, b"hi").unwrap();

        let response = request_share(&profile, "peer1", vec![file.clone(), std::env::temp_dir()]).expect("request goes through");
        assert!(response.ok, "{}", response.message);
        assert!(response.message.contains("skipped 1 folder"), "{}", response.message);
        match sent.try_recv().expect("a SendFile command") {
            EngineCommand::SendFile { peer_crypto_id, path } => {
                assert_eq!(peer_crypto_id, "peer1");
                assert_eq!(PathBuf::from(path), file);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(sent.try_recv().is_err(), "the folder must not be sent");

        let offline = request_share(&profile, "someone-else", vec![file.clone()]).unwrap();
        assert!(!offline.ok);

        // Same port, wrong token: refused, nothing sent.
        let endpoint: Endpoint = serde_json::from_slice(&std::fs::read(endpoint_path(&profile).unwrap()).unwrap()).unwrap();
        let stream = TcpStream::connect(("127.0.0.1", endpoint.port)).unwrap();
        let mut forged = serde_json::to_vec(&ShareRequest { token: "guess".into(), target: "peer1".into(), paths: vec![file.clone()] }).unwrap();
        forged.push(b'\n');
        (&stream).write_all(&forged).unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        assert!(!serde_json::from_str::<ShareResponse>(&line).unwrap().ok);
        assert!(sent.try_recv().is_err());

        server.shutdown();
        assert!(request_share(&profile, "peer1", vec![file.clone()]).is_err(), "no endpoint once shut down");
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn the_settings_api_answers_on_the_same_socket_and_needs_the_token_too() {
        let (commands, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let connected: ConnectedPeers = Arc::new(Mutex::new(HashMap::new()));
        let profile = format!("control-test-{}", rand::random::<u32>());
        let dir = tempfile::tempdir().unwrap();
        let control = control_state(&dir);
        let server = start_server(&profile, commands, connected, control.clone(), no_window()).expect("server starts");

        let (status, _) = request(&profile, serde_json::json!({ "op": "status" }));
        assert_eq!(status["ok"], true);
        assert_eq!(status["status"]["device"]["name"], "Desk");
        assert_eq!(status["status"]["paused"], false);

        let (refused, _) = request(&profile, serde_json::json!({ "op": "set_paused", "paused": true, "token": "guess" }));
        assert_eq!(refused["ok"], false);
        assert!(sent.try_recv().is_err(), "a wrong token changes nothing");

        let (paused, _) = request(&profile, serde_json::json!({ "op": "set_paused", "paused": true }));
        assert_eq!(paused["ok"], true);
        assert!(matches!(sent.try_recv(), Ok(EngineCommand::SetPaused(true))));

        let (unknown, _) = request(&profile, serde_json::json!({ "op": "format_disk" }));
        assert_eq!(unknown["ok"], false);

        // Contract B's newer requests go the same way, and say what's wrong.
        let chosen = dir.path().join("Incoming");
        let (moved, _) = request(&profile, serde_json::json!({ "op": "set_received_files_dir", "path": chosen }));
        assert_eq!(moved["ok"], true, "{moved}");
        assert!(matches!(sent.try_recv(), Ok(EngineCommand::SetReceivedFilesDir { .. })));
        let (status, _) = request(&profile, serde_json::json!({ "op": "status" }));
        assert_eq!(status["status"]["received_files_dir"], chosen.display().to_string());
        let (relative, _) = request(&profile, serde_json::json!({ "op": "set_received_files_dir", "path": "Incoming" }));
        assert_eq!(relative["ok"], false);
        assert!(!relative["message"].as_str().unwrap().is_empty());
        let (unlisted, _) = request(&profile, serde_json::json!({ "op": "open_transfer", "id": "nope", "reveal": true }));
        assert_eq!(unlisted["ok"], false);

        // A watch gets the status straight away, then again on a change.
        let (first, mut watch) = request(&profile, serde_json::json!({ "op": "watch" }));
        assert_eq!(first["status"]["paused"], false);
        control.observe(&continuity_daemon::SyncEvent::PausedStateChanged { paused: true });
        let mut next = String::new();
        watch.read_line(&mut next).unwrap();
        let next: serde_json::Value = serde_json::from_str(&next).unwrap();
        assert_eq!(next["status"]["paused"], true);

        server.shutdown();
    }

    #[test]
    fn a_second_launch_hands_off_to_the_running_instance() {
        let profile = format!("handoff-test-{}", rand::random::<u32>());
        assert!(!hand_off_to_running_instance(&profile), "nothing running: start normally");

        let (commands, _sent) = tokio::sync::mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let opened = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = opened.clone();
        let open_window: OpenWindow = Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let server =
            start_server(&profile, commands, Arc::new(Mutex::new(HashMap::new())), control_state(&dir), open_window).expect("server starts");
        assert!(hand_off_to_running_instance(&profile), "running: hand off and exit");
        assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 1, "the running one was asked to show its window");

        let (refused, _) = request(&profile, serde_json::json!({ "op": "open_window", "token": "guess" }));
        assert_eq!(refused["ok"], false);
        assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 1, "not without the token");

        // Another profile's endpoint file, left behind, naming the port
        // this instance now has: not that profile's instance.
        let other = format!("{profile}-other");
        let port = serde_json::from_slice::<Endpoint>(&std::fs::read(endpoint_path(&profile).unwrap()).unwrap()).unwrap().port;
        let stale = Endpoint { port, token: "from-a-run-long-gone".into() };
        write_private_file(&endpoint_path(&other).unwrap(), &serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(!hand_off_to_running_instance(&other), "a stale endpoint: start normally");
        assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_file(endpoint_path(&other).unwrap());

        server.shutdown();
        assert!(!hand_off_to_running_instance(&profile), "an instance that quit doesn't count");
    }
}

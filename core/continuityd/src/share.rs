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
//! `start_server`) and exits — it never starts a second tray app.

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
/// `--share-to` target meaning every currently-connected device.
const ALL_DEVICES: &str = "all";

type ConnectedPeers = Arc<Mutex<HashMap<String, String>>>;

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
struct ShareResponse {
    ok: bool,
    message: String,
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
/// the engine only sends regular files.
fn dispatch(target: &str, paths: &[PathBuf], commands: &UnboundedSender<EngineCommand>, connected: &ConnectedPeers) -> ShareResponse {
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

/// Listens on a random loopback port and publishes `{port, token}` in a
/// file only this user can read. Every request has to carry that token, so
/// another local account can't push files out through this device — the
/// same boundary the trust store and identity files already rely on.
pub fn start_server(profile: &str, commands: UnboundedSender<EngineCommand>, connected: ConnectedPeers) -> Option<ShareServer> {
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

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if let Err(e) = serve_one(stream, &token, &commands, &connected) {
                tracing::debug!("share request failed: {e}");
            }
        }
    });
    Some(ShareServer { endpoint })
}

fn serve_one(stream: TcpStream, token: &str, commands: &UnboundedSender<EngineCommand>, connected: &ConnectedPeers) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new((&stream).take(1024 * 1024)).read_line(&mut line)?;
    let response = match serde_json::from_str::<ShareRequest>(&line) {
        Ok(request) if request.token == token => dispatch(&request.target, &request.paths, commands, connected),
        Ok(_) => ShareResponse { ok: false, message: "rejected: wrong token".to_string() },
        Err(e) => ShareResponse { ok: false, message: format!("malformed request: {e}") },
    };
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    (&stream).write_all(&out)
}

fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
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
    let path = endpoint_path(profile).ok_or_else(|| std::io::Error::other("no config directory"))?;
    let endpoint: Endpoint = serde_json::from_slice(&std::fs::read(path)?)?;
    let stream = TcpStream::connect_timeout(&([127, 0, 0, 1], endpoint.port).into(), Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut request = serde_json::to_vec(&ShareRequest { token: endpoint.token, target: target.to_string(), paths })?;
    request.push(b'\n');
    (&stream).write_all(&request)?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
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

    #[test]
    fn share_requests_go_through_the_loopback_listener_and_need_the_token() {
        let (commands, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let connected: ConnectedPeers = Arc::new(Mutex::new(peers(&[("peer1", "Pixel")])));
        let profile = format!("share-test-{}", rand::random::<u32>());
        let server = start_server(&profile, commands, connected).expect("server starts");

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
}

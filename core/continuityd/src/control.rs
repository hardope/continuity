//! What a desktop settings window talks to: a small JSON-lines API on the
//! same token-guarded loopback socket "Send with Continuity" uses (see
//! `share.rs`'s `start_server`). One request per connection — a status
//! snapshot, a live stream of them (`watch`), or a change: pausing,
//! pairing with a nearby device, forgetting one, its permissions to control
//! or unlock this computer, sending it files. Everything here is something
//! the tray menu can already do; this just gives it a window.
//!
//! State comes from two places. What the tray learns from engine events —
//! who's connected and on what platform, who's nearby, whether syncing is
//! paused, what's been sent and received — is mirrored here by `observe`.
//! Who's paired, and each paired device's permissions, come from the trust
//! store file the engine keeps, re-read for every snapshot (it's saved
//! atomically, so a read never sees half a write) and polled for changes.

use continuity_crypto::TrustStore;
use continuity_daemon::{EngineCommand, SyncEvent};
use continuity_proto::{Platform, ScreenLockOutcome, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc::UnboundedSender;

/// How often the trust store file (and, on macOS, the privacy permissions)
/// are checked for changes the tray never hears about as an event — a
/// remembered remote-control answer, say, or Accessibility being granted in
/// System Settings.
const TRUST_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A `watch` connection re-sends the current status at least this often
/// even when nothing changed — which is also how it notices a window that
/// closed without saying so (the write fails).
const WATCH_RESEND_INTERVAL: Duration = Duration::from_secs(30);

/// One request, tagged by `op`. The connection's token is checked before
/// this is even parsed.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlOp {
    Status,
    /// Keeps the connection open and sends a fresh status line whenever
    /// anything in it changes.
    Watch,
    SetPaused { paused: bool },
    /// Remembered consent to control this computer; turned off, that
    /// device's next request asks again.
    SetRemoteControlAllowed { device: String, allowed: bool },
    /// Linux only — the one platform that can be unlocked remotely.
    SetUnlockAllowed { device: String, allowed: bool },
    /// Pairs with a nearby device, or reconnects a paired one.
    Connect { device: String },
    Disconnect { device: String },
    /// Forgets a paired device: closes its connection and unpairs it.
    Forget { device: String },
    SendFiles { device: String, paths: Vec<PathBuf> },
    /// Forgets every paired device.
    Reset,
    /// macOS only: asks for a privacy permission — the system prompt the
    /// first time, System Settings at the right page after that.
    RequestPermission { permission: Permission },
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    ScreenRecording,
    Accessibility,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Status {
    pub device: ThisDevice,
    pub paused: bool,
    /// Paired devices, sorted by name.
    pub devices: Vec<PairedDevice>,
    /// Devices seen on the network that aren't paired, sorted by name.
    pub nearby: Vec<NearbyDevice>,
    pub received_files_dir: String,
    /// Whether a paired device can take remote control of this computer at
    /// all (off in a "lite" build).
    pub can_be_controlled: bool,
    /// Whether this computer can be unlocked remotely (Linux only).
    pub can_be_unlocked: bool,
    pub about: About,
    pub activity: Activity,
    /// The macOS privacy permissions Continuity needs, as this process has
    /// them. `None` on Linux and Windows, which have nothing to grant ahead
    /// of time.
    pub permissions: Option<Permissions>,
}

/// The rest of what the window's info panel shows.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct About {
    pub protocol_version: u32,
    /// "macOS 12.7.6", "Ubuntu 26.04 LTS", "Windows 11 24H2".
    pub os: String,
    /// When this run of Continuity started.
    pub started_at_unix: u64,
    pub log_file: Option<String>,
    /// Where the identity, paired devices and log live.
    pub config_dir: Option<String>,
}

/// What's happened since Continuity started.
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct Activity {
    /// Connections made to paired devices, reconnections included.
    pub connections: u64,
    /// Copies on this computer that went to at least one device.
    pub clipboard_sent: u64,
    pub clipboard_received: u64,
    pub files_sent: u64,
    pub bytes_sent: u64,
    pub files_received: u64,
    pub bytes_received: u64,
    /// Either way: a device controlling this computer, or this one another.
    pub remote_control_sessions: u64,
    /// Times a device locked or unlocked this computer.
    pub screen_locks: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
pub struct Permissions {
    /// What a controlling device sees. Without it, macOS shows it only the
    /// wallpaper.
    pub screen_recording: bool,
    /// Keyboard and mouse input from a controlling device, and a phone's
    /// play/pause/next.
    pub accessibility: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ThisDevice {
    pub name: String,
    pub id: String,
    pub platform: Platform,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PairedDevice {
    pub id: String,
    pub name: String,
    /// Known once the device has connected (or been seen nearby) since
    /// Continuity started.
    pub platform: Option<Platform>,
    pub connected: bool,
    pub paired_at_unix: u64,
    pub remote_control_allowed: bool,
    pub unlock_allowed: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct NearbyDevice {
    pub id: String,
    pub name: String,
    pub platform: Platform,
}

/// What `observe` keeps from engine events.
#[derive(Default)]
struct Live {
    paused: bool,
    connected: HashMap<String, String>,
    nearby: HashMap<String, (String, Platform)>,
    /// Every platform seen since start, so an offline paired device keeps
    /// its icon.
    platforms: HashMap<String, Platform>,
    activity: Activity,
    /// Sizes of the transfers under way, by transfer id — the events that
    /// finish one don't repeat it.
    transfer_sizes: HashMap<String, u64>,
    /// Bumped on every change a watcher should hear about.
    generation: u64,
}

pub struct ControlState {
    device: ThisDevice,
    trust_path: PathBuf,
    received_files_dir: PathBuf,
    about: About,
    live: Mutex<Live>,
    changed: Condvar,
}

impl ControlState {
    pub fn new(device: ThisDevice, trust_path: PathBuf, received_files_dir: PathBuf, log_file: Option<PathBuf>) -> Arc<Self> {
        let about = About {
            protocol_version: PROTOCOL_VERSION,
            os: os_description(),
            started_at_unix: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default(),
            log_file: log_file.map(|path| path.display().to_string()),
            config_dir: trust_path.parent().filter(|dir| !dir.as_os_str().is_empty()).map(|dir| dir.display().to_string()),
        };
        Arc::new(Self { device, trust_path, received_files_dir, about, live: Mutex::new(Live::default()), changed: Condvar::new() })
    }

    /// Mirrors the parts of an engine event a settings window shows.
    pub fn observe(&self, event: &SyncEvent) {
        let mut live = self.live.lock().unwrap();
        match event {
            SyncEvent::Connected { peer } => {
                live.connected.insert(peer.id.clone(), peer.name.clone());
                live.platforms.insert(peer.id.clone(), peer.platform);
                live.nearby.remove(&peer.id);
                live.activity.connections += 1;
            }
            SyncEvent::ClipboardBroadcast { peer_count } if *peer_count > 0 => live.activity.clipboard_sent += 1,
            SyncEvent::ClipboardReceived { .. } => live.activity.clipboard_received += 1,
            SyncEvent::FileSending { transfer_id, size_bytes, .. } | SyncEvent::FileReceiving { transfer_id, size_bytes, .. } => {
                live.transfer_sizes.insert(transfer_id.clone(), *size_bytes);
                return;
            }
            SyncEvent::FileSent { transfer_id, .. } => {
                let size = live.transfer_sizes.remove(transfer_id).unwrap_or_default();
                live.activity.files_sent += 1;
                live.activity.bytes_sent += size;
            }
            SyncEvent::FileReceived { transfer_id, .. } => {
                let size = live.transfer_sizes.remove(transfer_id).unwrap_or_default();
                live.activity.files_received += 1;
                live.activity.bytes_received += size;
            }
            SyncEvent::FileTransferFailed { transfer_id, .. } => {
                live.transfer_sizes.remove(transfer_id);
                return;
            }
            SyncEvent::RemoteControlSessionStarted { .. } => live.activity.remote_control_sessions += 1,
            SyncEvent::ScreenLockRequested { outcome: ScreenLockOutcome::Done, .. } => live.activity.screen_locks += 1,
            SyncEvent::Disconnected { peer_id, .. } | SyncEvent::WasRevoked { peer_id, .. } => {
                live.connected.remove(peer_id);
            }
            SyncEvent::PeerDiscovered { device } => {
                live.platforms.insert(device.id.clone(), device.platform);
                live.nearby.insert(device.id.clone(), (device.name.clone(), device.platform));
            }
            SyncEvent::Paired { peer } => {
                live.platforms.insert(peer.id.clone(), peer.platform);
                live.nearby.remove(&peer.id);
            }
            SyncEvent::WasReset => live.connected.clear(),
            SyncEvent::PausedStateChanged { paused } => live.paused = *paused,
            SyncEvent::UnlockPermissionChanged { .. } | SyncEvent::RevokedByPeer { .. } => {}
            _ => return,
        }
        live.generation += 1;
        drop(live);
        self.changed.notify_all();
    }

    fn bump(&self) {
        self.live.lock().unwrap().generation += 1;
        self.changed.notify_all();
    }

    /// Watches the trust store file — and the privacy permissions — for
    /// changes the tray never sees as an event, for as long as the process
    /// runs.
    pub fn spawn_trust_watcher(self: &Arc<Self>) {
        let state = Arc::clone(self);
        std::thread::spawn(move || {
            let modified = |path: &PathBuf| std::fs::metadata(path).and_then(|m| m.modified()).ok();
            let mut last = (modified(&state.trust_path), permissions());
            loop {
                std::thread::sleep(TRUST_POLL_INTERVAL);
                let now = (modified(&state.trust_path), permissions());
                if now != last {
                    last = now;
                    state.bump();
                }
            }
        });
    }

    pub fn status(&self) -> Status {
        let trust = TrustStore::load(self.trust_path.clone())
            .map_err(|e| tracing::warn!("couldn't read the trust store for the settings window: {e}"))
            .ok();
        let live = self.live.lock().unwrap();
        let mut devices: Vec<PairedDevice> = trust
            .as_ref()
            .map(|trust| {
                trust
                    .list()
                    .map(|d| PairedDevice {
                        id: d.id.clone(),
                        name: d.name.clone(),
                        platform: live.platforms.get(&d.id).copied(),
                        connected: live.connected.contains_key(&d.id),
                        paired_at_unix: d.paired_at_unix,
                        remote_control_allowed: trust.is_remote_control_allowed(&d.id),
                        unlock_allowed: trust.is_unlock_allowed(&d.id),
                    })
                    .collect()
            })
            .unwrap_or_default();
        devices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        let mut nearby: Vec<NearbyDevice> = live
            .nearby
            .iter()
            .filter(|(id, _)| !trust.as_ref().is_some_and(|t| t.is_trusted(id)))
            .map(|(id, (name, platform))| NearbyDevice { id: id.clone(), name: name.clone(), platform: *platform })
            .collect();
        nearby.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        Status {
            device: self.device.clone(),
            paused: live.paused,
            devices,
            nearby,
            received_files_dir: self.received_files_dir.display().to_string(),
            can_be_controlled: cfg!(all(feature = "remote-control", any(target_os = "macos", target_os = "windows", target_os = "linux"))),
            can_be_unlocked: cfg!(target_os = "linux"),
            about: self.about.clone(),
            activity: live.activity.clone(),
            permissions: permissions(),
        }
    }

    /// Sends a status line now, then again every time anything changes,
    /// until a write fails (the window went away).
    pub fn watch(&self, mut send: impl FnMut(&Status) -> std::io::Result<()>) {
        loop {
            let seen = self.live.lock().unwrap().generation;
            if send(&self.status()).is_err() {
                return;
            }
            let live = self.live.lock().unwrap();
            let _unchanged = self.changed.wait_timeout_while(live, WATCH_RESEND_INTERVAL, |live| live.generation == seen).unwrap();
        }
    }

    /// Carries out one change. `Err` is worded for the window to show.
    /// `Status`, `Watch` and `SendFiles` are the caller's to handle.
    pub fn apply(&self, op: ControlOp, commands: &UnboundedSender<EngineCommand>) -> Result<(), String> {
        let paired = |device: &str| -> Result<(), String> {
            let trust = TrustStore::load(self.trust_path.clone()).map_err(|e| format!("couldn't read the paired devices: {e}"))?;
            if trust.is_trusted(device) {
                Ok(())
            } else {
                Err("That device isn't paired.".to_string())
            }
        };
        let command = match op {
            ControlOp::SetPaused { paused } => EngineCommand::SetPaused(paused),
            ControlOp::SetRemoteControlAllowed { device, allowed } => {
                paired(&device)?;
                EngineCommand::SetRemoteControlAllowed { peer_crypto_id: device, allowed }
            }
            ControlOp::SetUnlockAllowed { device, allowed } => {
                if !cfg!(target_os = "linux") {
                    return Err("This computer can't be unlocked remotely.".to_string());
                }
                paired(&device)?;
                EngineCommand::SetUnlockAllowed { peer_crypto_id: device, allowed }
            }
            ControlOp::Connect { device } => EngineCommand::ReconnectPeer { peer_crypto_id: device },
            ControlOp::Disconnect { device } => EngineCommand::DisconnectPeer { peer_crypto_id: device },
            ControlOp::Forget { device } => {
                paired(&device)?;
                EngineCommand::RevokeDevice { peer_crypto_id: device }
            }
            ControlOp::Reset => EngineCommand::Reset,
            ControlOp::RequestPermission { permission } => {
                #[cfg(target_os = "macos")]
                {
                    crate::permissions_mac::request(permission);
                    self.bump();
                    return Ok(());
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = permission;
                    return Err("There's nothing to grant on this computer.".to_string());
                }
            }
            ControlOp::Status | ControlOp::Watch | ControlOp::SendFiles { .. } => {
                return Err("not a change".to_string());
            }
        };
        commands.send(command).map_err(|_| "Continuity is shutting down.".to_string())
    }
}

fn permissions() -> Option<Permissions> {
    #[cfg(target_os = "macos")]
    return Some(crate::permissions_mac::current());
    #[cfg(not(target_os = "macos"))]
    None
}

/// The operating system and its version, the way people know it.
#[cfg(target_os = "macos")]
fn os_description() -> String {
    let mut version = [0u8; 32];
    let mut len = version.len();
    let found =
        unsafe { libc::sysctlbyname(c"kern.osproductversion".as_ptr(), version.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) } == 0;
    let version = found.then(|| String::from_utf8_lossy(&version[..len]).trim_end_matches('\0').to_string());
    match version {
        Some(version) if !version.is_empty() => format!("macOS {version}"),
        _ => "macOS".to_string(),
    }
}

#[cfg(target_os = "linux")]
fn os_description() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|release| release.lines().find_map(|line| Some(line.strip_prefix("PRETTY_NAME=")?.trim_matches('"').to_string())))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Linux".to_string())
}

/// Windows 11 still calls itself "Windows 10" in `ProductName`; its build
/// number is what tells them apart.
#[cfg(target_os = "windows")]
fn os_description() -> String {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let read = |value: PCWSTR| -> Option<String> {
        let mut text = [0u16; 64];
        let mut bytes = std::mem::size_of_val(&text) as u32;
        unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
                value,
                RRF_RT_REG_SZ,
                None,
                Some(text.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        }
        .ok()
        .ok()?;
        let len = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        Some(String::from_utf16_lossy(&text[..len]))
    };
    let build: u32 = read(w!("CurrentBuild")).and_then(|build| build.parse().ok()).unwrap_or_default();
    let name = if build >= 22000 { "Windows 11" } else { "Windows 10" };
    match read(w!("DisplayVersion")) {
        Some(version) if !version.is_empty() => format!("{name} {version}"),
        _ => name.to_string(),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn os_description() -> String {
    std::env::consts::OS.to_string()
}

/// This desktop's own platform, as peers see it.
pub fn this_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Linux
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuity_crypto::TrustedDevice;
    use continuity_proto::DeviceInfo;

    fn state(dir: &tempfile::TempDir) -> Arc<ControlState> {
        let device = ThisDevice { name: "Desk".into(), id: "me".into(), platform: Platform::Linux, version: "test".into() };
        ControlState::new(device, dir.path().join("trust.json"), dir.path().join("Received"), Some(dir.path().join("continuityd.log")))
    }

    fn pair(dir: &tempfile::TempDir, id: &str, name: &str) -> TrustStore {
        let mut trust = TrustStore::load(dir.path().join("trust.json")).unwrap();
        trust.trust(TrustedDevice { id: id.into(), name: name.into(), paired_at_unix: 7 }).unwrap();
        trust
    }

    fn info(id: &str, name: &str, platform: Platform) -> DeviceInfo {
        DeviceInfo { id: id.into(), name: name.into(), platform, protocol_version: 2 }
    }

    /// The macOS settings app decodes this same file in its own tests
    /// (apps/macos/ContinuitySettings/Tests/SettingsUITests), so a field
    /// renamed on one side and not the other fails here or there.
    #[test]
    fn status_json_matches_the_settings_app_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../apps/macos/ContinuitySettings/Tests/SettingsUITests/Fixtures/status.json")).unwrap();
        let status = Status {
            device: ThisDevice { name: "MacBook Air".into(), id: "0f3a9c".into(), platform: Platform::MacOs, version: "0.1.6-beta.6".into() },
            paused: false,
            devices: vec![
                PairedDevice {
                    id: "pixel".into(),
                    name: "Pixel 8".into(),
                    platform: Some(Platform::Android),
                    connected: true,
                    paired_at_unix: 1_756_000_000,
                    remote_control_allowed: true,
                    unlock_allowed: false,
                },
                PairedDevice {
                    id: "office".into(),
                    name: "Office PC".into(),
                    platform: None,
                    connected: false,
                    paired_at_unix: 1_757_500_000,
                    remote_control_allowed: false,
                    unlock_allowed: false,
                },
            ],
            nearby: vec![NearbyDevice { id: "tab".into(), name: "Galaxy Tab S9".into(), platform: Platform::Android }],
            received_files_dir: "/Users/me/Downloads/Continuity".into(),
            can_be_controlled: true,
            can_be_unlocked: false,
            about: About {
                protocol_version: 2,
                os: "macOS 12.7.6".into(),
                started_at_unix: 1_759_900_000,
                log_file: Some("/Users/me/Library/Application Support/app.continuity.continuity/continuityd.log".into()),
                config_dir: Some("/Users/me/Library/Application Support/app.continuity.continuity".into()),
            },
            activity: Activity {
                connections: 3,
                clipboard_sent: 12,
                clipboard_received: 5,
                files_sent: 2,
                bytes_sent: 3_500_000,
                files_received: 1,
                bytes_received: 820_000,
                remote_control_sessions: 1,
                screen_locks: 0,
            },
            permissions: Some(Permissions { screen_recording: false, accessibility: true }),
        };
        assert_eq!(serde_json::json!({ "ok": true, "status": status }), fixture);
    }

    #[test]
    fn activity_counts_what_happened_since_start() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let phone = info("phone", "pixel", Platform::Android);
        state.observe(&SyncEvent::Connected { peer: phone.clone() });
        state.observe(&SyncEvent::ClipboardBroadcast { peer_count: 1 });
        state.observe(&SyncEvent::ClipboardBroadcast { peer_count: 0 });
        state.observe(&SyncEvent::ClipboardReceived { from_name: "pixel".into() });
        state.observe(&SyncEvent::FileSending { transfer_id: "t1".into(), to_name: "pixel".into(), file_name: "a".into(), size_bytes: 1000 });
        state.observe(&SyncEvent::FileSent { transfer_id: "t1".into(), file_name: "a".into(), to_name: "pixel".into() });
        state.observe(&SyncEvent::FileReceiving { transfer_id: "t2".into(), from_name: "pixel".into(), file_name: "b".into(), size_bytes: 50 });
        state.observe(&SyncEvent::FileTransferFailed { transfer_id: "t2".into(), reason: "cancelled".into() });
        state.observe(&SyncEvent::FileReceiving { transfer_id: "t3".into(), from_name: "pixel".into(), file_name: "c".into(), size_bytes: 70 });
        state.observe(&SyncEvent::FileReceived { transfer_id: "t3".into(), file_name: "c".into(), path: "/tmp/c".into() });
        state.observe(&SyncEvent::ScreenLockRequested {
            peer_id: "phone".into(),
            peer_name: "pixel".into(),
            action: continuity_proto::ScreenLockAction::Lock,
            outcome: ScreenLockOutcome::Done,
        });
        state.observe(&SyncEvent::ScreenLockRequested {
            peer_id: "phone".into(),
            peer_name: "pixel".into(),
            action: continuity_proto::ScreenLockAction::Unlock,
            outcome: ScreenLockOutcome::NotAllowed,
        });

        let activity = state.status().activity;
        assert_eq!(
            activity,
            Activity {
                connections: 1,
                clipboard_sent: 1,
                clipboard_received: 1,
                files_sent: 1,
                bytes_sent: 1000,
                files_received: 1,
                bytes_received: 70,
                remote_control_sessions: 0,
                screen_locks: 1,
            },
            "a copy nobody received, a failed transfer and a refused unlock don't count"
        );
    }

    #[test]
    fn about_names_this_os_and_where_things_are() {
        let dir = tempfile::tempdir().unwrap();
        let about = state(&dir).status().about;
        assert_eq!(about.protocol_version, PROTOCOL_VERSION);
        assert!(!about.os.is_empty());
        assert_eq!(about.config_dir.as_deref(), Some(dir.path().display().to_string().as_str()));
        assert!(about.log_file.unwrap().ends_with("continuityd.log"));
        assert!(about.started_at_unix > 1_700_000_000);
    }

    #[test]
    fn requests_parse_by_op() {
        let op: ControlOp = serde_json::from_str(r#"{"op":"set_remote_control_allowed","device":"abc","allowed":false}"#).unwrap();
        assert_eq!(op, ControlOp::SetRemoteControlAllowed { device: "abc".into(), allowed: false });
        assert_eq!(serde_json::from_str::<ControlOp>(r#"{"op":"watch"}"#).unwrap(), ControlOp::Watch);
        assert_eq!(
            serde_json::from_str::<ControlOp>(r#"{"op":"request_permission","permission":"screen_recording"}"#).unwrap(),
            ControlOp::RequestPermission { permission: Permission::ScreenRecording }
        );
        assert!(serde_json::from_str::<ControlOp>(r#"{"op":"format_disk"}"#).is_err());
    }

    #[test]
    fn status_combines_the_trust_store_with_live_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        pair(&dir, "phone", "pixel");
        pair(&dir, "laptop", "Air").allow_remote_control("phone").unwrap();

        state.observe(&SyncEvent::Connected { peer: info("phone", "pixel", Platform::Android) });
        state.observe(&SyncEvent::PeerDiscovered { device: info("stranger", "Living room", Platform::Windows) });
        state.observe(&SyncEvent::PausedStateChanged { paused: true });

        let status = state.status();
        assert!(status.paused);
        let names: Vec<&str> = status.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Air", "pixel"], "paired devices sorted by name, case-insensitively");
        let phone = &status.devices[1];
        assert!(phone.connected && phone.remote_control_allowed && !phone.unlock_allowed);
        assert_eq!(phone.platform, Some(Platform::Android));
        assert!(!status.devices[0].connected);
        assert_eq!(status.devices[0].platform, None, "never seen since start");
        assert_eq!(status.nearby, [NearbyDevice { id: "stranger".into(), name: "Living room".into(), platform: Platform::Windows }]);

        state.observe(&SyncEvent::Disconnected { peer_id: "phone".into(), peer_name: "pixel".into() });
        let phone = state.status().devices.into_iter().find(|d| d.id == "phone").unwrap();
        assert!(!phone.connected);
        assert_eq!(phone.platform, Some(Platform::Android), "an offline device keeps its platform");
    }

    #[test]
    fn a_device_that_gets_paired_leaves_the_nearby_list() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        state.observe(&SyncEvent::PeerDiscovered { device: info("new", "Tablet", Platform::Android) });
        pair(&dir, "new", "Tablet");
        assert!(state.status().nearby.is_empty(), "paired devices aren't nearby candidates");
    }

    #[test]
    fn watch_sends_a_fresh_status_when_something_changes() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = Arc::clone(&state);
        std::thread::spawn(move || {
            watcher.watch(|status| tx.send(status.paused).map_err(|_| std::io::Error::other("closed")));
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), false, "the current status, straight away");
        state.observe(&SyncEvent::PausedStateChanged { paused: true });
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), true, "and again on a change");
    }

    #[test]
    fn changes_are_refused_for_unpaired_devices_and_unlock_off_linux() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(state.apply(ControlOp::Forget { device: "nobody".into() }, &tx).is_err());
        assert!(state.apply(ControlOp::SetRemoteControlAllowed { device: "nobody".into(), allowed: true }, &tx).is_err());
        assert!(rx.try_recv().is_err(), "nothing reached the engine");

        pair(&dir, "phone", "pixel");
        state.apply(ControlOp::SetRemoteControlAllowed { device: "phone".into(), allowed: false }, &tx).unwrap();
        assert!(matches!(rx.try_recv(), Ok(EngineCommand::SetRemoteControlAllowed { allowed: false, .. })));

        let unlock = state.apply(ControlOp::SetUnlockAllowed { device: "phone".into(), allowed: true }, &tx);
        assert_eq!(unlock.is_ok(), cfg!(target_os = "linux"));
    }
}

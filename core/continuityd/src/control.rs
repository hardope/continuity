//! What a desktop Continuity window talks to: a small JSON-lines API on the
//! same token-guarded loopback socket "Send with Continuity" uses (see
//! `share.rs`'s `start_server`). One request per connection — a status
//! snapshot, a live stream of them (`watch`), or a change: pausing,
//! pairing with a nearby device, forgetting one, its permissions to control
//! or unlock this computer, sending it files, taking control of it, where
//! received files go. The window is Continuity's main window; the tray menu
//! only keeps a few quick actions.
//!
//! State comes from three places. What the tray learns from engine events —
//! who's connected and on what platform, who's nearby, whether syncing is
//! paused, what's being sent and received — is mirrored here by `observe`.
//! Who's paired, and each paired device's permissions, come from the trust
//! store file the engine keeps, re-read for every snapshot (it's saved
//! atomically, so a read never sees half a write) and polled for changes.
//! And the few choices that outlive a run — where received files go,
//! whether the window has ever been shown — live in `settings.json` next to
//! the trust store (see `AppSettings`), written only from here.

use continuity_crypto::TrustStore;
use continuity_daemon::{EngineCommand, RemoteControlRole, SyncEvent};
use continuity_proto::{Platform, ScreenLockOutcome, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};
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

/// The most often a `watch` connection hears about a transfer's progress.
/// The engine already throttles its progress events, but several transfers
/// at once each send their own — and every status a watcher gets re-reads
/// the trust store and, for the Linux/Windows window, re-renders the page.
/// Anything else that changes still goes out straight away.
const PROGRESS_PUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Finished transfers kept for the window's list, most recent first —
/// enough to find "the file that just came in" without keeping a whole
/// history in memory. Transfers under way are always all listed.
const FINISHED_TRANSFERS_KEPT: usize = 20;

/// What a transfer that failed before the engine ever named it is listed
/// as (see `observe`'s `FileTransferFailed`): the windows show these where
/// a file's and a device's names go.
const UNANNOUNCED_FILE: &str = "A file";
const UNANNOUNCED_DEVICE: &str = "a device";

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
    /// Looks for nearby devices again, rather than waiting for them to
    /// announce themselves.
    RefreshNearby,
    /// Asks a connected computer to let this one control it. It asks
    /// whoever's at it first, unless it already allows this device.
    RequestRemoteControl { device: String },
    /// Where files received from now on are saved. Must be a full path; the
    /// folder is created if it's missing, and has to be writable.
    SetReceivedFilesDir { path: PathBuf },
    /// Shows the received-files folder in the system's file manager.
    OpenReceivedFilesDir,
    /// Opens a file that finished arriving — or, with `reveal`, shows it in
    /// its folder. Only files in the transfers list, never any other path.
    OpenTransfer {
        id: String,
        #[serde(default)]
        reveal: bool,
    },
    /// A second launch of Continuity asking the running one to show its
    /// window (see `share.rs`'s `hand_off_to_running_instance`). Handled by
    /// the socket server, which is the only thing that can reach the window.
    OpenWindow,
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
    /// Where received files are going right now.
    pub received_files_dir: String,
    /// Whether a paired device can take remote control of this computer at
    /// all (off in a "lite" build).
    pub can_be_controlled: bool,
    /// Whether this computer can be unlocked remotely (Linux only).
    pub can_be_unlocked: bool,
    /// Whether this computer can take control of another one — it has the
    /// remote-control viewer (off in a "lite" build).
    pub can_control_others: bool,
    /// The computer this one is controlling right now, if any. Only one at
    /// a time: there's one viewer window, and the windows don't offer
    /// Control while this is set.
    pub controlling_device: Option<String>,
    /// Every transfer under way and the most recently finished ones, newest
    /// first.
    pub transfers: Vec<Transfer>,
    pub about: About,
    pub activity: Activity,
    /// The macOS privacy permissions Continuity needs, as this process has
    /// them. `None` on Linux and Windows, which have nothing to grant ahead
    /// of time.
    pub permissions: Option<Permissions>,
}

/// One file on its way to or from a device.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Transfer {
    pub id: String,
    pub direction: TransferDirection,
    pub file_name: String,
    /// The device it's going to or coming from.
    pub peer_name: String,
    pub size_bytes: u64,
    pub bytes_done: u64,
    pub state: TransferState,
    /// Why it failed, or why it's stalled.
    pub reason: Option<String>,
    /// Where a received file was saved — only once it's done.
    pub path: Option<String>,
    pub started_at_unix: u64,
    pub finished_at_unix: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Sending,
    Receiving,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Active,
    /// Lost its connection partway and waiting to pick up where it left
    /// off (see `SyncEvent::FileTransferStalled`).
    Stalled,
    Done,
    Failed,
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

/// What Continuity remembers between runs, in `settings.json` next to the
/// trust store. A missing file, or one that can't be read, is just the
/// defaults — nothing in here is worth refusing to start over.
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct AppSettings {
    /// Where received files go. `None` is the default, Downloads/Continuity.
    #[serde(default)]
    pub received_files_dir: Option<PathBuf>,
    /// Whether the window has been opened for someone at least once — it
    /// opens by itself on the very first launch, so a new install doesn't
    /// look like nothing happened.
    #[serde(default)]
    pub window_shown_once: bool,
    /// Whatever a newer version wrote that this one doesn't know, kept as
    /// it was rather than dropped the next time this version saves.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl AppSettings {
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|e| {
                tracing::warn!("ignoring {}, which can't be read ({e}) — using the default settings", path.display());
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!("couldn't read {} ({e}) — using the default settings", path.display());
                Self::default()
            }
        }
    }

    /// Written beside the old file and renamed over it, so a crash halfway
    /// leaves the old settings rather than a truncated file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temp, path)
    }
}

/// `settings.json` in the same folder as the trust store and log — or, for
/// a test profile, `settings.<profile>.json`, the way the log file is named.
pub fn settings_path(profile: &str) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("app", "continuity", "continuity")?;
    let file_name = if profile == "default" { "settings.json".to_string() } else { format!("settings.{profile}.json") };
    Some(dirs.config_dir().join(file_name))
}

/// Whether received files can go in `dir`: it's created if it's missing,
/// and a file is written there and removed again. Same check the engine
/// makes for `EngineCommand::SetReceivedFilesDir`, done here first so the
/// window can say what's wrong before anything is saved.
pub fn check_usable_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    if !dir.is_dir() {
        return Err("it isn't a folder".to_string());
    }
    let probe = dir.join(format!(".continuity-write-test-{:016x}", rand::random::<u64>()));
    std::fs::File::create(&probe).map_err(|e| format!("can't write there: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
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
    /// Transfers in the order they started. The finished ones are trimmed
    /// to `FINISHED_TRANSFERS_KEPT`, oldest finish first, by `finished`.
    transfers: Vec<Transfer>,
    /// Ids of the finished transfers in `transfers`, in the order they
    /// finished.
    finished: VecDeque<String>,
    /// The computers this one is controlling — peer id to session id. One,
    /// except for the moment between a second session starting and the
    /// tray ending the first (see `handle_remote_control_sync_event`).
    controlling: HashMap<String, String>,
    received_files_dir: PathBuf,
    /// Bumped on every change a watcher should hear about straight away.
    generation: u64,
    /// Bumped when only a transfer's progress changed — which a watcher
    /// hears about at most every `PROGRESS_PUSH_INTERVAL` (see `watch`).
    progress_generation: u64,
}

impl Live {
    fn transfer_mut(&mut self, id: &str) -> Option<&mut Transfer> {
        self.transfers.iter_mut().find(|t| t.id == id)
    }

    /// A transfer starting — or announced again for one already listed,
    /// which then just carries on.
    fn start_transfer(&mut self, id: &str, direction: TransferDirection, file_name: &str, peer_name: &str, size_bytes: u64) {
        if let Some(transfer) = self.transfer_mut(id) {
            transfer.state = TransferState::Active;
            transfer.reason = None;
            transfer.finished_at_unix = None;
            self.finished.retain(|finished| finished != id);
            return;
        }
        self.transfers.push(Transfer {
            id: id.to_string(),
            direction,
            file_name: file_name.to_string(),
            peer_name: peer_name.to_string(),
            size_bytes,
            bytes_done: 0,
            state: TransferState::Active,
            reason: None,
            path: None,
            started_at_unix: unix_now(),
            finished_at_unix: None,
        });
    }

    /// Marks a transfer finished and drops the oldest finished one past
    /// `FINISHED_TRANSFERS_KEPT`. Returns its size, for the activity counts.
    fn finish_transfer(&mut self, id: &str, finish: impl FnOnce(&mut Transfer)) -> Option<u64> {
        let transfer = self.transfer_mut(id)?;
        finish(transfer);
        transfer.finished_at_unix = Some(unix_now());
        let size = transfer.size_bytes;
        self.finished.retain(|finished| finished != id);
        self.finished.push_back(id.to_string());
        while self.finished.len() > FINISHED_TRANSFERS_KEPT {
            if let Some(oldest) = self.finished.pop_front() {
                self.transfers.retain(|t| t.id != oldest);
            }
        }
        Some(size)
    }
}

pub struct ControlState {
    device: ThisDevice,
    trust_path: PathBuf,
    about: About,
    /// Where `settings` is saved; `None` without a config folder, when
    /// changes still apply but only until Continuity quits.
    settings_path: Option<PathBuf>,
    settings: Mutex<AppSettings>,
    live: Mutex<Live>,
    changed: Condvar,
}

impl ControlState {
    /// `received_files_dir` is the folder the engine was actually started
    /// with, which isn't always the one `settings` asks for (see `main`).
    pub fn new(
        device: ThisDevice,
        trust_path: PathBuf,
        received_files_dir: PathBuf,
        log_file: Option<PathBuf>,
        settings_path: Option<PathBuf>,
        settings: AppSettings,
    ) -> Arc<Self> {
        let about = About {
            protocol_version: PROTOCOL_VERSION,
            os: os_description(),
            started_at_unix: unix_now(),
            log_file: log_file.map(|path| path.display().to_string()),
            config_dir: trust_path.parent().filter(|dir| !dir.as_os_str().is_empty()).map(|dir| dir.display().to_string()),
        };
        let live = Live { received_files_dir, ..Live::default() };
        Arc::new(Self {
            device,
            trust_path,
            about,
            settings_path,
            settings: Mutex::new(settings),
            live: Mutex::new(live),
            changed: Condvar::new(),
        })
    }

    /// Mirrors the parts of an engine event a Continuity window shows.
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
            SyncEvent::FileSending { transfer_id, to_name, file_name, size_bytes } => {
                live.start_transfer(transfer_id, TransferDirection::Sending, file_name, to_name, *size_bytes);
            }
            SyncEvent::FileReceiving { transfer_id, from_name, file_name, size_bytes } => {
                live.start_transfer(transfer_id, TransferDirection::Receiving, file_name, from_name, *size_bytes);
            }
            // The one event that arrives many times a second across several
            // transfers — see `PROGRESS_PUSH_INTERVAL`. Also what says a
            // stalled transfer picked up again.
            SyncEvent::FileTransferProgress { transfer_id, bytes_transferred, total_bytes, .. } => {
                let Some(transfer) = live.transfer_mut(transfer_id) else { return };
                if transfer.state == TransferState::Stalled {
                    transfer.state = TransferState::Active;
                    transfer.reason = None;
                    transfer.bytes_done = *bytes_transferred;
                    transfer.size_bytes = *total_bytes;
                } else if transfer.state == TransferState::Active {
                    transfer.bytes_done = *bytes_transferred;
                    transfer.size_bytes = *total_bytes;
                    live.progress_generation += 1;
                    drop(live);
                    self.changed.notify_all();
                    return;
                } else {
                    // A late progress event for one that already finished.
                    return;
                }
            }
            SyncEvent::FileTransferStalled { transfer_id, reason } => {
                let under_way = |t: &&mut Transfer| matches!(t.state, TransferState::Active | TransferState::Stalled);
                let Some(transfer) = live.transfer_mut(transfer_id).filter(under_way) else { return };
                transfer.state = TransferState::Stalled;
                transfer.reason = Some(reason.clone());
            }
            SyncEvent::FileSent { transfer_id, .. } => {
                let size = live.finish_transfer(transfer_id, |t| {
                    t.state = TransferState::Done;
                    t.bytes_done = t.size_bytes;
                    t.reason = None;
                });
                live.activity.files_sent += 1;
                live.activity.bytes_sent += size.unwrap_or_default();
            }
            SyncEvent::FileReceived { transfer_id, file_name, path } => {
                let size = live.finish_transfer(transfer_id, |t| {
                    t.state = TransferState::Done;
                    t.bytes_done = t.size_bytes;
                    t.reason = None;
                    t.file_name = file_name.clone();
                    t.path = Some(path.clone());
                });
                live.activity.files_received += 1;
                live.activity.bytes_received += size.unwrap_or_default();
            }
            SyncEvent::FileTransferFailed { transfer_id, reason } => {
                let fail = |t: &mut Transfer| {
                    t.state = TransferState::Failed;
                    t.reason = Some(reason.clone());
                };
                if live.finish_transfer(transfer_id, fail).is_none() {
                    // Never announced: the engine only says `FileSending`
                    // once the receiver has accepted, so a file it refused
                    // (not enough free space, say), or one that couldn't
                    // even be offered (unreadable, the device gone), fails
                    // under an id nothing here has seen — and wasn't listed
                    // at all, leaving the reason in a passing notification
                    // only. It's listed as a send that failed; which file
                    // and which device the engine doesn't say, so those are
                    // left generic, and the reason says the rest.
                    live.start_transfer(transfer_id, TransferDirection::Sending, UNANNOUNCED_FILE, UNANNOUNCED_DEVICE, 0);
                    live.finish_transfer(transfer_id, fail);
                }
            }
            SyncEvent::RemoteControlSessionStarted { peer_id, session_id, role, .. } => {
                live.activity.remote_control_sessions += 1;
                if *role == RemoteControlRole::Controlling {
                    live.controlling.insert(peer_id.clone(), session_id.clone());
                }
            }
            SyncEvent::RemoteControlSessionEnded { peer_id, session_id, .. } => {
                if live.controlling.get(peer_id) != Some(session_id) {
                    return;
                }
                live.controlling.remove(peer_id);
            }
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

    /// One transfer as the window lists it, while it's still listed — how
    /// the tray names the device a received file came from.
    pub fn transfer(&self, id: &str) -> Option<Transfer> {
        self.live.lock().unwrap().transfers.iter().find(|t| t.id == id).cloned()
    }

    /// True exactly once per install: the first time it's asked, when the
    /// window has never been shown. Remembered straight away, so a window
    /// that fails to open isn't retried at every login.
    pub fn first_window_showing(&self) -> bool {
        let mut settings = self.settings.lock().unwrap();
        if settings.window_shown_once {
            return false;
        }
        settings.window_shown_once = true;
        if let Some(path) = &self.settings_path {
            if let Err(e) = settings.save(path) {
                tracing::warn!("couldn't save {}: {e}", path.display());
            }
        }
        true
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
            .map_err(|e| tracing::warn!("couldn't read the trust store for the Continuity window: {e}"))
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
            received_files_dir: live.received_files_dir.display().to_string(),
            can_be_controlled: cfg!(all(feature = "remote-control", any(target_os = "macos", target_os = "windows", target_os = "linux"))),
            can_be_unlocked: cfg!(target_os = "linux"),
            can_control_others: cfg!(feature = "remote-control"),
            controlling_device: live.controlling.keys().min().cloned(),
            transfers: live.transfers.iter().rev().cloned().collect(),
            about: self.about.clone(),
            activity: live.activity.clone(),
            permissions: permissions(),
        }
    }

    /// Sends a status line now, then again every time anything changes,
    /// until a write fails (the window went away). A change to a transfer's
    /// progress alone waits until `PROGRESS_PUSH_INTERVAL` has passed since
    /// the last line, so a burst of them goes out as one.
    pub fn watch(&self, mut send: impl FnMut(&Status) -> std::io::Result<()>) {
        loop {
            let (seen, seen_progress) = {
                let live = self.live.lock().unwrap();
                (live.generation, live.progress_generation)
            };
            if send(&self.status()).is_err() {
                return;
            }
            let sent_at = Instant::now();
            let mut live = self.live.lock().unwrap();
            loop {
                if live.generation != seen {
                    break;
                }
                let now = Instant::now();
                let due = if live.progress_generation != seen_progress {
                    sent_at + PROGRESS_PUSH_INTERVAL
                } else {
                    sent_at + WATCH_RESEND_INTERVAL
                };
                if now >= due {
                    break;
                }
                live = self.changed.wait_timeout(live, due - now).unwrap().0;
            }
        }
    }

    /// Carries out one change. `Err` is worded for the window to show.
    /// `Status`, `Watch`, `SendFiles` and `OpenWindow` are the caller's to
    /// handle.
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
            // `RefreshDiscovery` on its own only redials paired devices whose
            // address is already known — it never asks the network again
            // (a second mDNS browse on the same daemon would orphan the
            // first; see `redial_disconnected_trusted_peers` in the engine),
            // so a nearby device whose announcement was missed stayed
            // missing from the very list this button sits on.
            // `NetworkChanged` is what makes the engine start discovery over
            // with a fresh daemon and browse, which asks every device on the
            // network to answer again. Both are sent, so a refresh still
            // redials paired devices whatever `NetworkChanged` does besides.
            ControlOp::RefreshNearby => {
                commands.send(EngineCommand::RefreshDiscovery).map_err(|_| "Continuity is shutting down.".to_string())?;
                EngineCommand::NetworkChanged
            }
            ControlOp::RequestRemoteControl { device } => {
                if !cfg!(feature = "remote-control") {
                    return Err("This build of Continuity can't control other computers.".to_string());
                }
                paired(&device)?;
                let (name, platform, controlling) = {
                    let live = self.live.lock().unwrap();
                    let controlling = live.controlling.keys().min().map(|other| live.connected.get(other).cloned().unwrap_or_else(|| "another computer".to_string()));
                    (live.connected.get(&device).cloned(), live.platforms.get(&device).copied(), controlling)
                };
                let Some(name) = name else {
                    return Err("That device isn't connected right now.".to_string());
                };
                // One at a time — there's one viewer window. (The windows
                // don't offer Control meanwhile; this is for a request
                // that was already on its way.)
                if let Some(other) = controlling {
                    return Err(format!("Already controlling {other} — close its window first."));
                }
                // Phones can control a computer, but can't be controlled.
                if matches!(platform, Some(Platform::Android | Platform::Ios)) {
                    return Err(format!("{name} can't be controlled — only computers can."));
                }
                EngineCommand::RequestRemoteControl { peer_crypto_id: device }
            }
            ControlOp::SetReceivedFilesDir { path } => return self.set_received_files_dir(path, commands),
            ControlOp::OpenReceivedFilesDir => {
                let dir = self.live.lock().unwrap().received_files_dir.clone();
                // Created on the first file received; make sure there's
                // something to show before that.
                let _ = std::fs::create_dir_all(&dir);
                let dir = dir.display().to_string();
                return crate::open_path(&dir).map_err(|e| format!("Couldn't open {dir}: {e}"));
            }
            ControlOp::OpenTransfer { id, reveal } => return self.open_transfer(&id, reveal),
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
            ControlOp::Status | ControlOp::Watch | ControlOp::SendFiles { .. } | ControlOp::OpenWindow => {
                return Err("not a change".to_string());
            }
        };
        commands.send(command).map_err(|_| "Continuity is shutting down.".to_string())
    }

    /// Checked and saved before the engine hears about it, so nothing
    /// changes unless all of it can.
    fn set_received_files_dir(&self, path: PathBuf, commands: &UnboundedSender<EngineCommand>) -> Result<(), String> {
        if !path.is_absolute() {
            return Err("Choose the folder by its full path.".to_string());
        }
        check_usable_dir(&path).map_err(|e| format!("Received files can't go in {}: {e}", path.display()))?;
        let engine_path = path.to_str().ok_or("That folder's name can't be used — choose another folder.")?.to_string();
        {
            let mut settings = self.settings.lock().unwrap();
            let mut changed = settings.clone();
            changed.received_files_dir = Some(path.clone());
            if let Some(settings_path) = &self.settings_path {
                changed.save(settings_path).map_err(|e| format!("Couldn't save the change: {e}"))?;
            }
            *settings = changed;
        }
        commands
            .send(EngineCommand::SetReceivedFilesDir { path: engine_path })
            .map_err(|_| "Continuity is shutting down.".to_string())?;
        tracing::info!("received files now go to {}", path.display());
        self.live.lock().unwrap().received_files_dir = path;
        self.bump();
        Ok(())
    }

    fn open_transfer(&self, id: &str, reveal: bool) -> Result<(), String> {
        let transfer = self.transfer(id).ok_or("That transfer isn't listed anymore.")?;
        let path = match (&transfer.direction, &transfer.state, &transfer.path) {
            (TransferDirection::Receiving, TransferState::Done, Some(path)) => path.clone(),
            _ => return Err("Only a file that finished arriving can be opened.".to_string()),
        };
        if !Path::new(&path).exists() {
            return Err(format!("'{}' isn't there anymore — it may have been moved or deleted.", transfer.file_name));
        }
        let opened = if reveal { crate::reveal_in_folder(&path) } else { crate::open_path(&path) };
        opened.map_err(|e| format!("Couldn't open '{}': {e}", transfer.file_name))
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
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
    use continuity_daemon::FileTransferDirection;
    use continuity_proto::DeviceInfo;

    fn state(dir: &tempfile::TempDir) -> Arc<ControlState> {
        let device = ThisDevice { name: "Desk".into(), id: "me".into(), platform: Platform::Linux, version: "test".into() };
        ControlState::new(
            device,
            dir.path().join("trust.json"),
            dir.path().join("Received"),
            Some(dir.path().join("continuityd.log")),
            Some(dir.path().join("settings.json")),
            AppSettings::default(),
        )
    }

    fn pair(dir: &tempfile::TempDir, id: &str, name: &str) -> TrustStore {
        let mut trust = TrustStore::load(dir.path().join("trust.json")).unwrap();
        trust.trust(TrustedDevice { id: id.into(), name: name.into(), paired_at_unix: 7 }).unwrap();
        trust
    }

    fn info(id: &str, name: &str, platform: Platform) -> DeviceInfo {
        DeviceInfo { id: id.into(), name: name.into(), platform, protocol_version: 2 }
    }

    fn sending(id: &str, to: &str, file: &str, size: u64) -> SyncEvent {
        SyncEvent::FileSending { transfer_id: id.into(), to_name: to.into(), file_name: file.into(), size_bytes: size }
    }

    fn receiving(id: &str, from: &str, file: &str, size: u64) -> SyncEvent {
        SyncEvent::FileReceiving { transfer_id: id.into(), from_name: from.into(), file_name: file.into(), size_bytes: size }
    }

    fn progress(id: &str, done: u64, total: u64, direction: FileTransferDirection) -> SyncEvent {
        SyncEvent::FileTransferProgress { transfer_id: id.into(), bytes_transferred: done, total_bytes: total, direction }
    }

    /// The macOS app decodes this same file in its own tests
    /// (apps/macos/ContinuitySettings/Tests/SettingsUITests), so a field
    /// renamed on one side and not the other fails here or there. The
    /// transfers are taken from the fixture as they are — what's checked
    /// for them is that they read back into `Transfer`s and write out
    /// exactly the same JSON again (`a_transfer_has_the_contract_shape`
    /// pins the shape itself).
    #[test]
    fn status_json_matches_the_settings_app_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../apps/macos/ContinuitySettings/Tests/SettingsUITests/Fixtures/status.json")).unwrap();
        let transfers: Vec<Transfer> =
            serde_json::from_value(fixture["status"]["transfers"].clone()).expect("the fixture's transfers read as `Transfer`s");
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
            can_control_others: true,
            controlling_device: None,
            transfers,
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

    /// Contract B's `Transfer`, field for field — with the optional ones
    /// written as `null`, not left out.
    #[test]
    fn a_transfer_has_the_contract_shape() {
        let active = Transfer {
            id: "t1".into(),
            direction: TransferDirection::Sending,
            file_name: "talk.mov".into(),
            peer_name: "Pixel 8".into(),
            size_bytes: 1_300_000_000,
            bytes_done: 650_000_000,
            state: TransferState::Active,
            reason: None,
            path: None,
            started_at_unix: 1_760_000_000,
            finished_at_unix: None,
        };
        let json = serde_json::json!({
            "id": "t1", "direction": "sending", "file_name": "talk.mov", "peer_name": "Pixel 8",
            "size_bytes": 1_300_000_000u64, "bytes_done": 650_000_000u64, "state": "active",
            "reason": null, "path": null, "started_at_unix": 1_760_000_000u64, "finished_at_unix": null
        });
        assert_eq!(serde_json::to_value(&active).unwrap(), json);
        assert_eq!(serde_json::from_value::<Transfer>(json).unwrap(), active, "and reads back");
        let states: Vec<serde_json::Value> = [TransferState::Active, TransferState::Stalled, TransferState::Done, TransferState::Failed]
            .iter()
            .map(|s| serde_json::to_value(s).unwrap())
            .collect();
        assert_eq!(states, ["active", "stalled", "done", "failed"]);
        assert_eq!(serde_json::to_value(TransferDirection::Receiving).unwrap(), "receiving");
    }

    #[test]
    fn status_says_whether_this_build_can_control_others() {
        let dir = tempfile::tempdir().unwrap();
        let status = serde_json::to_value(state(&dir).status()).unwrap();
        assert_eq!(status["can_control_others"], cfg!(feature = "remote-control"));
        assert_eq!(status["transfers"], serde_json::json!([]));
        assert_eq!(status["received_files_dir"], dir.path().join("Received").display().to_string());
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
        state.observe(&sending("t1", "pixel", "a", 1000));
        state.observe(&SyncEvent::FileSent { transfer_id: "t1".into(), file_name: "a".into(), to_name: "pixel".into() });
        state.observe(&receiving("t2", "pixel", "b", 50));
        state.observe(&SyncEvent::FileTransferFailed { transfer_id: "t2".into(), reason: "cancelled".into() });
        state.observe(&receiving("t3", "pixel", "c", 70));
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
    fn a_transfer_goes_from_active_through_stalled_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        state.observe(&sending("t1", "Pixel", "talk.mov", 1000));
        let t = state.transfer("t1").unwrap();
        assert_eq!((t.direction, t.state, t.bytes_done, t.size_bytes), (TransferDirection::Sending, TransferState::Active, 0, 1000));
        assert_eq!(t.peer_name, "Pixel");
        assert!(t.started_at_unix > 1_700_000_000 && t.finished_at_unix.is_none());

        state.observe(&progress("t1", 400, 1000, FileTransferDirection::Sending));
        assert_eq!(state.transfer("t1").unwrap().bytes_done, 400);

        state.observe(&SyncEvent::FileTransferStalled { transfer_id: "t1".into(), reason: "the connection dropped".into() });
        let t = state.transfer("t1").unwrap();
        assert_eq!(t.state, TransferState::Stalled);
        assert_eq!(t.reason.as_deref(), Some("the connection dropped"));
        assert_eq!(t.bytes_done, 400, "keeps what it had");

        state.observe(&progress("t1", 500, 1000, FileTransferDirection::Sending));
        let t = state.transfer("t1").unwrap();
        assert_eq!((t.state, t.reason, t.bytes_done), (TransferState::Active, None, 500), "progress means it resumed");

        state.observe(&SyncEvent::FileSent { transfer_id: "t1".into(), file_name: "talk.mov".into(), to_name: "Pixel".into() });
        let t = state.transfer("t1").unwrap();
        assert_eq!((t.state, t.bytes_done, t.path), (TransferState::Done, 1000, None), "a sent file has no path here");
        assert!(t.finished_at_unix.is_some());

        state.observe(&progress("t1", 900, 1000, FileTransferDirection::Sending));
        assert_eq!(state.transfer("t1").unwrap().state, TransferState::Done, "a late progress event doesn't revive it");
        state.observe(&SyncEvent::FileTransferStalled { transfer_id: "t1".into(), reason: "late".into() });
        assert_eq!(state.transfer("t1").unwrap().state, TransferState::Done, "nor does a late stall");
    }

    #[test]
    fn a_received_file_records_where_it_went_and_a_failure_why() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        state.observe(&receiving("in", "MacBook", "photo.jpg", 70));
        state.observe(&progress("in", 35, 70, FileTransferDirection::Receiving));
        state.observe(&SyncEvent::FileReceived {
            transfer_id: "in".into(),
            file_name: "photo (1).jpg".into(),
            path: "/home/me/Downloads/Continuity/photo (1).jpg".into(),
        });
        let t = state.transfer("in").unwrap();
        assert_eq!((t.direction, t.state, t.bytes_done), (TransferDirection::Receiving, TransferState::Done, 70));
        assert_eq!(t.path.as_deref(), Some("/home/me/Downloads/Continuity/photo (1).jpg"));
        assert_eq!(t.file_name, "photo (1).jpg", "the name it was saved under");
        assert_eq!(t.peer_name, "MacBook");

        state.observe(&receiving("big", "MacBook", "disk.img", 5_000_000_000));
        state.observe(&SyncEvent::FileTransferFailed { transfer_id: "big".into(), reason: "Not enough free space".into() });
        let t = state.transfer("big").unwrap();
        assert_eq!((t.state, t.reason.as_deref(), t.path), (TransferState::Failed, Some("Not enough free space"), None));

        let ids: Vec<String> = state.status().transfers.into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["big", "in"], "newest first");
    }

    #[test]
    fn a_send_that_fails_before_it_was_announced_is_still_listed() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let reason = "Not enough free space on Pixel 8 (needs 5.1 GB, 300 MB free)";
        state.observe(&SyncEvent::FileTransferFailed { transfer_id: "refused".into(), reason: reason.into() });
        let t = state.transfer("refused").expect("listed");
        assert_eq!((t.direction, t.state, t.reason.as_deref()), (TransferDirection::Sending, TransferState::Failed, Some(reason)));
        assert_eq!((t.file_name.as_str(), t.peer_name.as_str()), (UNANNOUNCED_FILE, UNANNOUNCED_DEVICE));
        assert_eq!((t.size_bytes, t.bytes_done, t.path), (0, 0, None));
        assert!(t.finished_at_unix.is_some());
        assert_eq!(state.status().transfers.len(), 1);
        assert_eq!(state.status().activity, Activity::default(), "nothing counted");

        // One the window already lists keeps its own names.
        state.observe(&sending("known", "Pixel 8", "talk.mov", 10));
        state.observe(&SyncEvent::FileTransferFailed { transfer_id: "known".into(), reason: "the connection dropped".into() });
        let t = state.transfer("known").unwrap();
        assert_eq!((t.file_name.as_str(), t.peer_name.as_str(), t.state), ("talk.mov", "Pixel 8", TransferState::Failed));
        assert_eq!(state.status().transfers.len(), 2);
    }

    #[test]
    fn only_the_twenty_most_recently_finished_transfers_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        state.observe(&sending("long", "Pixel", "movie.mkv", 10));
        for i in 0..25 {
            state.observe(&sending(&format!("t{i}"), "Pixel", "f", 1));
            state.observe(&SyncEvent::FileSent { transfer_id: format!("t{i}"), file_name: "f".into(), to_name: "Pixel".into() });
        }
        let transfers = state.status().transfers;
        assert_eq!(transfers.len(), 21, "the one under way and twenty finished");
        assert_eq!(transfers.first().unwrap().id, "t24");
        assert_eq!(transfers[19].id, "t5");
        assert_eq!(transfers.last().unwrap().id, "long", "under way, however old");
        assert!(state.transfer("t4").is_none());
    }

    #[test]
    fn progress_reaches_watchers_at_most_about_four_times_a_second() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        state.observe(&sending("t1", "Pixel", "big.iso", 1_000_000));
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = Arc::clone(&state);
        std::thread::spawn(move || {
            watcher.watch(|status| tx.send((Instant::now(), status.clone())).map_err(|_| std::io::Error::other("closed")));
        });
        rx.recv_timeout(Duration::from_secs(2)).expect("the current status, straight away");

        // A second of progress events every 5 ms.
        let started = Instant::now();
        let mut done = 0;
        while started.elapsed() < Duration::from_secs(1) {
            done += 1000;
            state.observe(&progress("t1", done, 1_000_000, FileTransferDirection::Sending));
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(PROGRESS_PUSH_INTERVAL * 2);
        let pushes: Vec<(Instant, Status)> = rx.try_iter().collect();
        assert!((2..=6).contains(&pushes.len()), "{} pushes for a second of progress", pushes.len());
        assert_eq!(pushes.last().unwrap().1.transfers[0].bytes_done, done, "the last push has the latest progress");

        // Anything else still goes out straight away, even right after a
        // progress push.
        state.observe(&progress("t1", done + 1, 1_000_000, FileTransferDirection::Sending));
        rx.recv_timeout(Duration::from_secs(1)).expect("that progress, pushed");
        let changed = Instant::now();
        state.observe(&SyncEvent::PausedStateChanged { paused: true });
        let (at, status) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(status.paused);
        assert!(at.duration_since(changed) < Duration::from_millis(150), "not held back by the progress interval");
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
        assert_eq!(
            serde_json::from_str::<ControlOp>(r#"{"op":"request_remote_control","device":"mac"}"#).unwrap(),
            ControlOp::RequestRemoteControl { device: "mac".into() }
        );
        assert_eq!(
            serde_json::from_str::<ControlOp>(r#"{"op":"set_received_files_dir","path":"/home/me/Incoming"}"#).unwrap(),
            ControlOp::SetReceivedFilesDir { path: "/home/me/Incoming".into() }
        );
        assert_eq!(serde_json::from_str::<ControlOp>(r#"{"op":"open_received_files_dir"}"#).unwrap(), ControlOp::OpenReceivedFilesDir);
        assert_eq!(
            serde_json::from_str::<ControlOp>(r#"{"op":"open_transfer","id":"t1","reveal":true}"#).unwrap(),
            ControlOp::OpenTransfer { id: "t1".into(), reveal: true }
        );
        assert_eq!(
            serde_json::from_str::<ControlOp>(r#"{"op":"open_transfer","id":"t1"}"#).unwrap(),
            ControlOp::OpenTransfer { id: "t1".into(), reveal: false },
            "reveal defaults to opening the file itself"
        );
        assert_eq!(serde_json::from_str::<ControlOp>(r#"{"op":"refresh_nearby"}"#).unwrap(), ControlOp::RefreshNearby);
        assert_eq!(serde_json::from_str::<ControlOp>(r#"{"op":"open_window"}"#).unwrap(), ControlOp::OpenWindow);
        assert!(serde_json::from_str::<ControlOp>(r#"{"op":"set_received_files_dir"}"#).is_err(), "a path is required");
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

    #[test]
    fn refresh_looks_for_nearby_devices_again() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state(&dir).apply(ControlOp::RefreshNearby, &tx).unwrap();
        assert!(matches!(rx.try_recv(), Ok(EngineCommand::RefreshDiscovery)), "paired devices are redialed");
        assert!(matches!(rx.try_recv(), Ok(EngineCommand::NetworkChanged)), "and the network is asked again, with a fresh mDNS browse");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn remote_control_is_only_requested_of_a_connected_paired_computer() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let request = |device: &str| state.apply(ControlOp::RequestRemoteControl { device: device.into() }, &tx);

        assert!(request("nobody").is_err(), "not paired");
        pair(&dir, "mac", "MacBook");
        pair(&dir, "phone", "Pixel");
        assert!(request("mac").is_err(), "paired but not connected");
        state.observe(&SyncEvent::Connected { peer: info("phone", "Pixel", Platform::Android) });
        assert!(request("phone").is_err(), "a phone can't be controlled");
        assert!(rx.try_recv().is_err(), "nothing reached the engine");

        state.observe(&SyncEvent::Connected { peer: info("mac", "MacBook", Platform::MacOs) });
        assert_eq!(request("mac").is_ok(), cfg!(feature = "remote-control"));
        if cfg!(feature = "remote-control") {
            assert!(matches!(rx.try_recv(), Ok(EngineCommand::RequestRemoteControl { peer_crypto_id }) if peer_crypto_id == "mac"));
        }
    }

    #[test]
    fn only_one_computer_is_controlled_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        pair(&dir, "mac", "MacBook");
        pair(&dir, "pc", "Office PC");
        state.observe(&SyncEvent::Connected { peer: info("mac", "MacBook", Platform::MacOs) });
        state.observe(&SyncEvent::Connected { peer: info("pc", "Office PC", Platform::Windows) });
        let started = |peer: &str, name: &str, session: &str, role: RemoteControlRole| SyncEvent::RemoteControlSessionStarted {
            peer_id: peer.into(),
            peer_name: name.into(),
            session_id: session.into(),
            role,
        };
        let ended = |peer: &str, name: &str, session: &str| SyncEvent::RemoteControlSessionEnded {
            peer_id: peer.into(),
            peer_name: name.into(),
            session_id: session.into(),
            reason: None,
        };

        // Being controlled doesn't count.
        state.observe(&started("pc", "Office PC", "s0", RemoteControlRole::Controlled));
        assert_eq!(state.status().controlling_device, None);
        state.observe(&ended("pc", "Office PC", "s0"));

        state.observe(&started("mac", "MacBook", "s1", RemoteControlRole::Controlling));
        assert_eq!(state.status().controlling_device.as_deref(), Some("mac"));
        let refused = state.apply(ControlOp::RequestRemoteControl { device: "pc".into() }, &tx);
        if cfg!(feature = "remote-control") {
            let refused = refused.unwrap_err();
            assert!(refused.contains("MacBook"), "{refused}");
        } else {
            assert!(refused.is_err());
        }
        assert!(rx.try_recv().is_err(), "nothing reached the engine");

        // The end of some other session with it changes nothing.
        state.observe(&ended("mac", "MacBook", "older"));
        assert_eq!(state.status().controlling_device.as_deref(), Some("mac"));

        state.observe(&ended("mac", "MacBook", "s1"));
        assert_eq!(state.status().controlling_device, None);
        assert_eq!(state.apply(ControlOp::RequestRemoteControl { device: "pc".into() }, &tx).is_ok(), cfg!(feature = "remote-control"));
    }

    #[test]
    fn a_received_files_folder_is_checked_saved_and_passed_on() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let relative = state.apply(ControlOp::SetReceivedFilesDir { path: "Incoming".into() }, &tx);
        assert!(relative.is_err(), "a relative path is refused");
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let not_a_folder = state.apply(ControlOp::SetReceivedFilesDir { path: file.clone() }, &tx).unwrap_err();
        assert!(not_a_folder.contains("a-file"), "{not_a_folder}");
        assert!(rx.try_recv().is_err(), "nothing reached the engine");
        assert!(!dir.path().join("settings.json").exists(), "nothing was saved");

        let chosen = dir.path().join("Some/New Folder");
        state.apply(ControlOp::SetReceivedFilesDir { path: chosen.clone() }, &tx).unwrap();
        assert!(chosen.is_dir(), "created");
        assert!(std::fs::read_dir(&chosen).unwrap().next().is_none(), "the write test left nothing behind");
        assert!(matches!(rx.try_recv(), Ok(EngineCommand::SetReceivedFilesDir { path }) if path == chosen.display().to_string()));
        assert_eq!(state.status().received_files_dir, chosen.display().to_string());
        let saved = AppSettings::load(&dir.path().join("settings.json"));
        assert_eq!(saved.received_files_dir.as_deref(), Some(chosen.as_path()));
    }

    #[test]
    fn only_a_finished_received_file_can_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let open = |id: &str| state.apply(ControlOp::OpenTransfer { id: id.into(), reveal: false }, &tx);
        assert!(open("unknown").is_err());

        state.observe(&sending("out", "Pixel", "a.txt", 1));
        state.observe(&SyncEvent::FileSent { transfer_id: "out".into(), file_name: "a.txt".into(), to_name: "Pixel".into() });
        assert!(open("out").is_err(), "a sent file isn't opened from here");

        state.observe(&receiving("in", "Pixel", "b.txt", 1));
        assert!(open("in").is_err(), "not finished yet");
        let gone = dir.path().join("b.txt");
        state.observe(&SyncEvent::FileReceived { transfer_id: "in".into(), file_name: "b.txt".into(), path: gone.display().to_string() });
        let error = open("in").unwrap_err();
        assert!(error.contains("isn't there anymore"), "{error}");
    }

    #[test]
    fn settings_load_defaults_for_a_missing_or_corrupt_file_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/settings.json");
        assert_eq!(AppSettings::load(&path), AppSettings::default(), "missing");
        assert_eq!(AppSettings::default().received_files_dir, None);
        assert!(!AppSettings::default().window_shown_once);

        let settings = AppSettings { received_files_dir: Some("/media/usb/Incoming".into()), window_shown_once: true, ..AppSettings::default() };
        settings.save(&path).unwrap();
        assert_eq!(AppSettings::load(&path), settings);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(),
            serde_json::json!({ "received_files_dir": "/media/usb/Incoming", "window_shown_once": true }),
            "Contract C's shape"
        );

        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(AppSettings::load(&path), AppSettings::default(), "corrupt");
        std::fs::write(&path, b"[1, 2]").unwrap();
        assert_eq!(AppSettings::load(&path), AppSettings::default(), "not an object");
        std::fs::write(&path, br#"{"window_shown_once": true}"#).unwrap();
        assert_eq!(AppSettings::load(&path), AppSettings { window_shown_once: true, ..AppSettings::default() }, "partial");

        std::fs::write(&path, br#"{"received_files_dir": null, "window_shown_once": false, "from_a_newer_version": 3}"#).unwrap();
        let newer = AppSettings::load(&path);
        newer.save(&path).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["from_a_newer_version"], 3, "what this version doesn't know survives a save");
    }

    #[test]
    fn settings_files_are_per_profile() {
        let default = settings_path("default").unwrap();
        assert_eq!(default.file_name().unwrap(), "settings.json");
        assert_eq!(settings_path("alice").unwrap().file_name().unwrap(), "settings.alice.json");
        assert_eq!(default.parent(), TrustStore::default_path("default").unwrap().parent(), "next to the trust store");
    }

    #[test]
    fn the_window_opens_by_itself_only_the_first_time() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        assert!(state.first_window_showing());
        assert!(!state.first_window_showing());
        assert!(AppSettings::load(&dir.path().join("settings.json")).window_shown_once, "remembered");
    }
}

//! UniFFI bindings surface for the Android/iOS shells (Phase 2/3). Mirrors
//! `continuityd`'s role for desktop: a thin shell over
//! `continuity-daemon`'s engine, but talking to Kotlin/Swift instead of a
//! native GUI toolkit.
//!
//! Two callback interfaces do the cross-language talking: `EventListener`
//! (Rust -> host language, for `SyncEvent`s) and `ClipboardProvider` (host
//! language -> Rust, since Android/iOS clipboard access has to happen in
//! Kotlin/Swift — there's no background-thread OS clipboard API to call
//! into from Rust the way desktop's `arboard` works).

use continuity_crypto::{Identity, TrustStore};
use continuity_daemon::{ClipboardBackend, EngineCommand, EngineConfig};
use continuity_proto::{
    DeviceInfo as CoreDeviceInfo, MediaCommand as CoreMediaCommand, Platform as CorePlatform,
};
use std::path::PathBuf;
use std::sync::Arc;

uniffi::setup_scaffolding!();

/// Routes the Rust side's `tracing` output to `logcat` on Android. Without
/// this, every `tracing::debug!`/`warn!` call in `continuity-daemon` and
/// friends goes to stdout — which doesn't exist for an Android app, so it's
/// silently discarded and there's no way to see what the engine is doing.
/// No-op on iOS (Console.app / os_log integration is a reasonable follow-up
/// but not needed yet — `print!`-based debugging has been enough there).
/// Safe to call more than once; only the first call takes effect.
#[uniffi::export]
pub fn init_android_logging() {
    #[cfg(target_os = "android")]
    {
        use std::sync::Once;
        use tracing_subscriber::prelude::*;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            tracing_subscriber::registry()
                .with(paranoid_android::layer("continuity_ffi"))
                .init();
        });
    }
}

#[derive(uniffi::Error, Debug, thiserror::Error)]
pub enum FfiError {
    #[error("{0}")]
    Internal(String),
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiDeviceInfo {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub protocol_version: u32,
}

impl From<CoreDeviceInfo> for FfiDeviceInfo {
    fn from(d: CoreDeviceInfo) -> Self {
        let platform = match d.platform {
            CorePlatform::MacOs => "mac_os",
            CorePlatform::Windows => "windows",
            CorePlatform::Linux => "linux",
            CorePlatform::Android => "android",
            CorePlatform::Ios => "ios",
        };
        Self {
            id: d.id,
            name: d.name,
            platform: platform.to_string(),
            protocol_version: d.protocol_version,
        }
    }
}

/// Mirrors `continuity_proto::MediaCommand` — see
/// `ContinuityEngine::send_media_command` for where the host language uses
/// this.
#[derive(uniffi::Enum, Debug, Clone, Copy)]
pub enum FfiMediaCommand {
    PlayPause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
}

impl From<FfiMediaCommand> for CoreMediaCommand {
    fn from(c: FfiMediaCommand) -> Self {
        match c {
            FfiMediaCommand::PlayPause => CoreMediaCommand::PlayPause,
            FfiMediaCommand::Next => CoreMediaCommand::Next,
            FfiMediaCommand::Previous => CoreMediaCommand::Previous,
            FfiMediaCommand::VolumeUp => CoreMediaCommand::VolumeUp,
            FfiMediaCommand::VolumeDown => CoreMediaCommand::VolumeDown,
        }
    }
}

/// Mirrors `continuity_proto::NowPlayingInfo`. `artwork` is an empty list
/// rather than the host language's null/empty-string convention for "no
/// artwork" — decode it as an image (e.g. `BitmapFactory.decodeByteArray`
/// on Android) only when non-empty.
#[derive(uniffi::Record, Debug, Clone)]
pub struct FfiNowPlayingInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub artwork: Vec<u8>,
    pub is_playing: bool,
}

impl From<continuity_proto::NowPlayingInfo> for FfiNowPlayingInfo {
    fn from(i: continuity_proto::NowPlayingInfo) -> Self {
        Self {
            title: i.title,
            artist: i.artist,
            album: i.album,
            artwork: i.artwork,
            is_playing: i.is_playing,
        }
    }
}

#[derive(uniffi::Enum, Debug, Clone)]
pub enum FfiSyncEvent {
    Listening { port: u16 },
    PairingRequested { peer: FfiDeviceInfo, code: String },
    Paired { peer: FfiDeviceInfo },
    PairingDeclined { peer_name: String },
    Connected { peer: FfiDeviceInfo },
    Disconnected { peer_id: String, peer_name: String },
    ClipboardReceived { from_name: String },
    ClipboardBroadcast { peer_count: u32 },
    FileReceiving { transfer_id: String, from_name: String, file_name: String, size_bytes: u64 },
    FileReceived { transfer_id: String, file_name: String, path: String },
    FileSent { transfer_id: String, file_name: String, to_name: String },
    FileTransferFailed { transfer_id: String, reason: String },
    Error { message: String },
    WasReset,
    PausedStateChanged { paused: bool },
    ReconnectFailed { peer_id: String },
    NowPlayingChanged { peer_id: String, peer_name: String, info: FfiNowPlayingInfo },
    PeerDiscovered { device: FfiDeviceInfo },
    PeerActivity { peer_id: String, seconds_since_activity: u64 },
}

impl From<continuity_daemon::SyncEvent> for FfiSyncEvent {
    fn from(event: continuity_daemon::SyncEvent) -> Self {
        use continuity_daemon::SyncEvent as E;
        match event {
            E::Listening { port } => FfiSyncEvent::Listening { port },
            E::PairingRequested { peer, code } => FfiSyncEvent::PairingRequested { peer: peer.into(), code },
            E::Paired { peer } => FfiSyncEvent::Paired { peer: peer.into() },
            E::PairingDeclined { peer_name } => FfiSyncEvent::PairingDeclined { peer_name },
            E::Connected { peer } => FfiSyncEvent::Connected { peer: peer.into() },
            E::Disconnected { peer_id, peer_name } => FfiSyncEvent::Disconnected { peer_id, peer_name },
            E::ClipboardReceived { from_name } => FfiSyncEvent::ClipboardReceived { from_name },
            E::ClipboardBroadcast { peer_count } => FfiSyncEvent::ClipboardBroadcast { peer_count: peer_count as u32 },
            E::FileReceiving { transfer_id, from_name, file_name, size_bytes } => {
                FfiSyncEvent::FileReceiving { transfer_id, from_name, file_name, size_bytes }
            }
            E::FileReceived { transfer_id, file_name, path } => {
                FfiSyncEvent::FileReceived { transfer_id, file_name, path }
            }
            E::FileSent { transfer_id, file_name, to_name } => {
                FfiSyncEvent::FileSent { transfer_id, file_name, to_name }
            }
            E::FileTransferFailed { transfer_id, reason } => {
                FfiSyncEvent::FileTransferFailed { transfer_id, reason }
            }
            E::Error(message) => FfiSyncEvent::Error { message },
            E::WasReset => FfiSyncEvent::WasReset,
            E::PausedStateChanged { paused } => FfiSyncEvent::PausedStateChanged { paused },
            E::ReconnectFailed { peer_id } => FfiSyncEvent::ReconnectFailed { peer_id },
            E::NowPlayingChanged { peer_id, peer_name, info } => {
                FfiSyncEvent::NowPlayingChanged { peer_id, peer_name, info: info.into() }
            }
            E::PeerDiscovered { device } => FfiSyncEvent::PeerDiscovered { device: device.into() },
            E::PeerActivity { peer_id, seconds_since_activity } => {
                FfiSyncEvent::PeerActivity { peer_id, seconds_since_activity }
            }
        }
    }
}

/// Implemented by the host language to receive engine events (Kotlin
/// class / Swift type conforming to the generated protocol), passed to
/// `ContinuityEngine::start`.
#[uniffi::export(with_foreign)]
pub trait EventListener: Send + Sync {
    fn on_event(&self, event: FfiSyncEvent);
}

/// Implemented by the host language to bridge the platform clipboard
/// (`ClipboardManager` on Android, `UIPasteboard` on iOS) into the engine.
#[uniffi::export(with_foreign)]
pub trait ClipboardProvider: Send + Sync {
    fn get_text(&self) -> Option<String>;
    fn set_text(&self, text: String);
}

struct ClipboardBridge(Arc<dyn ClipboardProvider>);

impl ClipboardBackend for ClipboardBridge {
    fn get_text(&self) -> Option<String> {
        self.0.get_text()
    }

    fn set_text(&self, text: &str) {
        self.0.set_text(text.to_string());
    }
}

/// A running engine instance, owned by the host language for the lifetime
/// of the app/service. Holds its own tokio runtime — the host platform's
/// own threading model (Android foreground service thread, iOS app
/// process) doesn't need to know tokio exists.
#[derive(uniffi::Object)]
pub struct ContinuityEngine {
    commands: tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    _runtime: tokio::runtime::Runtime,
}

#[uniffi::export]
impl ContinuityEngine {
    /// `identity_der` and `data_dir` are supplied by the host app rather
    /// than derived here, unlike the desktop shells — Android and iOS have
    /// no OS-level keychain reachable from Rust and no ambient "app config
    /// directory" convention the `directories` crate can guess at, so the
    /// host provides both: `identity_der` is PKCS8 bytes the host persists
    /// in Android Keystore-backed storage or the real iOS Keychain (create
    /// one on first run with `generate_identity_der()`), and `data_dir` is
    /// the app's own sandboxed storage directory (`Context.filesDir` /
    /// `FileManager` application support URL).
    #[uniffi::constructor]
    pub fn start(
        identity_der: Vec<u8>,
        profile: String,
        device_name: String,
        data_dir: String,
        received_files_dir: String,
        clipboard: Arc<dyn ClipboardProvider>,
        listener: Arc<dyn EventListener>,
    ) -> Result<Arc<Self>, FfiError> {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| FfiError::Internal(format!("couldn't start tokio runtime: {e}")))?;

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        runtime.spawn(async move {
            let setup = async {
                let identity = Identity::from_pkcs8_der(&identity_der).map_err(|e| e.to_string())?;
                let trust_store_path =
                    PathBuf::from(&data_dir).join(format!("trusted_devices.{profile}.json"));
                let trust_store = TrustStore::load(trust_store_path).map_err(|e| e.to_string())?;
                let config = EngineConfig {
                    identity,
                    device_name,
                    trust_store,
                    clipboard: Arc::new(ClipboardBridge(clipboard)),
                    // Mobile only sends media commands (see
                    // ContinuityEngine::send_media_command below), it
                    // doesn't need to act on receiving one.
                    media: Arc::new(continuity_daemon::NoopMediaController),
                    received_files_dir: PathBuf::from(received_files_dir),
                };
                continuity_daemon::start(config).await.map_err(|e| e.to_string())
            };

            match setup.await {
                Ok(mut engine) => {
                    tracing::debug!("engine setup complete, forwarding events to host listener");
                    let _ = ready_tx.send(Ok(engine.command_sender()));
                    while let Some(event) = engine.events.recv().await {
                        listener.on_event(event.into());
                    }
                    tracing::warn!("event forwarding loop ended (engine event channel closed)");
                }
                Err(e) => {
                    tracing::warn!("engine setup failed: {e}");
                    let _ = ready_tx.send(Err(e));
                }
            }
        });

        let commands = ready_rx
            .recv()
            .map_err(|_| FfiError::Internal("engine task exited before starting".to_string()))?
            .map_err(FfiError::Internal)?;

        Ok(Arc::new(Self {
            commands,
            _runtime: runtime,
        }))
    }

    pub fn confirm_pairing(&self, peer_id: String, accept: bool) {
        let _ = self.commands.send(EngineCommand::ConfirmPairing {
            peer_crypto_id: peer_id,
            accept,
        });
    }

    pub fn send_file(&self, peer_id: String, path: String) {
        let _ = self.commands.send(EngineCommand::SendFile {
            peer_crypto_id: peer_id,
            path,
        });
    }

    /// Clears every paired device and disconnects all active peers — the
    /// host app should confirm with the user before calling this, there's
    /// no undo.
    pub fn reset(&self) {
        let _ = self.commands.send(EngineCommand::Reset);
    }

    /// Temporarily stop syncing (new connections, dialing, clipboard) in
    /// either direction, without stopping the engine or foreground
    /// service. Call again with `false` to resume.
    pub fn set_paused(&self, paused: bool) {
        let _ = self.commands.send(EngineCommand::SetPaused(paused));
    }

    /// Drops the connection to one specific peer without forgetting it —
    /// unlike `reset`, it stays paired and `reconnect_peer` can bring it
    /// back. Sticks until then; the peer won't silently reconnect on its
    /// own (from either side) in the meantime.
    pub fn disconnect_peer(&self, peer_id: String) {
        let _ = self.commands.send(EngineCommand::DisconnectPeer { peer_crypto_id: peer_id });
    }

    /// Re-dials a peer previously dropped with `disconnect_peer`. Emits
    /// `FfiSyncEvent::ReconnectFailed` (not an error return — this is
    /// fire-and-forget like every other command) if the engine has no
    /// cached address for that peer, e.g. it hasn't been seen on the
    /// network since this engine started.
    pub fn reconnect_peer(&self, peer_id: String) {
        let _ = self.commands.send(EngineCommand::ReconnectPeer { peer_crypto_id: peer_id });
    }

    /// Remote-controls a transport command on `peer_id`'s currently-playing
    /// media. Fire-and-forget — only acted on if the receiving peer is
    /// macOS (the only platform with a real `MediaController` today);
    /// every other platform silently ignores it.
    pub fn send_media_command(&self, peer_id: String, command: FfiMediaCommand) {
        let _ = self.commands.send(EngineCommand::SendMediaCommand {
            peer_crypto_id: peer_id,
            command: command.into(),
        });
    }

    /// Re-queries mDNS right now for a manual "Refresh" action, instead of
    /// waiting for the background querier's own timer — for when a device
    /// that should be nearby isn't showing up yet.
    pub fn refresh_discovery(&self) {
        let _ = self.commands.send(EngineCommand::RefreshDiscovery);
    }
}

/// Generates a fresh identity's PKCS8 DER bytes. Call once on first run;
/// the host app persists the result (Android Keystore-backed storage, iOS
/// Keychain) and passes it back into `ContinuityEngine::start` and
/// `device_id_for` on every subsequent run — losing these bytes means
/// losing the device's identity and every pairing made with it.
#[uniffi::export]
pub fn generate_identity_der() -> Result<Vec<u8>, FfiError> {
    Identity::generate()
        .to_pkcs8_der()
        .map_err(|e| FfiError::Internal(e.to_string()))
}

/// Hex-encoded device id for the given identity bytes — lets a host app
/// show "this device" without starting the full engine.
#[uniffi::export]
pub fn device_id_for(identity_der: Vec<u8>) -> Result<String, FfiError> {
    Identity::from_pkcs8_der(&identity_der)
        .map(|identity| identity.device_id())
        .map_err(|e| FfiError::Internal(e.to_string()))
}

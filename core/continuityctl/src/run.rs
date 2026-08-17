//! Thin CLI shell over `continuity-daemon`'s engine: print events, read
//! stdin for pairing confirmation and a `send <path>` test command. The
//! real UI lives in `continuityd` (Phase 1) — this exists to exercise the
//! protocol core end-to-end without needing a platform UI.

use continuity_crypto::{Identity, TrustStore};
use continuity_daemon::{ArboardClipboard, EngineCommand, EngineConfig, SyncEvent};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub async fn run(profile: &str, name: Option<String>) -> anyhow::Result<()> {
    let identity = Identity::load_or_create(profile)?;
    let device_name = name.unwrap_or_else(continuity_daemon::default_device_name);
    let trust_store = TrustStore::load_default(profile)?;

    println!("device id:   {}", identity.device_id());
    println!("device name: {}", device_name);
    println!("received files will be saved to: {}", received_files_dir(profile).display());
    println!(
        "commands: y/n confirms a pending pairing request; 'connect <id>' dials a nearby (not yet paired) device; \
         'refresh' re-queries mDNS right now; 'send <path>' sends a file to the most recently connected peer; \
         'revoke <id>' forgets a paired device and closes its connection"
    );

    let config = EngineConfig {
        identity,
        device_name,
        trust_store,
        clipboard: Arc::new(ArboardClipboard),
        media: Arc::new(continuity_daemon::NoopMediaController),
        received_files_dir: received_files_dir(profile),
    };

    let mut engine = continuity_daemon::start(config).await?;
    let commands = engine.command_sender();

    let cli_state = Arc::new(CliState::default());
    spawn_stdin_reader(commands, cli_state.clone());

    loop {
        tokio::select! {
            event = engine.events.recv() => {
                match event {
                    Some(event) => handle_event(event, &cli_state),
                    None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("shutting down");
                break;
            }
        }
    }

    engine.shutdown();
    Ok(())
}

#[derive(Default)]
struct CliState {
    /// Peer awaiting a y/n answer to a pairing confirmation code.
    pending_pairing: Mutex<Option<String>>,
    /// Target for the `send <path>` test command — whichever peer we most
    /// recently connected to.
    last_connected_peer: Mutex<Option<String>>,
}

fn handle_event(event: SyncEvent, cli_state: &CliState) {
    match event {
        SyncEvent::Listening { port } => println!("listening on port {port}, waiting for peers..."),
        SyncEvent::PairingRequested { peer, code } => {
            *cli_state.pending_pairing.lock().unwrap() = Some(peer.id.clone());
            println!(
                "\npairing request from '{}' ({})\nconfirmation code: {code}\ndoes this match the code shown on the other device? [y/N] ",
                peer.name, peer.id
            );
        }
        SyncEvent::Paired { peer } => println!("paired with '{}'", peer.name),
        SyncEvent::PairingDeclined { peer_name } => {
            println!("pairing with '{peer_name}' did not complete (declined, or codes didn't match)");
        }
        SyncEvent::Connected { peer } => {
            *cli_state.last_connected_peer.lock().unwrap() = Some(peer.id.clone());
            println!("connected: '{}' ({})", peer.name, peer.id);
        }
        SyncEvent::Disconnected { peer_name, .. } => println!("disconnected: '{peer_name}'"),
        SyncEvent::ClipboardReceived { from_name } => println!("clipboard synced from '{from_name}'"),
        SyncEvent::ClipboardBroadcast { peer_count } => {
            println!("clipboard changed locally, synced to {peer_count} peer(s)");
        }
        SyncEvent::FileReceiving { from_name, file_name, size_bytes, .. } => {
            println!("receiving '{file_name}' ({size_bytes} bytes) from '{from_name}'...");
        }
        SyncEvent::FileReceived { file_name, path, .. } => println!("received '{file_name}' -> {path}"),
        SyncEvent::FileSent { file_name, to_name, .. } => println!("sent '{file_name}' to '{to_name}'"),
        SyncEvent::FileTransferFailed { reason, .. } => println!("file transfer failed: {reason}"),
        SyncEvent::Error(e) => tracing::warn!("{e}"),
        SyncEvent::WasReset => println!("reset: all paired devices forgotten"),
        SyncEvent::PausedStateChanged { paused } => {
            println!("{}", if paused { "syncing paused" } else { "syncing resumed" });
        }
        SyncEvent::ReconnectFailed { peer_id } => {
            println!("couldn't reconnect to '{peer_id}': no known address");
        }
        SyncEvent::NowPlayingChanged { peer_name, info, .. } => {
            if info.is_playing {
                let title = info.title.as_deref().unwrap_or("?");
                let artist = info.artist.as_deref().unwrap_or("?");
                println!("'{peer_name}' now playing: {title} — {artist}");
            } else {
                println!("'{peer_name}' now playing: (nothing)");
            }
        }
        SyncEvent::PeerDiscovered { device } => {
            println!("nearby (not paired): '{}' ({})", device.name, device.id);
        }
        // Fires once per connection per ping interval (30s) — routine
        // health telemetry, not worth printing on every tick in a CLI
        // that otherwise logs real events.
        SyncEvent::PeerActivity { .. } => {}
        SyncEvent::WasRevoked { peer_name, .. } => println!("forgot '{peer_name}'"),
        SyncEvent::RevokedByPeer { peer_name, .. } => {
            println!("'{peer_name}' removed this device — pair again to reconnect");
        }
    }
}

fn spawn_stdin_reader(
    commands: tokio::sync::mpsc::UnboundedSender<EngineCommand>,
    cli_state: Arc<CliState>,
) {
    tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(peer_crypto_id) = line.strip_prefix("connect ") {
                let _ = commands.send(EngineCommand::ReconnectPeer {
                    peer_crypto_id: peer_crypto_id.trim().to_string(),
                });
                continue;
            }

            if line == "refresh" {
                let _ = commands.send(EngineCommand::RefreshDiscovery);
                continue;
            }

            if let Some(peer_crypto_id) = line.strip_prefix("revoke ") {
                let _ = commands.send(EngineCommand::RevokeDevice {
                    peer_crypto_id: peer_crypto_id.trim().to_string(),
                });
                continue;
            }

            if let Some(path) = line.strip_prefix("send ") {
                match cli_state.last_connected_peer.lock().unwrap().clone() {
                    Some(peer_crypto_id) => {
                        let _ = commands.send(EngineCommand::SendFile {
                            peer_crypto_id,
                            path: path.trim().to_string(),
                        });
                    }
                    None => println!("no connected peer to send to yet"),
                }
                continue;
            }

            if let Some(peer_crypto_id) = cli_state.pending_pairing.lock().unwrap().take() {
                let accept = matches!(line.to_lowercase().as_str(), "y" | "yes");
                let _ = commands.send(EngineCommand::ConfirmPairing { peer_crypto_id, accept });
            }
        }
    });
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

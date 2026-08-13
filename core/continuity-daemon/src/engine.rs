use crate::clipboard::ClipboardBackend;
use crate::events::{EngineCommand, SyncEvent};
use crate::media::MediaController;
use continuity_crypto::{
    content_hash, generate_self_signed, Identity, IncrementalHash, TlsIdentity, TrustStore,
    TrustedDevice,
};
use continuity_net::{
    announce_and_identify, connect, peer_from_service_info, read_message, start_pairing,
    write_message, Connection, Discovery, Listener, ServiceEvent,
};
use continuity_proto::{DeviceInfo, Message, NowPlayingInfo, Platform, PROTOCOL_VERSION};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const CHUNK_SIZE: usize = 64 * 1024;
const FILE_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// Nothing else in this connection's message loop ever sends anything
/// unprompted except on real activity (clipboard changes, file transfers,
/// now-playing changes) — a quiet-but-healthy connection can go long
/// stretches with no traffic at all. Without an active ping, a connection
/// that goes silently dead (wifi drop, sleep/wake, a NAT/router dropping
/// idle mappings) has no way to be detected: nothing here sets a TCP
/// keepalive, so `read_message` just blocks forever, `connected` never
/// clears the peer, and the mDNS tie-break logic that would otherwise
/// redial on rediscovery sees "already connected" and skips it — the
/// reported "device doesn't reconnect until I restart the app" symptom.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// **Tuned up once already** — this started at 50s, which turned out far
/// too tight for real WiFi: normal jitter (roaming between APs, a laptop
/// waking from a brief sleep, a router's own hiccups) can plausibly stall
/// all traffic for the better part of a minute on its own, and TCP is
/// already resilient to exactly that — it just retransmits and carries on
/// once packets flow again, with the application never needing to know
/// anything happened. A 50s application-level timeout was *more*
/// aggressive than TCP's own recovery, actively tearing down connections
/// TCP would have quietly kept alive — reported as "connections are a lot
/// more unstable, randomly disconnecting" after this was first added.
/// Three minutes gives real transient conditions plenty of room to
/// resolve themselves before this steps in, while still being enormously
/// better than the pre-keepalive "never" for a truly dead connection
/// (wifi off, device asleep, out of range).
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(180);
/// Only a trusted (paired) peer can send a file at all, but this still
/// bounds how much an unattended auto-accept can write to disk.
const MAX_AUTO_ACCEPT_BYTES: u64 = 500 * 1024 * 1024;

pub struct EngineConfig {
    pub identity: Identity,
    pub device_name: String,
    pub trust_store: TrustStore,
    pub clipboard: Arc<dyn ClipboardBackend>,
    pub media: Arc<dyn MediaController>,
    pub received_files_dir: PathBuf,
}

/// A running engine instance. `events` streams everything the shell might
/// want to show the user; `send_command` is how the shell talks back
/// (confirming a pairing code, kicking off a file send).
pub struct EngineHandle {
    pub events: mpsc::UnboundedReceiver<SyncEvent>,
    commands_tx: mpsc::UnboundedSender<EngineCommand>,
    tasks: Vec<JoinHandle<()>>,
    discovery: Discovery,
    state: Arc<SharedState>,
}

impl EngineHandle {
    pub fn send_command(&self, cmd: EngineCommand) {
        let _ = self.commands_tx.send(cmd);
    }

    /// A cheap-to-clone handle for issuing commands from a task that
    /// doesn't otherwise hold the `EngineHandle` (e.g. a background stdin
    /// reader or a UI callback).
    pub fn command_sender(&self) -> mpsc::UnboundedSender<EngineCommand> {
        self.commands_tx.clone()
    }

    pub fn shutdown(self) {
        for task in &self.tasks {
            task.abort();
        }
        // `self.tasks` only holds the top-level background loops (mDNS
        // browse, inbound-accept, command processing, the watchers) — each
        // individual peer connection's task lives in its own place
        // (`state.connection_handles`, keyed by peer id, the same map
        // `Reset` aborts everything in) and was never in `tasks` at all.
        // Without this, `shutdown()` left every active connection's task
        // running untouched — harmless if the whole process exits right
        // after, but a real bug for any caller that expects `shutdown()`
        // to actually tear the engine down while the process keeps living.
        for handle in self.state.connection_handles.lock().unwrap().values() {
            handle.abort();
        }
        let _ = self.discovery.shutdown();
    }
}

struct SharedState {
    my_device: DeviceInfo,
    tls_identity: TlsIdentity,
    trust_store: Mutex<TrustStore>,
    /// Cryptographic peer ids with an active connection — the authoritative
    /// dedup point (see `continuity-net`'s pairing docs for why this can't
    /// just be the pre-handshake mDNS-advertised id).
    connected: Mutex<HashSet<String>>,
    /// Abort handles for each active connection's task, keyed by peer
    /// crypto id — the only way to forcibly drop a connection from outside
    /// its own task, since it's normally just blocked reading. Populated
    /// at spawn time (both inbound accept and outbound dial), removed in
    /// `handle_connection`'s cleanup. `Reset` aborts everything in here.
    connection_handles: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// Peers the user explicitly disconnected via `EngineCommand::DisconnectPeer`
    /// — checked by the accept loop (reject their inbound reconnect attempts
    /// too, not just our own outbound dialing) and the mDNS dial loop, so a
    /// manual disconnect actually sticks instead of the mesh immediately
    /// healing itself back to connected. Cleared by `ReconnectPeer`.
    manually_disconnected: Mutex<HashSet<String>>,
    /// Last-known address for any peer seen via mDNS resolution or an
    /// inbound connection, trusted or not — `ReconnectPeer` needs somewhere
    /// to dial, since the trust store itself only has id/name, no address.
    known_addresses: Mutex<HashMap<String, SocketAddr>>,
    peer_senders: Mutex<HashMap<String, mpsc::UnboundedSender<Message>>>,
    last_programmatic_hash: Mutex<Option<String>>,
    pending_pairings: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    pending_file_accepts: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    clipboard: Arc<dyn ClipboardBackend>,
    media: Arc<dyn MediaController>,
    events_tx: mpsc::UnboundedSender<SyncEvent>,
    received_files_dir: PathBuf,
    /// See `EngineCommand::SetPaused`. Checked by the accept loop, the
    /// dial loop, the clipboard watcher, and inbound clipboard-update
    /// handling — everywhere new syncing activity could start.
    paused: AtomicBool,
}

impl SharedState {
    fn emit(&self, event: SyncEvent) {
        let _ = self.events_tx.send(event);
    }
}

pub async fn start(config: EngineConfig) -> anyhow::Result<EngineHandle> {
    let tls_identity = generate_self_signed(&config.identity)?;
    let my_device = DeviceInfo {
        id: config.identity.device_id(),
        name: config.device_name,
        platform: detect_platform(),
        protocol_version: PROTOCOL_VERSION,
    };

    std::fs::create_dir_all(&config.received_files_dir)?;

    let listener = Listener::bind("0.0.0.0:0".parse()?, &tls_identity).await?;
    let port = listener.local_addr()?.port();

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();

    let discovery = Discovery::new()?;
    discovery.advertise(&my_device, port)?;
    let browse_rx = discovery.browse()?;

    let state = Arc::new(SharedState {
        my_device: my_device.clone(),
        tls_identity,
        trust_store: Mutex::new(config.trust_store),
        connected: Mutex::new(HashSet::new()),
        connection_handles: Mutex::new(HashMap::new()),
        manually_disconnected: Mutex::new(HashSet::new()),
        known_addresses: Mutex::new(HashMap::new()),
        peer_senders: Mutex::new(HashMap::new()),
        last_programmatic_hash: Mutex::new(None),
        pending_pairings: Mutex::new(HashMap::new()),
        pending_file_accepts: Mutex::new(HashMap::new()),
        clipboard: config.clipboard,
        media: config.media,
        events_tx,
        received_files_dir: config.received_files_dir,
        paused: AtomicBool::new(false),
    });

    state.emit(SyncEvent::Listening { port });

    let mut tasks = Vec::new();

    tasks.push({
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((conn, addr)) => {
                        if state.paused.load(Ordering::Relaxed) {
                            tracing::debug!("dropping inbound connection from {addr}: paused");
                            continue;
                        }
                        let peer_crypto_id = match conn.peer_device_id() {
                            Ok(id) => id,
                            Err(e) => {
                                tracing::debug!("couldn't read peer id for inbound conn from {addr}: {e}");
                                continue;
                            }
                        };
                        state.known_addresses.lock().unwrap().insert(peer_crypto_id.clone(), addr);
                        if state.manually_disconnected.lock().unwrap().contains(&peer_crypto_id) {
                            tracing::debug!("rejecting inbound connection from {addr}: manually disconnected");
                            continue;
                        }
                        let conn_state = state.clone();
                        let id_for_registry = peer_crypto_id.clone();
                        let join = tokio::spawn(async move {
                            if let Err(e) = handle_connection(conn, conn_state, peer_crypto_id).await {
                                tracing::debug!("inbound connection from {addr} ended: {e}");
                            }
                        });
                        state
                            .connection_handles
                            .lock()
                            .unwrap()
                            .insert(id_for_registry, join.abort_handle());
                    }
                    Err(e) => tracing::warn!("accept error: {e}"),
                }
            }
        })
    });

    tasks.push({
        let state = state.clone();
        tokio::spawn(async move {
            while let Ok(event) = browse_rx.recv_async().await {
                tracing::debug!("mdns event: {event:?}");
                let ServiceEvent::ServiceResolved(info) = event else {
                    continue;
                };
                let Some(peer) = peer_from_service_info(&info) else {
                    continue;
                };
                if peer.device.id == state.my_device.id {
                    continue;
                }
                // Cache the address regardless of whether this side ends up
                // dialing — `ReconnectPeer` needs it, and the tie-break
                // below means this side often won't be the one connecting.
                // mdns-sd delivers `ServiceResolved` incrementally (each
                // event can carry a different subset of the peer's
                // addresses as they resolve), so a later event can easily
                // have only link-local IPv6 addresses where an earlier one
                // had the real LAN IPv4 — don't let that downgrade an
                // already-good cached address, or a `ReconnectPeer` sent
                // right after such an event dials an unroutable address
                // and fails silently ("no route to host"), which reads to
                // the user as clicking Connect just doing nothing.
                {
                    let mut known = state.known_addresses.lock().unwrap();
                    let already_have_ipv4 = known.get(&peer.device.id).is_some_and(|a| a.is_ipv4());
                    if !already_have_ipv4 || peer.addr.is_ipv4() {
                        known.insert(peer.device.id.clone(), peer.addr);
                    }
                }

                let is_trusted = state.trust_store.lock().unwrap().is_trusted(&peer.device.id);
                if !is_trusted {
                    // Don't auto-dial (and therefore don't auto-initiate
                    // pairing with) a device just because it's on the same
                    // network — that meant a pairing prompt for literally
                    // every other Continuity install anyone ever joined a
                    // LAN with. Surface it as "nearby, tap to connect"
                    // instead; the actual dial happens via `ReconnectPeer`
                    // once the user picks it. No tie-break needed here
                    // either — unlike auto-reconnect, there's no duplicate-
                    // connection race to avoid when nothing is connecting.
                    state.emit(SyncEvent::PeerDiscovered { device: peer.device.clone() });
                    continue;
                }

                // Tie-break so only one side dials: without this, both
                // devices would see each other over mDNS at roughly the
                // same time and race to open duplicate connections.
                if peer.device.id >= state.my_device.id {
                    continue;
                }
                if state.paused.load(Ordering::Relaxed) {
                    continue;
                }
                if state.connected.lock().unwrap().contains(&peer.device.id) {
                    continue;
                }
                if state.manually_disconnected.lock().unwrap().contains(&peer.device.id) {
                    continue;
                }

                let conn_state = state.clone();
                let peer_id = peer.device.id.clone();
                let id_for_registry = peer_id.clone();
                let peer_addr = peer.addr;
                let join = tokio::spawn(async move {
                    match connect(peer_addr, &conn_state.tls_identity).await {
                        Ok(conn) => {
                            if let Err(e) = handle_connection(conn, conn_state, peer_id).await {
                                tracing::debug!("outbound connection to {peer_addr} ended: {e}");
                            }
                        }
                        Err(e) => tracing::debug!("failed to dial {peer_addr}: {e}"),
                    }
                });
                state
                    .connection_handles
                    .lock()
                    .unwrap()
                    .insert(id_for_registry, join.abort_handle());
            }
        })
    });

    tasks.push({
        let state = state.clone();
        tokio::spawn(async move {
            while let Some(cmd) = commands_rx.recv().await {
                match cmd {
                    EngineCommand::ConfirmPairing { peer_crypto_id, accept } => {
                        if let Some(tx) = state.pending_pairings.lock().unwrap().remove(&peer_crypto_id) {
                            let _ = tx.send(accept);
                        }
                    }
                    EngineCommand::SendFile { peer_crypto_id, path } => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            send_file(&state, &peer_crypto_id, PathBuf::from(path)).await;
                        });
                    }
                    EngineCommand::Reset => {
                        let handles: Vec<_> = state
                            .connection_handles
                            .lock()
                            .unwrap()
                            .drain()
                            .map(|(_, h)| h)
                            .collect();
                        for handle in handles {
                            handle.abort();
                        }
                        state.connected.lock().unwrap().clear();
                        state.peer_senders.lock().unwrap().clear();
                        if let Err(e) = state.trust_store.lock().unwrap().clear() {
                            tracing::warn!("failed to clear trust store during reset: {e}");
                        }
                        state.emit(SyncEvent::WasReset);
                    }
                    EngineCommand::SetPaused(paused) => {
                        state.paused.store(paused, Ordering::Relaxed);
                        state.emit(SyncEvent::PausedStateChanged { paused });
                    }
                    EngineCommand::DisconnectPeer { peer_crypto_id } => {
                        let peer_name = state
                            .trust_store
                            .lock()
                            .unwrap()
                            .get(&peer_crypto_id)
                            .map(|d| d.name.clone())
                            .unwrap_or_else(|| peer_crypto_id.clone());

                        state.manually_disconnected.lock().unwrap().insert(peer_crypto_id.clone());
                        if let Some(handle) = state.connection_handles.lock().unwrap().remove(&peer_crypto_id) {
                            handle.abort();
                        }
                        state.connected.lock().unwrap().remove(&peer_crypto_id);
                        state.peer_senders.lock().unwrap().remove(&peer_crypto_id);

                        // Aborting the connection task skips its own normal
                        // exit-path `Disconnected` emit (abort cancels
                        // immediately, no more of that future's code runs),
                        // so it has to happen here instead.
                        state.emit(SyncEvent::Disconnected {
                            peer_id: peer_crypto_id,
                            peer_name,
                        });
                    }
                    EngineCommand::ReconnectPeer { peer_crypto_id } => {
                        state.manually_disconnected.lock().unwrap().remove(&peer_crypto_id);
                        let Some(addr) = state.known_addresses.lock().unwrap().get(&peer_crypto_id).copied() else {
                            state.emit(SyncEvent::ReconnectFailed { peer_id: peer_crypto_id });
                            continue;
                        };
                        if state.connected.lock().unwrap().contains(&peer_crypto_id) {
                            continue;
                        }

                        let conn_state = state.clone();
                        let id_for_registry = peer_crypto_id.clone();
                        let join = tokio::spawn(async move {
                            match connect(addr, &conn_state.tls_identity).await {
                                Ok(conn) => {
                                    if let Err(e) = handle_connection(conn, conn_state, peer_crypto_id).await {
                                        tracing::debug!("reconnect to {addr} ended: {e}");
                                    }
                                }
                                Err(e) => tracing::debug!("failed to reconnect to {addr}: {e}"),
                            }
                        });
                        state.connection_handles.lock().unwrap().insert(id_for_registry, join.abort_handle());
                    }
                    EngineCommand::SendMediaCommand { peer_crypto_id, command } => {
                        if let Some(tx) = state.peer_senders.lock().unwrap().get(&peer_crypto_id) {
                            let _ = tx.send(Message::MediaCommand { command });
                        }
                    }
                    EngineCommand::RefreshDiscovery => {
                        // *Not* an mDNS re-query — `mdns_sd::ServiceDaemon::
                        // browse()` unconditionally overwrites its single
                        // registered listener for a service type ("if there
                        // is already a listener, it will be updated, i.e.
                        // overwritten" — straight from its own doc comment),
                        // so calling it a second time silently orphans the
                        // receiver this engine's whole browse loop actually
                        // reads from: real bug, shipped and caught after the
                        // fact — it killed all further discovery (new
                        // devices *and* rediscovery-triggered reconnects)
                        // after the first refresh, whether from this timer
                        // or the user's own Refresh click. mdns-sd already
                        // re-queries on its own via retransmission using the
                        // *original* listener, so nothing external is needed
                        // for that half. What's actually useful here instead:
                        // retry dialing every trusted peer with a cached
                        // address that isn't currently connected — doesn't
                        // touch mDNS at all, so it can't repeat that mistake.
                        let targets: Vec<(String, SocketAddr)> = {
                            let trust_store = state.trust_store.lock().unwrap();
                            let connected = state.connected.lock().unwrap();
                            let manually_disconnected = state.manually_disconnected.lock().unwrap();
                            let known_addresses = state.known_addresses.lock().unwrap();
                            trust_store
                                .list()
                                .filter(|d| !connected.contains(&d.id) && !manually_disconnected.contains(&d.id))
                                .filter_map(|d| known_addresses.get(&d.id).copied().map(|addr| (d.id.clone(), addr)))
                                .collect()
                        };
                        for (peer_crypto_id, addr) in targets {
                            let conn_state = state.clone();
                            let id_for_registry = peer_crypto_id.clone();
                            let join = tokio::spawn(async move {
                                match connect(addr, &conn_state.tls_identity).await {
                                    Ok(conn) => {
                                        if let Err(e) = handle_connection(conn, conn_state, peer_crypto_id).await {
                                            tracing::debug!("refresh-triggered reconnect to {addr} ended: {e}");
                                        }
                                    }
                                    Err(e) => tracing::debug!("refresh-triggered reconnect to {addr} failed: {e}"),
                                }
                            });
                            state.connection_handles.lock().unwrap().insert(id_for_registry, join.abort_handle());
                        }
                    }
                }
            }
        })
    });

    tasks.push(spawn_clipboard_watcher(state.clone()));
    tasks.push(spawn_now_playing_watcher(state.clone()));

    Ok(EngineHandle {
        events: events_rx,
        commands_tx,
        tasks,
        discovery,
        state,
    })
}

async fn handle_connection(
    conn: Connection,
    state: Arc<SharedState>,
    peer_crypto_id: String,
) -> anyhow::Result<()> {
    {
        let mut connected = state.connected.lock().unwrap();
        if connected.contains(&peer_crypto_id) {
            anyhow::bail!("already connected to {peer_crypto_id}");
        }
        connected.insert(peer_crypto_id.clone());
    }

    let result = handle_connection_inner(conn, &state, &peer_crypto_id).await;

    state.connected.lock().unwrap().remove(&peer_crypto_id);
    state.peer_senders.lock().unwrap().remove(&peer_crypto_id);
    // Not removed on the `Reset` path — that drains the whole map itself
    // and aborts every handle, so there's nothing left here to remove by
    // the time this cleanup runs (aborting a task doesn't let it finish
    // this line).
    state.connection_handles.lock().unwrap().remove(&peer_crypto_id);
    result
}

async fn handle_connection_inner(
    mut conn: Connection,
    state: &Arc<SharedState>,
    peer_crypto_id: &str,
) -> anyhow::Result<()> {
    let is_trusted = state.trust_store.lock().unwrap().is_trusted(peer_crypto_id);

    tracing::debug!("handle_connection_inner: peer={peer_crypto_id} is_trusted={is_trusted}");

    let peer = if is_trusted {
        announce_and_identify(&mut conn, &state.my_device).await?
    } else {
        tracing::debug!("calling start_pairing for {peer_crypto_id}");
        let pending = start_pairing(conn, &state.my_device).await?;
        tracing::debug!("start_pairing returned, code={}", pending.code);
        let peer_name = pending.peer.name.clone();

        let (tx, rx) = oneshot::channel();
        state
            .pending_pairings
            .lock()
            .unwrap()
            .insert(peer_crypto_id.to_string(), tx);
        state.emit(SyncEvent::PairingRequested {
            peer: pending.peer.clone(),
            code: pending.code.clone(),
        });
        tracing::debug!("PairingRequested emitted, awaiting local confirmation");
        let accepted = rx.await.unwrap_or(false);
        tracing::debug!("local confirmation resolved: accepted={accepted}");

        match pending.confirm(accepted).await? {
            Some((c, peer)) => {
                conn = c;
                state.trust_store.lock().unwrap().trust(TrustedDevice {
                    id: peer.id.clone(),
                    name: peer.name.clone(),
                    paired_at_unix: now_unix(),
                })?;
                state.emit(SyncEvent::Paired { peer: peer.clone() });
                peer
            }
            None => {
                state.emit(SyncEvent::PairingDeclined { peer_name });
                return Ok(());
            }
        }
    };

    state.emit(SyncEvent::Connected { peer: peer.clone() });

    let (mut read_half, mut write_half) = tokio::io::split(conn);
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    state
        .peer_senders
        .lock()
        .unwrap()
        .insert(peer_crypto_id.to_string(), tx.clone());

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_message(&mut write_half, &msg).await.is_err() {
                break;
            }
        }
    });

    let mut receiving: HashMap<String, ReceivingFile> = HashMap::new();

    let mut ping_ticker = tokio::time::interval(PING_INTERVAL);
    ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Tracks time since the last *received* message (any message, not just
    // Pong — anything arriving proves the connection is alive). Deliberately
    // NOT a `timeout()` wrapped fresh around `read_message` each iteration:
    // since PING_INTERVAL < CONNECTION_READ_TIMEOUT, the ping branch would
    // win the select every cycle and recreate that timeout from scratch
    // before it could ever elapse, so a truly dead connection would never
    // be detected. Checking elapsed-since-`last_activity` on each ping tick
    // instead means sending a ping can't reset a clock it doesn't own.
    let mut last_activity = std::time::Instant::now();

    loop {
        let read_result = tokio::select! {
            _ = ping_ticker.tick() => {
                if last_activity.elapsed() > CONNECTION_READ_TIMEOUT {
                    tracing::debug!(
                        "no message from '{}' in over {CONNECTION_READ_TIMEOUT:?} — treating as disconnected",
                        peer.name
                    );
                    break;
                }
                if tx.send(Message::Ping).is_err() {
                    break; // writer task is gone — connection is already dead
                }
                continue;
            }
            result = read_message(&mut read_half) => result,
        };
        last_activity = std::time::Instant::now();
        match read_result {
            Ok(Message::Ping) => {
                let _ = tx.send(Message::Pong);
            }
            Ok(Message::Pong) => {}
            Ok(Message::MediaCommand { command }) => {
                state.media.handle(command);
            }
            Ok(Message::NowPlayingUpdate { info }) => {
                if state.paused.load(Ordering::Relaxed) {
                    continue;
                }
                state.emit(SyncEvent::NowPlayingChanged {
                    peer_id: peer.id.clone(),
                    peer_name: peer.name.clone(),
                    info,
                });
            }
            Ok(Message::ClipboardUpdate {
                origin_device,
                content_hash: hash,
                mime,
                data,
            }) => {
                if state.paused.load(Ordering::Relaxed) {
                    tracing::debug!("ignoring clipboard update from {origin_device}: paused");
                    continue;
                }
                if mime != "text/plain" {
                    tracing::debug!("ignoring non-text clipboard update from {origin_device}");
                    continue;
                }
                if content_hash(&data) != hash {
                    tracing::warn!("clipboard update from {origin_device} failed its integrity check, ignoring");
                    continue;
                }
                apply_remote_clipboard(state, hash, data).await;
                state.emit(SyncEvent::ClipboardReceived {
                    from_name: peer.name.clone(),
                });
            }
            Ok(Message::FileOffer {
                transfer_id,
                origin_device: _,
                file_name,
                size_bytes,
                mime: _,
            }) => {
                handle_file_offer(state, &tx, &peer, &mut receiving, transfer_id, file_name, size_bytes).await;
            }
            Ok(Message::FileAccept { transfer_id, accepted }) => {
                if let Some(waiter) = state.pending_file_accepts.lock().unwrap().remove(&transfer_id) {
                    let _ = waiter.send(accepted);
                }
            }
            Ok(Message::FileChunk { transfer_id, seq: _, data }) => {
                handle_file_chunk(state, &mut receiving, &transfer_id, data).await;
            }
            Ok(Message::FileComplete { transfer_id, content_hash: expected_hash }) => {
                handle_file_complete(state, &mut receiving, &transfer_id, expected_hash).await;
            }
            Ok(other) => tracing::debug!("ignoring unhandled message from '{}': {other:?}", peer.name),
            Err(_) => break,
        }
    }

    writer_task.abort();
    state.emit(SyncEvent::Disconnected {
        peer_id: peer.id.clone(),
        peer_name: peer.name.clone(),
    });
    Ok(())
}

struct ReceivingFile {
    file: tokio::fs::File,
    hasher: IncrementalHash,
    path: PathBuf,
    file_name: String,
    written: u64,
    expected_size: u64,
}

async fn handle_file_offer(
    state: &Arc<SharedState>,
    tx: &mpsc::UnboundedSender<Message>,
    peer: &DeviceInfo,
    receiving: &mut HashMap<String, ReceivingFile>,
    transfer_id: String,
    file_name: String,
    size_bytes: u64,
) {
    if size_bytes > MAX_AUTO_ACCEPT_BYTES {
        tracing::warn!(
            "rejecting file '{file_name}' from '{}': {size_bytes} bytes exceeds the auto-accept limit",
            peer.name
        );
        let _ = tx.send(Message::FileAccept {
            transfer_id,
            accepted: false,
        });
        return;
    }

    let path = unique_destination_path(&state.received_files_dir, &file_name);
    let file = match tokio::fs::File::create(&path).await {
        Ok(f) => f,
        Err(e) => {
            state.emit(SyncEvent::FileTransferFailed {
                transfer_id: transfer_id.clone(),
                reason: format!("couldn't create destination file: {e}"),
            });
            let _ = tx.send(Message::FileAccept {
                transfer_id,
                accepted: false,
            });
            return;
        }
    };

    state.emit(SyncEvent::FileReceiving {
        transfer_id: transfer_id.clone(),
        from_name: peer.name.clone(),
        file_name: file_name.clone(),
        size_bytes,
    });

    receiving.insert(
        transfer_id.clone(),
        ReceivingFile {
            file,
            hasher: IncrementalHash::new(),
            path,
            file_name,
            written: 0,
            expected_size: size_bytes,
        },
    );

    let _ = tx.send(Message::FileAccept {
        transfer_id,
        accepted: true,
    });
}

async fn handle_file_chunk(
    state: &Arc<SharedState>,
    receiving: &mut HashMap<String, ReceivingFile>,
    transfer_id: &str,
    data: Vec<u8>,
) {
    let Some(rf) = receiving.get_mut(transfer_id) else {
        tracing::debug!("chunk for unknown/already-finished transfer {transfer_id}, ignoring");
        return;
    };
    if rf.file.write_all(&data).await.is_err() {
        state.emit(SyncEvent::FileTransferFailed {
            transfer_id: transfer_id.to_string(),
            reason: "disk write error".into(),
        });
        receiving.remove(transfer_id);
        return;
    }
    rf.hasher.update(&data);
    rf.written += data.len() as u64;
}

async fn handle_file_complete(
    state: &Arc<SharedState>,
    receiving: &mut HashMap<String, ReceivingFile>,
    transfer_id: &str,
    expected_hash: String,
) {
    let Some(rf) = receiving.remove(transfer_id) else {
        return;
    };
    let actual_hash = rf.hasher.finalize_hex();
    if actual_hash != expected_hash || rf.written != rf.expected_size {
        state.emit(SyncEvent::FileTransferFailed {
            transfer_id: transfer_id.to_string(),
            reason: "integrity check failed after transfer".into(),
        });
        let _ = tokio::fs::remove_file(&rf.path).await;
        return;
    }
    state.emit(SyncEvent::FileReceived {
        transfer_id: transfer_id.to_string(),
        file_name: rf.file_name,
        path: rf.path.display().to_string(),
    });
}

fn unique_destination_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name);
    let ext = Path::new(file_name).extension().and_then(|s| s.to_str());
    for i in 1..1000 {
        let name = match ext {
            Some(ext) => format!("{stem} ({i}).{ext}"),
            None => format!("{stem} ({i})"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(file_name)
}

async fn send_file(state: &Arc<SharedState>, peer_crypto_id: &str, path: PathBuf) {
    let transfer_id = format!("{:016x}", rand::random::<u64>());
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();

    let data = match tokio::fs::read(&path).await {
        Ok(d) => d,
        Err(e) => {
            state.emit(SyncEvent::FileTransferFailed {
                transfer_id,
                reason: format!("couldn't read '{}': {e}", path.display()),
            });
            return;
        }
    };
    let size_bytes = data.len() as u64;
    let hash = content_hash(&data);

    let Some(tx) = state
        .peer_senders
        .lock()
        .unwrap()
        .get(peer_crypto_id)
        .cloned()
    else {
        state.emit(SyncEvent::FileTransferFailed {
            transfer_id,
            reason: "peer is not currently connected".into(),
        });
        return;
    };

    let (accept_tx, accept_rx) = oneshot::channel();
    state
        .pending_file_accepts
        .lock()
        .unwrap()
        .insert(transfer_id.clone(), accept_tx);

    let _ = tx.send(Message::FileOffer {
        transfer_id: transfer_id.clone(),
        origin_device: state.my_device.id.clone(),
        file_name: file_name.clone(),
        size_bytes,
        mime: None,
    });

    let accepted = matches!(
        tokio::time::timeout(FILE_ACCEPT_TIMEOUT, accept_rx).await,
        Ok(Ok(true))
    );

    if !accepted {
        state.pending_file_accepts.lock().unwrap().remove(&transfer_id);
        state.emit(SyncEvent::FileTransferFailed {
            transfer_id,
            reason: "declined, or the peer didn't respond in time".into(),
        });
        return;
    }

    let peer_name = state
        .trust_store
        .lock()
        .unwrap()
        .get(peer_crypto_id)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| peer_crypto_id.to_string());

    for (seq, chunk) in data.chunks(CHUNK_SIZE).enumerate() {
        if tx
            .send(Message::FileChunk {
                transfer_id: transfer_id.clone(),
                seq: seq as u64,
                data: chunk.to_vec(),
            })
            .is_err()
        {
            state.emit(SyncEvent::FileTransferFailed {
                transfer_id,
                reason: "connection closed mid-transfer".into(),
            });
            return;
        }
    }

    let _ = tx.send(Message::FileComplete {
        transfer_id: transfer_id.clone(),
        content_hash: hash,
    });
    state.emit(SyncEvent::FileSent {
        transfer_id,
        file_name,
        to_name: peer_name,
    });
}

async fn apply_remote_clipboard(state: &Arc<SharedState>, hash: String, data: Vec<u8>) {
    let text = String::from_utf8_lossy(&data).to_string();
    let state = state.clone();
    let _ = tokio::task::spawn_blocking(move || {
        *state.last_programmatic_hash.lock().unwrap() = Some(hash);
        state.clipboard.set_text(&text);
    })
    .await;
}

fn spawn_clipboard_watcher(state: Arc<SharedState>) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let mut last_seen: Option<String> = None;
        loop {
            std::thread::sleep(Duration::from_millis(500));

            if state.paused.load(Ordering::Relaxed) {
                continue;
            }

            // `get_text` crosses the FFI boundary into Kotlin/Swift on
            // mobile (desktop's `arboard` backend doesn't need this, but
            // catch_unwind is cheap either way). A host-language exception
            // that isn't handled on its own side of the boundary surfaces
            // here as a Rust panic — without catching it, one bad read
            // (e.g. a transient clipboard-permission denial) would unwind
            // this whole spawn_blocking closure and permanently kill
            // clipboard sync for the rest of the process's life, since
            // nothing ever joins or restarts this task. Reopening the app
            // wouldn't help: the watcher is just gone.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                state.clipboard.get_text()
            }));
            let Ok(Some(text)) = result else {
                continue;
            };
            if text.is_empty() {
                continue;
            }

            let hash = content_hash(text.as_bytes());
            if last_seen.as_deref() == Some(hash.as_str()) {
                continue;
            }
            last_seen = Some(hash.clone());

            let is_echo_of_remote_write =
                state.last_programmatic_hash.lock().unwrap().as_deref() == Some(hash.as_str());
            if is_echo_of_remote_write {
                continue;
            }

            let msg = Message::ClipboardUpdate {
                origin_device: state.my_device.id.clone(),
                content_hash: hash,
                mime: "text/plain".to_string(),
                data: text.into_bytes(),
            };

            let senders: Vec<_> = state.peer_senders.lock().unwrap().values().cloned().collect();
            let peer_count = senders.len();
            for tx in senders {
                let _ = tx.send(msg.clone());
            }
            // Emit even when peer_count is 0 — a shell that surfaces this
            // (see MainActivity's activity feed) needs to be able to tell
            // "watcher saw the clipboard change but had no one to send it
            // to" apart from "watcher never saw a change at all". Those
            // look identical to a user if this only fires on a successful
            // send, which was the entire reason "did it even try" was
            // unanswerable from the Android UI.
            state.emit(SyncEvent::ClipboardBroadcast { peer_count });
        }
    })
}

fn spawn_now_playing_watcher(state: Arc<SharedState>) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let mut last_seen: Option<NowPlayingInfo> = None;
        loop {
            std::thread::sleep(Duration::from_millis(1500));

            if state.paused.load(Ordering::Relaxed) {
                continue;
            }

            // Same reasoning as the clipboard watcher's catch_unwind: this
            // crosses into host-language/private-framework code (macOS's
            // MediaRemote today), and an unhandled panic there would
            // otherwise permanently kill this watcher for the rest of the
            // process's life.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                state.media.now_playing()
            }));
            let Ok(info) = result else {
                continue;
            };

            if info == last_seen {
                continue;
            }
            last_seen = info.clone();

            // `None` (nothing playing / app quit) still gets broadcast, as
            // a default/empty snapshot — otherwise a peer's display would
            // keep showing whatever was playing last, forever, once
            // playback actually stops.
            let msg = Message::NowPlayingUpdate { info: info.unwrap_or_default() };
            let senders: Vec<_> = state.peer_senders.lock().unwrap().values().cloned().collect();
            for tx in senders {
                let _ = tx.send(msg.clone());
            }
        }
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn detect_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Linux
    }
}

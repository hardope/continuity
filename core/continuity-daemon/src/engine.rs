use crate::clipboard::ClipboardBackend;
use crate::events::{EngineCommand, RemoteControlRole, SyncEvent};
use crate::media::MediaController;
use crate::remote_control::RemoteControlHost;
use continuity_crypto::{
    content_hash, generate_self_signed, Identity, IncrementalHash, TlsIdentity, TrustStore,
    TrustedDevice,
};
use continuity_net::{
    announce_and_identify, connect, peer_from_service_info, read_frame, read_message,
    start_pairing, write_frame, write_message, Connection, Discovery, Listener, ServiceEvent,
};
use continuity_proto::{DeviceInfo, Message, NowPlayingInfo, Platform, PROTOCOL_VERSION};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, Notify};
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
    pub remote_control: Arc<dyn RemoteControlHost>,
    pub received_files_dir: PathBuf,
}

/// A running engine instance. `events` streams everything the shell might
/// want to show the user; `send_command` is how the shell talks back
/// (confirming a pairing code, kicking off a file send).
pub struct EngineHandle {
    pub events: mpsc::UnboundedReceiver<SyncEvent>,
    commands_tx: mpsc::UnboundedSender<EngineCommand>,
    tasks: Vec<JoinHandle<()>>,
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
        // Must happen before the aborts below: `task.abort()` can't
        // preempt the clipboard/now-playing watchers, since they run on
        // tokio's blocking thread pool and are already executing their
        // loop bodies on a dedicated OS thread by the time shutdown is
        // called — abort only stops a blocking task that hasn't started
        // yet. Setting this flag lets them notice and exit on their own
        // right after their next `std::thread::sleep` wakes up.
        self.state.shutting_down.store(true, Ordering::Relaxed);
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
        let _ = self.state.discovery.lock().unwrap().shutdown();
    }
}

#[derive(Clone)]
struct RemoteControlSession {
    session_id: String,
    role: RemoteControlRole,
    /// `false` for the controlling side between sending `RequestRemoteControl`
    /// and hearing back — `SendInputEvent` and the accept-loop screen-stream
    /// routing both only act once this flips to `true`, so an event fired
    /// in that brief window (or a session that gets declined) can't leak
    /// through. Always `true` for the controlled side, since that entry
    /// is only ever created *after* the local user has already accepted.
    active: bool,
}

struct SharedState {
    my_device: DeviceInfo,
    tls_identity: TlsIdentity,
    /// Behind a `Mutex` (not just owned by `EngineHandle` like it used to
    /// be) so the browse task can swap in a freshly recreated `Discovery`
    /// after the mDNS daemon dies — see the recovery loop around the
    /// browse task below. `shutdown()` reaches it through `state` now
    /// instead of `EngineHandle` holding its own copy.
    discovery: Mutex<Discovery>,
    /// Notified by the network-change watcher (and by the mDNS
    /// channel-death recovery path reusing the same signal) whenever the
    /// browse task should tear down and recreate its `Discovery` early
    /// instead of waiting for the channel to actually die. See the browse
    /// task below.
    discovery_recreate: Notify,
    trust_store: Mutex<TrustStore>,
    /// Cryptographic peer ids with an active connection — the authoritative
    /// dedup point (see `continuity-net`'s pairing docs for why this can't
    /// just be the pre-handshake mDNS-advertised id).
    connected: Mutex<HashSet<String>>,
    /// Peer ids with an outbound dial currently in flight (from `connect()`
    /// starting through `handle_connection` finishing, success or failure)
    /// — see `try_claim_dial`. **Real bug fixed here**: without this,
    /// nothing stopped several *redundant* outbound dials to the same peer
    /// from starting concurrently, since the only earlier guard
    /// (`!connected.contains(id)`) doesn't become true until a dial's TLS
    /// handshake actually finishes — a window easily wide enough for
    /// mdns-sd's `ServiceResolved` events (which arrive in rapid bursts,
    /// often a dozen-plus within single-digit milliseconds — one per
    /// resolved address/interface for the same peer) to each independently
    /// see "not connected yet" and spawn their own dial. Each side (the
    /// dialer and the accepter) then resolves which of the redundant
    /// attempts "wins" *completely independently*, with no correlation
    /// between the two — so it was entirely possible for this side to
    /// settle on attempt #1 while the peer settled on the inbound
    /// connection matching attempt #2, leaving both sides holding a
    /// connection object the other had already abandoned and closed. That
    /// read as an immediate, inexplicable disconnect with no relation to
    /// file transfers or the keepalive timeout — a "connections are
    /// randomly dropping" report even at total idle, right after pairing.
    dialing: Mutex<HashSet<String>>,
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
    remote_control: Arc<dyn RemoteControlHost>,
    /// One entry per peer with a remote-control session in flight or
    /// active — a peer can only ever be in *one* session (either role) at
    /// a time, so keying by `peer_crypto_id` (rather than `session_id`) is
    /// enough and keeps every lookup a single hash-map access instead of
    /// a scan.
    remote_control_sessions: Mutex<HashMap<String, RemoteControlSession>>,
    /// Inbound `RemoteControlRequest`s awaiting a local answer via
    /// `EngineCommand::RespondToRemoteControlRequest` — mirrors
    /// `pending_pairings`'s shape, but doesn't need that one's `oneshot`
    /// channel: answering a pairing code resumes a suspended `.await` in
    /// the middle of the handshake, while answering a remote-control
    /// request is just "look up the session id, send a reply" entirely
    /// within the command loop, nothing paused waiting on it.
    pending_remote_control_requests: Mutex<HashMap<String, String>>,
    /// Peer ids the accept loop should route to `handle_screen_stream_connection`
    /// instead of the normal pairing/mesh path for their *next* inbound
    /// connection — populated right when this side becomes the
    /// controlling party of a newly-accepted session (see
    /// `RemoteControlResponse { accepted: true }` handling), so the
    /// controlled peer's incoming screen-stream connection is recognized
    /// immediately by identity alone, no message-peeking or added
    /// latency on the far more common case of a normal reconnect.
    pending_screen_stream_peers: Mutex<HashSet<String>>,
    /// Abort handle for the screen-stream connection's task, keyed by
    /// peer id — the controlled side's frame-push loop, or the
    /// controlling side's frame-receive loop, whichever role this device
    /// is playing in the session. Aborted on `EndRemoteControlSession`
    /// and on the underlying mesh connection dropping, so a session never
    /// outlives the connection that authorized it.
    remote_control_stream_handles: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    events_tx: mpsc::UnboundedSender<SyncEvent>,
    received_files_dir: PathBuf,
    /// See `EngineCommand::SetPaused`. Checked by the accept loop, the
    /// dial loop, the clipboard watcher, and inbound clipboard-update
    /// handling — everywhere new syncing activity could start.
    paused: AtomicBool,
    /// **Real bug fixed here**: `EngineHandle::shutdown()`'s `task.abort()`
    /// calls are no-ops against `spawn_clipboard_watcher` and `spawn_
    /// now_playing_watcher` specifically — both run on tokio's blocking
    /// thread pool (`spawn_blocking`, needed since they call synchronous
    /// host/FFI code), and `abort()` only prevents a *queued* blocking
    /// task from starting; once one is actually running its closure on its
    /// own OS thread, cancellation can't preempt it — it just keeps
    /// looping forever, since neither watcher's `loop` had any exit
    /// condition. That thread staying alive is exactly what a `tokio::
    /// Runtime` waits on when it's dropped, so any caller relying on the
    /// process/runtime actually exiting after `shutdown()` (this crate's
    /// own integration tests included — that's how this was caught: a
    /// test hung well past its own internal deadline) would hang
    /// indefinitely. Checked by both watchers right after they wake from
    /// each sleep; set by `shutdown()` before it aborts anything else, so
    /// they notice and exit within one sleep interval instead of never.
    shutting_down: AtomicBool,
}

impl SharedState {
    fn emit(&self, event: SyncEvent) {
        let _ = self.events_tx.send(event);
    }
}

/// Tears down whatever's left of an active/pending remote-control session
/// with `peer_crypto_id` — stops local capture if this device was the
/// controlled side, aborts the screen-stream task, and emits
/// `SyncEvent::RemoteControlSessionEnded`. Idempotent: a no-op (no emit)
/// if there's no session for that peer at all, since every caller below
/// can race another — a connection dying and an explicit `EndRemoteControlSession`
/// arriving at nearly the same moment, say — and only the first should
/// actually report anything.
fn end_remote_control_session(state: &Arc<SharedState>, peer_crypto_id: &str, reason: Option<String>, notify_peer: bool) {
    let Some(session) = state.remote_control_sessions.lock().unwrap().remove(peer_crypto_id) else {
        return;
    };
    state.pending_screen_stream_peers.lock().unwrap().remove(peer_crypto_id);
    if let Some(handle) = state.remote_control_stream_handles.lock().unwrap().remove(peer_crypto_id) {
        handle.abort();
    }
    if session.role == RemoteControlRole::Controlled {
        state.remote_control.stop_capture();
    }
    if notify_peer {
        if let Some(tx) = state.peer_senders.lock().unwrap().get(peer_crypto_id) {
            let _ = tx.send(Message::RemoteControlEnded { session_id: session.session_id.clone() });
        }
    }
    let peer_name = state
        .trust_store
        .lock()
        .unwrap()
        .get(peer_crypto_id)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| peer_crypto_id.to_string());
    state.emit(SyncEvent::RemoteControlSessionEnded {
        peer_id: peer_crypto_id.to_string(),
        peer_name,
        session_id: session.session_id,
        reason,
    });
}

/// Handles the *separate*, dedicated connection opened for one session's
/// screen stream — never the normal mesh connection, and never carries
/// anything but this one session's frames. Skips the usual `announce_and_
/// identify` handshake entirely: identity here is already fully proven by
/// the TLS client certificate alone (`Connection::peer_device_id()`,
/// exactly what `announce_and_identify` itself cross-checks against), and
/// this connection is only ever routed here in the first place because
/// the accept loop already confirmed this exact peer id was expected
/// (see `pending_screen_stream_peers`) — a second identity round-trip
/// would confirm nothing a redundant read wouldn't already need to pay
/// for.
async fn handle_screen_stream_connection(mut conn: Connection, state: Arc<SharedState>, peer_crypto_id: String) -> anyhow::Result<()> {
    let Message::ScreenStreamHandshake { session_id } = read_message(&mut conn).await? else {
        anyhow::bail!("expected ScreenStreamHandshake as the first message on a screen-stream connection");
    };

    let valid = {
        let sessions = state.remote_control_sessions.lock().unwrap();
        sessions
            .get(&peer_crypto_id)
            .is_some_and(|s| s.role == RemoteControlRole::Controlling && s.active && s.session_id == session_id)
    };
    if !valid {
        anyhow::bail!("no active controlling session for {peer_crypto_id} matching session {session_id}");
    }

    let result = async {
        loop {
            let frame = read_frame(&mut conn).await?;
            state.emit(SyncEvent::ScreenFrameReceived {
                peer_id: peer_crypto_id.clone(),
                session_id: session_id.clone(),
                frame,
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Reaches here on a real error (peer closed the stream, network died)
    // — an *intentional* end (either side's `EndRemoteControlSession`)
    // aborts this whole task from outside instead, so execution never
    // gets this far in that case. `end_remote_control_session` is
    // idempotent, so this is still safe to call even if something else
    // already did.
    end_remote_control_session(&state, &peer_crypto_id, Some("screen stream ended unexpectedly".to_string()), true);
    result
}

/// The controlled side's half of the screen stream: dials a fresh
/// connection to the controlling peer, identifies it with
/// `ScreenStreamHandshake`, then pushes every frame `start_capture`
/// produces until the channel closes (the engine dropped its capture
/// handle — see `end_remote_control_session`'s `stop_capture` call) or a
/// write fails (the connection died). Always ends by tearing the session
/// down — there's no path back to "idle but still accepted," a stream
/// that stops is a session that's over.
async fn push_screen_stream(
    state: Arc<SharedState>,
    peer_crypto_id: String,
    session_id: String,
    addr: SocketAddr,
    mut frames: mpsc::Receiver<Vec<u8>>,
) {
    let mut conn = match connect(addr, &state.tls_identity).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("couldn't dial screen-stream connection to {addr}: {e}");
            end_remote_control_session(&state, &peer_crypto_id, Some(format!("couldn't open screen stream: {e}")), true);
            return;
        }
    };
    if let Err(e) = write_message(&mut conn, &Message::ScreenStreamHandshake { session_id }).await {
        tracing::debug!("couldn't send screen-stream handshake to {addr}: {e}");
        end_remote_control_session(&state, &peer_crypto_id, Some(format!("couldn't start screen stream: {e}")), true);
        return;
    }
    while let Some(frame) = frames.recv().await {
        if write_frame(&mut conn, &frame).await.is_err() {
            break;
        }
    }
    end_remote_control_session(&state, &peer_crypto_id, None, false);
}

/// Claims the right to start a single outbound dial to `peer_id` — `true`
/// means proceed, `false` means skip (already connected, or another dial
/// to the same peer is already in flight). The caller **must** eventually
/// pair a successful claim with `release_dial_claim`, on every exit path
/// (`connect()` failing included, not just a completed `handle_connection`).
/// `HashSet::insert`'s own return value is the actual source of truth for
/// who wins a race between concurrent callers — the `connected` check
/// above it is just a cheap, non-atomic fast path to skip dialing a peer
/// that's obviously already connected; a stale read there costs nothing
/// beyond one wasted `dialing` entry that `handle_connection`'s own
/// "already connected" bail-out (and this same release path) cleans up
/// immediately.
fn try_claim_dial(state: &Arc<SharedState>, peer_id: &str) -> bool {
    if state.connected.lock().unwrap().contains(peer_id) {
        return false;
    }
    state.dialing.lock().unwrap().insert(peer_id.to_string())
}

fn release_dial_claim(state: &Arc<SharedState>, peer_id: &str) {
    state.dialing.lock().unwrap().remove(peer_id);
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
        discovery: Mutex::new(discovery),
        discovery_recreate: Notify::new(),
        trust_store: Mutex::new(config.trust_store),
        connected: Mutex::new(HashSet::new()),
        dialing: Mutex::new(HashSet::new()),
        connection_handles: Mutex::new(HashMap::new()),
        manually_disconnected: Mutex::new(HashSet::new()),
        known_addresses: Mutex::new(HashMap::new()),
        peer_senders: Mutex::new(HashMap::new()),
        last_programmatic_hash: Mutex::new(None),
        pending_pairings: Mutex::new(HashMap::new()),
        pending_file_accepts: Mutex::new(HashMap::new()),
        clipboard: config.clipboard,
        media: config.media,
        remote_control: config.remote_control,
        remote_control_sessions: Mutex::new(HashMap::new()),
        pending_remote_control_requests: Mutex::new(HashMap::new()),
        pending_screen_stream_peers: Mutex::new(HashSet::new()),
        remote_control_stream_handles: Mutex::new(HashMap::new()),
        events_tx,
        received_files_dir: config.received_files_dir,
        paused: AtomicBool::new(false),
        shutting_down: AtomicBool::new(false),
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
                        // **Real bug found and fixed while adding the
                        // remote-control screen stream**: this used to
                        // cache `addr` into `known_addresses` here, but
                        // `addr` is the *remote ephemeral port* of this
                        // specific inbound TCP connection (straight from
                        // `TcpListener::accept()`), never the peer's
                        // actual listening port — dialing it back later
                        // would connect to nothing, or nothing useful.
                        // Mostly silently harmless before now: the mDNS
                        // browse loop below writes the *correct*
                        // (listening-port) address for every peer it
                        // sees regardless of connection direction, and
                        // usually re-writes it again soon after,
                        // papering over the bad entry in practice. It
                        // stopped being harmless once something needed
                        // to reliably dial a peer *immediately* after
                        // accepting a connection from them, before
                        // mDNS's next natural re-announce had a chance
                        // to overwrite the bad value again —
                        // `push_screen_stream` dialing back to open the
                        // dedicated screen-stream connection right after
                        // accepting a remote-control request does
                        // exactly that. Caught by
                        // `core/continuity-daemon/tests/remote_control.rs`
                        // flaking under real concurrent mDNS activity
                        // (multiple engine pairs in one test binary) —
                        // reliable in isolation, where there was more
                        // incidental time for mDNS to have already
                        // fixed the entry. Fixed by simply not writing
                        // here at all — mDNS discovery (see below) is
                        // the only source that's ever actually correct
                        // for this map.
                        if state.manually_disconnected.lock().unwrap().contains(&peer_crypto_id) {
                            tracing::debug!("rejecting inbound connection from {addr}: manually disconnected");
                            continue;
                        }
                        // A screen-stream connection is recognized purely
                        // by identity, not by peeking at anything it
                        // sends — this device already knows it's
                        // expecting exactly one more inbound connection
                        // from this specific, TLS-cert-proven peer id
                        // (see `pending_screen_stream_peers`'s doc
                        // comment), so there's nothing to read-and-decide
                        // here, no added latency on the far more common
                        // plain-reconnect case.
                        if state.pending_screen_stream_peers.lock().unwrap().remove(&peer_crypto_id) {
                            let conn_state = state.clone();
                            let id_for_registry = peer_crypto_id.clone();
                            let join = tokio::spawn(async move {
                                if let Err(e) = handle_screen_stream_connection(conn, conn_state, peer_crypto_id).await {
                                    tracing::debug!("screen-stream connection from {addr} ended: {e}");
                                }
                            });
                            state
                                .remote_control_stream_handles
                                .lock()
                                .unwrap()
                                .insert(id_for_registry, join.abort_handle());
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
                    Err(e) => {
                        tracing::warn!("accept error: {e}");
                        // A transient per-connection error (a reset before
                        // the handshake, say) should just retry on the next
                        // iteration — but some accept errors (the process
                        // hitting its file descriptor limit, most notably)
                        // recur on *every* immediate retry with nothing to
                        // wait on, which without this would spin this loop
                        // at 100% CPU on one core forever. A short, fixed
                        // backoff is cheap insurance against that either
                        // way, and invisible in the one-shot-error case.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        })
    });

    tasks.push({
        let state = state.clone();
        tokio::spawn(async move {
            let mut browse_rx = browse_rx;
            'recover: loop {
            loop {
                let event = tokio::select! {
                    biased;
                    // Checked first: an explicit recreate request (network
                    // change, or this same signal reused by the recovery
                    // loop below after a failed attempt) should win over a
                    // stale event that happened to be queued on the old
                    // receiver at the same moment.
                    _ = state.discovery_recreate.notified() => break,
                    result = browse_rx.recv_async() => match result {
                        Ok(event) => event,
                        Err(_) => break,
                    },
                };
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
                if state.manually_disconnected.lock().unwrap().contains(&peer.device.id) {
                    continue;
                }
                if !try_claim_dial(&state, &peer.device.id) {
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
                        Err(e) => {
                            tracing::debug!("failed to dial {peer_addr}: {e}");
                            release_dial_claim(&conn_state, &peer_id);
                        }
                    }
                });
                state
                    .connection_handles
                    .lock()
                    .unwrap()
                    .insert(id_for_registry, join.abort_handle());
            }
            // Got here either because `recv_async` returned `Err` (mdns-sd's
            // daemon thread shut its sending half down for good — a crash
            // or unexpected internal exit) or because `discovery_recreate`
            // was notified (an explicit ask, currently only the network
            // watcher below). Either way: tear down and recreate the whole
            // `Discovery` (a fresh `ServiceDaemon`, re-advertised,
            // re-browsed), swap it into `state.discovery` so `shutdown()`
            // still reaches the current one, and resume browsing on the new
            // receiver. Retries with a backoff if the daemon can't even be
            // recreated yet (e.g. the network stack is mid-flap). Without
            // this, a dead daemon thread left discovery — new devices *and*
            // rediscovery-triggered reconnects — silently dead for the rest
            // of the process's life.
            tracing::warn!("mDNS browse loop stopped — recovering discovery");
            loop {
                if state.shutting_down.load(Ordering::Relaxed) {
                    break 'recover;
                }
                let recreated = Discovery::new().and_then(|d| {
                    d.advertise(&state.my_device, port)?;
                    let rx = d.browse()?;
                    Ok((d, rx))
                });
                match recreated {
                    Ok((new_discovery, new_rx)) => {
                        let old = std::mem::replace(&mut *state.discovery.lock().unwrap(), new_discovery);
                        let _ = old.shutdown();
                        browse_rx = new_rx;
                        tracing::info!("mDNS discovery recovered");
                        continue 'recover;
                    }
                    Err(e) => {
                        tracing::error!("failed to recover mDNS discovery, retrying: {e}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
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
                        // A dial aborted mid-flight (its task dropped by
                        // the `handle.abort()` calls just above, before it
                        // could reach either `handle_connection` or its own
                        // `Err` branch) never gets to release its own
                        // claim — same reasoning as the `connected`/
                        // `peer_senders` clears right here, just for
                        // `dialing`.
                        state.dialing.lock().unwrap().clear();
                        state.peer_senders.lock().unwrap().clear();
                        // Same "abort skips the normal cleanup tail"
                        // reasoning as everywhere else here — every
                        // active/pending remote-control session needs
                        // tearing down explicitly since the connections
                        // that would have done it themselves were just
                        // aborted out from under them.
                        let session_peers: Vec<String> = state.remote_control_sessions.lock().unwrap().keys().cloned().collect();
                        for peer_id in session_peers {
                            end_remote_control_session(&state, &peer_id, None, false);
                        }
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
                        // Same reasoning as `Reset`'s clear — if `peer_crypto_id`
                        // happened to be mid-dial (not yet connected) at the
                        // moment of the abort above, its task never got to
                        // release its own claim.
                        release_dial_claim(&state, &peer_crypto_id);
                        state.peer_senders.lock().unwrap().remove(&peer_crypto_id);
                        end_remote_control_session(&state, &peer_crypto_id, None, false);

                        // Aborting the connection task skips its own normal
                        // exit-path `Disconnected` emit (abort cancels
                        // immediately, no more of that future's code runs),
                        // so it has to happen here instead.
                        state.emit(SyncEvent::Disconnected {
                            peer_id: peer_crypto_id,
                            peer_name,
                        });
                    }
                    EngineCommand::RevokeDevice { peer_crypto_id } => {
                        let peer_name = state
                            .trust_store
                            .lock()
                            .unwrap()
                            .get(&peer_crypto_id)
                            .map(|d| d.name.clone())
                            .unwrap_or_else(|| peer_crypto_id.clone());

                        // Best-effort — if the send fails (channel already
                        // gone) there's nothing to notify anyway, the
                        // connection's already on its way down.
                        let notified_peer = if let Some(tx) = state.peer_senders.lock().unwrap().get(&peer_crypto_id) {
                            tx.send(Message::Revoked).is_ok()
                        } else {
                            false
                        };

                        if let Err(e) = state.trust_store.lock().unwrap().revoke(&peer_crypto_id) {
                            tracing::warn!("failed to revoke {peer_crypto_id} from trust store: {e}");
                        }
                        // Emitted right away rather than after the delayed
                        // abort below — a shell updating its UI shouldn't
                        // wait on a network flush it has no reason to care
                        // about.
                        state.emit(SyncEvent::WasRevoked { peer_id: peer_crypto_id.clone(), peer_name });

                        // Queuing onto `tx` above only *schedules* the
                        // write on the connection's own writer loop, which
                        // needs an actual scheduler turn to run and put it
                        // on the wire — aborting the connection task right
                        // away would otherwise very reliably win that race
                        // (no `.await` in between means the writer task
                        // never even gets polled once), silently dropping
                        // the notification every time instead of just
                        // occasionally. Spawned separately rather than
                        // `.await`ed inline so a brief, bounded wait here
                        // doesn't stall this shared command loop from
                        // processing whatever's queued behind it.
                        let state = state.clone();
                        tokio::spawn(async move {
                            if notified_peer {
                                tokio::time::sleep(Duration::from_millis(150)).await;
                            }
                            // Same abort/cleanup sequence as `DisconnectPeer`
                            // — aborting skips `handle_connection`'s own
                            // normal cleanup tail, so it's repeated here.
                            // Deliberately *not* added to
                            // `manually_disconnected`: that set is for
                            // "stays paired, don't auto-reconnect", but this
                            // peer is being forgotten outright — the
                            // `is_trusted` check in the mDNS auto-dial loop
                            // is what actually keeps it from being silently
                            // redialed now, the same as any other untrusted
                            // device.
                            if let Some(handle) = state.connection_handles.lock().unwrap().remove(&peer_crypto_id) {
                                handle.abort();
                            }
                            state.connected.lock().unwrap().remove(&peer_crypto_id);
                            release_dial_claim(&state, &peer_crypto_id);
                            state.peer_senders.lock().unwrap().remove(&peer_crypto_id);
                            end_remote_control_session(&state, &peer_crypto_id, None, false);
                        });
                    }
                    EngineCommand::ReconnectPeer { peer_crypto_id } => {
                        state.manually_disconnected.lock().unwrap().remove(&peer_crypto_id);
                        let Some(addr) = state.known_addresses.lock().unwrap().get(&peer_crypto_id).copied() else {
                            state.emit(SyncEvent::ReconnectFailed { peer_id: peer_crypto_id });
                            continue;
                        };
                        if !try_claim_dial(&state, &peer_crypto_id) {
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
                                Err(e) => {
                                    tracing::debug!("failed to reconnect to {addr}: {e}");
                                    release_dial_claim(&conn_state, &peer_crypto_id);
                                }
                            }
                        });
                        state.connection_handles.lock().unwrap().insert(id_for_registry, join.abort_handle());
                    }
                    EngineCommand::SendMediaCommand { peer_crypto_id, command } => {
                        if let Some(tx) = state.peer_senders.lock().unwrap().get(&peer_crypto_id) {
                            let _ = tx.send(Message::MediaCommand { command });
                        }
                    }
                    EngineCommand::RefreshDiscovery => redial_disconnected_trusted_peers(&state),
                    EngineCommand::NetworkChanged => {
                        tracing::info!("network change detected — refreshing discovery and retrying disconnected peers");
                        state.discovery_recreate.notify_one();
                        redial_disconnected_trusted_peers(&state);
                    }
                    EngineCommand::RequestRemoteControl { peer_crypto_id } => {
                        // One session per peer at a time — a second
                        // request while one's already pending/active with
                        // the same peer would just confuse which
                        // `session_id` the eventual response belongs to.
                        if state.remote_control_sessions.lock().unwrap().contains_key(&peer_crypto_id) {
                            continue;
                        }
                        let Some(tx) = state.peer_senders.lock().unwrap().get(&peer_crypto_id).cloned() else {
                            continue;
                        };
                        let session_id = format!("{:016x}", rand::random::<u64>());
                        state.remote_control_sessions.lock().unwrap().insert(
                            peer_crypto_id.clone(),
                            RemoteControlSession { session_id: session_id.clone(), role: RemoteControlRole::Controlling, active: false },
                        );
                        let _ = tx.send(Message::RemoteControlRequest { session_id });
                    }
                    EngineCommand::RespondToRemoteControlRequest { peer_crypto_id, accept } => {
                        let Some(session_id) = state.pending_remote_control_requests.lock().unwrap().remove(&peer_crypto_id) else {
                            continue;
                        };
                        let Some(tx) = state.peer_senders.lock().unwrap().get(&peer_crypto_id).cloned() else {
                            continue;
                        };

                        if !accept {
                            let _ = tx.send(Message::RemoteControlResponse { session_id, accepted: false });
                            continue;
                        }

                        // This device's keyboard/mouse/screen are one
                        // shared physical resource — two different peers
                        // controlling it at once would mean both fighting
                        // over the same cursor, and `RemoteControlHost`'s
                        // capture start/stop isn't session-scoped, just a
                        // single on/off switch. Accepting a second
                        // Controlled-role session while one's already
                        // active would silently corrupt that shared
                        // state (the first session's eventual
                        // `stop_capture` would kill the second one's
                        // stream too) — declined outright instead.
                        let already_controlled = state
                            .remote_control_sessions
                            .lock()
                            .unwrap()
                            .values()
                            .any(|s| s.role == RemoteControlRole::Controlled);
                        if already_controlled {
                            let _ = tx.send(Message::RemoteControlResponse { session_id, accepted: false });
                            continue;
                        }

                        // Accepting starts capture *before* telling the
                        // peer it succeeded — if it fails (no Screen
                        // Recording permission, say), the peer hears
                        // about a session that never really started
                        // instead of one that silently never sends any
                        // frames.
                        let Some(frames) = state.remote_control.start_capture() else {
                            let _ = tx.send(Message::RemoteControlResponse { session_id: session_id.clone(), accepted: false });
                            state.emit(SyncEvent::RemoteControlSessionEnded {
                                peer_id: peer_crypto_id.clone(),
                                peer_name: state
                                    .trust_store
                                    .lock()
                                    .unwrap()
                                    .get(&peer_crypto_id)
                                    .map(|d| d.name.clone())
                                    .unwrap_or_else(|| peer_crypto_id.clone()),
                                session_id,
                                reason: Some("couldn't start screen capture".to_string()),
                            });
                            continue;
                        };

                        let Some(addr) = state.known_addresses.lock().unwrap().get(&peer_crypto_id).copied() else {
                            let _ = tx.send(Message::RemoteControlResponse { session_id, accepted: false });
                            state.remote_control.stop_capture();
                            continue;
                        };

                        state.remote_control_sessions.lock().unwrap().insert(
                            peer_crypto_id.clone(),
                            RemoteControlSession { session_id: session_id.clone(), role: RemoteControlRole::Controlled, active: true },
                        );
                        let _ = tx.send(Message::RemoteControlResponse { session_id: session_id.clone(), accepted: true });
                        state.emit(SyncEvent::RemoteControlSessionStarted {
                            peer_id: peer_crypto_id.clone(),
                            peer_name: state
                                .trust_store
                                .lock()
                                .unwrap()
                                .get(&peer_crypto_id)
                                .map(|d| d.name.clone())
                                .unwrap_or_else(|| peer_crypto_id.clone()),
                            session_id: session_id.clone(),
                            role: RemoteControlRole::Controlled,
                        });

                        let push_state = state.clone();
                        let push_peer_id = peer_crypto_id.clone();
                        let join = tokio::spawn(async move {
                            push_screen_stream(push_state, push_peer_id, session_id, addr, frames).await;
                        });
                        state.remote_control_stream_handles.lock().unwrap().insert(peer_crypto_id, join.abort_handle());
                    }
                    EngineCommand::SendInputEvent { peer_crypto_id, event } => {
                        let valid_controlling_session = state
                            .remote_control_sessions
                            .lock()
                            .unwrap()
                            .get(&peer_crypto_id)
                            .filter(|s| s.role == RemoteControlRole::Controlling && s.active)
                            .map(|s| s.session_id.clone());
                        let Some(session_id) = valid_controlling_session else {
                            continue;
                        };
                        if let Some(tx) = state.peer_senders.lock().unwrap().get(&peer_crypto_id) {
                            let _ = tx.send(Message::InputEvent { session_id, event });
                        }
                    }
                    EngineCommand::EndRemoteControlSession { peer_crypto_id } => {
                        end_remote_control_session(&state, &peer_crypto_id, None, true);
                    }
                }
            }
        })
    });

    tasks.push(spawn_clipboard_watcher(state.clone()));
    tasks.push(spawn_now_playing_watcher(state.clone()));
    tasks.push(spawn_reconnect_ticker(state.clone()));
    tasks.push(spawn_network_watcher(commands_tx.clone()));

    Ok(EngineHandle {
        events: events_rx,
        commands_tx,
        tasks,
        state,
    })
}

async fn handle_connection(
    conn: Connection,
    state: Arc<SharedState>,
    peer_crypto_id: String,
) -> anyhow::Result<()> {
    // No-op for an inbound connection (never claimed a dial in the first
    // place — `remove` on an absent key just returns `false`). For an
    // outbound one, the dial phase this claim was guarding is over now
    // that a `Connection` exists — release it before, not after, the
    // `connected` check right below so a peer whose connection later
    // drops can be freely redialed without waiting on this one.
    release_dial_claim(&state, &peer_crypto_id);
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
    // A remote-control session has no reason to survive the mesh
    // connection that authorized it — input events and the screen stream
    // both depend on this same peer relationship staying intact.
    // `notify_peer: false` since there's no live connection left to send
    // `RemoteControlEnded` over anyway (idempotent either way if some
    // other path already cleaned this up).
    end_remote_control_session(&state, &peer_crypto_id, Some("connection ended".to_string()), false);
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

    // Tracks time since the last *activity* — a successful read OR write,
    // shared between the reader and writer loops below. Two real bugs
    // found post-release here:
    //
    // 1. A large/slow file transfer enqueues every chunk into the same
    //    unbounded FIFO `tx`/`rx` channel a `Ping` also goes through, with
    //    no priority between them. A ping queued behind a big backlog of
    //    chunks doesn't actually reach the wire until the writer works
    //    through that backlog, so the peer never gets it to reply to in
    //    time — and tracking only *incoming* messages meant the sending
    //    side (mostly writing, not reading, during a one-way transfer)
    //    could hit `CONNECTION_READ_TIMEOUT` and disconnect itself
    //    mid-transfer even though the connection was actively,
    //    successfully carrying data the whole time. A successful write is
    //    just as much proof of a live connection as a successful read (a
    //    truly dead connection's writes eventually fail/block once the OS
    //    send buffer and retries are exhausted) — tracked from both sides
    //    now, this can't happen.
    //
    // 2. The writer used to run as its own separately `tokio::spawn`ed
    //    task, holding `write_half`. `tokio::io::split` keeps the
    //    underlying stream alive (via a shared `Arc`) as long as *either*
    //    half is still referenced — so aborting only this connection's own
    //    task from outside (`DisconnectPeer`, `Reset`,
    //    `EngineHandle::shutdown`, the only abort handle ever tracked
    //    anywhere) dropped `read_half` but left the writer task — and
    //    `write_half`, and the underlying socket — running untouched,
    //    forever. A real leak on every non-organic disconnect. Racing the
    //    reader and writer as two futures *within this one task* instead
    //    (rather than a second spawned task) means dropping this task
    //    drops both halves together, every time.
    let last_activity = Arc::new(Mutex::new(std::time::Instant::now()));

    let mut receiving: HashMap<String, ReceivingFile> = HashMap::new();
    let mut ping_ticker = tokio::time::interval(PING_INTERVAL);
    ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let reader = async {
        loop {
            let read_result = tokio::select! {
                _ = ping_ticker.tick() => {
                    let elapsed = last_activity.lock().unwrap().elapsed();
                    if elapsed > CONNECTION_READ_TIMEOUT {
                        tracing::debug!(
                            "no activity with '{}' in over {CONNECTION_READ_TIMEOUT:?} — treating as disconnected",
                            peer.name
                        );
                        break;
                    }
                    // Piggybacks on this same tick rather than a separate
                    // timer — a shell's device list can show "active Ns
                    // ago" instead of a bare connected/disconnected dot,
                    // at the same bounded per-connection rate as the ping
                    // itself.
                    state.emit(SyncEvent::PeerActivity {
                        peer_id: peer.id.clone(),
                        seconds_since_activity: elapsed.as_secs(),
                    });
                    if tx.send(Message::Ping).is_err() {
                        break; // writer side is gone — connection is already dead
                    }
                    continue;
                }
                result = read_message(&mut read_half) => result,
            };
            *last_activity.lock().unwrap() = std::time::Instant::now();
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
                Ok(Message::Revoked) => {
                    state.emit(SyncEvent::RevokedByPeer {
                        peer_id: peer.id.clone(),
                        peer_name: peer.name.clone(),
                    });
                    // The sender is about to close their side anyway, but
                    // don't wait on that — end the loop here so the
                    // `Disconnected` emit below (and this side's own
                    // connection-state cleanup in `handle_connection`)
                    // isn't left hanging on a socket the peer no longer
                    // wants open.
                    break;
                }
                Ok(Message::RemoteControlRequest { session_id }) => {
                    if !state.remote_control.is_available() {
                        // Nothing for the local user to decide — this
                        // device genuinely can't be remotely controlled
                        // (Android/iOS, Linux, or a "lite" build), so
                        // auto-decline instead of surfacing a prompt for
                        // a capability that doesn't exist.
                        if let Some(tx) = state.peer_senders.lock().unwrap().get(&peer.id) {
                            let _ = tx.send(Message::RemoteControlResponse { session_id, accepted: false });
                        }
                        continue;
                    }
                    state.pending_remote_control_requests.lock().unwrap().insert(peer.id.clone(), session_id.clone());
                    state.emit(SyncEvent::RemoteControlRequested {
                        peer_id: peer.id.clone(),
                        peer_name: peer.name.clone(),
                        session_id,
                    });
                }
                Ok(Message::RemoteControlResponse { session_id, accepted }) => {
                    let matches_pending = state
                        .remote_control_sessions
                        .lock()
                        .unwrap()
                        .get(&peer.id)
                        .is_some_and(|s| s.role == RemoteControlRole::Controlling && !s.active && s.session_id == session_id);
                    if !matches_pending {
                        continue;
                    }
                    if !accepted {
                        state.remote_control_sessions.lock().unwrap().remove(&peer.id);
                        state.emit(SyncEvent::RemoteControlDeclined { peer_id: peer.id.clone(), peer_name: peer.name.clone() });
                        continue;
                    }
                    if let Some(s) = state.remote_control_sessions.lock().unwrap().get_mut(&peer.id) {
                        s.active = true;
                    }
                    // The peer is about to dial back with the screen
                    // stream — recognized by identity alone once it
                    // arrives, see the accept loop above.
                    state.pending_screen_stream_peers.lock().unwrap().insert(peer.id.clone());
                    state.emit(SyncEvent::RemoteControlSessionStarted {
                        peer_id: peer.id.clone(),
                        peer_name: peer.name.clone(),
                        session_id,
                        role: RemoteControlRole::Controlling,
                    });
                }
                Ok(Message::RemoteControlEnded { session_id }) => {
                    let matches_active =
                        state.remote_control_sessions.lock().unwrap().get(&peer.id).is_some_and(|s| s.session_id == session_id);
                    if matches_active {
                        end_remote_control_session(state, &peer.id, None, false);
                    }
                }
                Ok(Message::InputEvent { session_id, event }) => {
                    let valid = state
                        .remote_control_sessions
                        .lock()
                        .unwrap()
                        .get(&peer.id)
                        .is_some_and(|s| s.role == RemoteControlRole::Controlled && s.active && s.session_id == session_id);
                    if valid {
                        state.remote_control.inject(event);
                    }
                }
                Ok(Message::ScreenStreamHandshake { .. }) => {
                    // Only ever expected as the very first (and only)
                    // message on a *separate*, dedicated screen-stream
                    // connection (see `handle_screen_stream_connection`)
                    // — arriving here, on the normal mesh connection,
                    // means either a bug or a confused/malicious peer.
                    // Ignored like any other unexpected message rather
                    // than treated as fatal.
                    tracing::debug!("ignoring ScreenStreamHandshake on the mesh connection from '{}'", peer.name);
                }
                Ok(other) => tracing::debug!("ignoring unhandled message from '{}': {other:?}", peer.name),
                Err(_) => break,
            }
        }
    };

    let writer = async {
        while let Some(msg) = rx.recv().await {
            if write_message(&mut write_half, &msg).await.is_err() {
                break;
            }
            *last_activity.lock().unwrap() = std::time::Instant::now();
        }
    };

    // Whichever finishes first — the reader deciding the connection is
    // dead, or the writer hitting a write error — the other is dropped
    // right along with it, taking its half of the split stream with it.
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }

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
    // SHA-256 over the whole file, synchronously, used to run directly on
    // this tokio worker thread — for a large file that's real, CPU-bound
    // work blocking that thread from polling anything else scheduled on
    // it for as long as the hash takes (a debug build's unoptimized SHA-256
    // is dramatically slower than a release build's; this showed up as a
    // ~15s stall before the very first message of a 150MB transfer even
    // reached the wire, when testing the keepalive fix below). `spawn_
    // blocking` moves it to tokio's dedicated blocking-task thread pool
    // instead, matching every other CPU/host-bound call in this codebase
    // (the clipboard and now-playing watchers, for the same reason).
    let (data, hash) = match tokio::task::spawn_blocking(move || {
        let hash = content_hash(&data);
        (data, hash)
    })
    .await
    {
        Ok(result) => result,
        Err(e) => {
            state.emit(SyncEvent::FileTransferFailed {
                transfer_id,
                reason: format!("hashing task panicked: {e}"),
            });
            return;
        }
    };

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

            if state.shutting_down.load(Ordering::Relaxed) {
                break;
            }

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

            if state.shutting_down.load(Ordering::Relaxed) {
                break;
            }

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

/// Retries dialing every trusted peer with a cached address that isn't
/// currently connected (and wasn't deliberately disconnected — that stays
/// respected). Shared by the manual `RefreshDiscovery` command and
/// `spawn_reconnect_ticker` below.
///
/// **Deliberately does not touch mDNS.** `mdns_sd::ServiceDaemon::browse()`
/// unconditionally overwrites its single registered listener for a service
/// type ("if there is already a listener, it will be updated, i.e.
/// overwritten" — straight from its own doc comment), so calling it a
/// second time silently orphans the receiver this engine's whole browse
/// loop actually reads from: a real bug, shipped and caught after the
/// fact — it killed all further discovery (new devices *and*
/// rediscovery-triggered reconnects) after the first refresh, whether from
/// a timer or the user's own Refresh click. mdns-sd already re-queries on
/// its own via retransmission using the *original* listener, so nothing
/// external is needed for that half — this only needs to redial peers
/// whose address is already cached in `known_addresses`.
fn redial_disconnected_trusted_peers(state: &Arc<SharedState>) {
    let targets: Vec<(String, SocketAddr)> = {
        let trust_store = state.trust_store.lock().unwrap();
        let connected = state.connected.lock().unwrap();
        let dialing = state.dialing.lock().unwrap();
        let manually_disconnected = state.manually_disconnected.lock().unwrap();
        let known_addresses = state.known_addresses.lock().unwrap();
        trust_store
            .list()
            .filter(|d| !connected.contains(&d.id) && !dialing.contains(&d.id) && !manually_disconnected.contains(&d.id))
            .filter_map(|d| known_addresses.get(&d.id).copied().map(|addr| (d.id.clone(), addr)))
            .collect()
    };
    for (peer_crypto_id, addr) in targets {
        // This runs from both a periodic ticker and the manual Refresh
        // command — two independent tasks that could both reach this same
        // peer in the same tick — on top of racing the mDNS auto-dial
        // loop's own attempts at any moment. Same claim as everywhere else
        // that starts a dial, for the same reason.
        if !try_claim_dial(state, &peer_crypto_id) {
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
                Err(e) => {
                    tracing::debug!("reconnect to {addr} failed: {e}");
                    release_dial_claim(&conn_state, &peer_crypto_id);
                }
            }
        });
        state.connection_handles.lock().unwrap().insert(id_for_registry, join.abort_handle());
    }
}

/// Self-heals a disconnected-but-trusted peer without the user needing to
/// notice and hit the manual Refresh action. Real motivation: after
/// `mdns_sd`'s own retransmission for a long-running query backs off (up
/// to a 60-minute max delay — see its retransmission scheduling), waiting
/// on rediscovery alone to trigger a reconnect could take a very long
/// time. This doesn't wait on discovery at all — see
/// `redial_disconnected_trusted_peers`.
const RECONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(45);

fn spawn_reconnect_ticker(state: Arc<SharedState>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RECONNECT_RETRY_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if state.paused.load(Ordering::Relaxed) {
                continue;
            }
            redial_disconnected_trusted_peers(&state);
        }
    })
}

/// How often to check the host's local IP addresses for changes. This is a
/// plain poll rather than a native OS push notification (macOS's
/// `NWPathMonitor`, Android's `ConnectivityManager.NetworkCallback`, an
/// equivalent on Windows/Linux) specifically because a poll is portable,
/// cross-platform code that lives in this crate and is testable here —
/// separate native FFI per platform would need real Windows/Android
/// hardware or emulators to verify, neither available in this dev
/// environment. 10s is frequent enough that a Wi-Fi switch or VPN
/// connect/disconnect is noticed well within the time a user would
/// consider the app "just stuck", without polling so often it shows up in
/// battery/CPU profiling on mobile.
const NETWORK_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Notices when the host's local IP addresses change and prompts a
/// rediscovery + reconnect pass — the whole reason this exists is that
/// otherwise nothing reacts to a network change at all until either the
/// user manually hits refresh or the periodic reconnect ticker's own timer
/// happens to fire, which could be tens of seconds after a Wi-Fi switch
/// most users would expect to "just work" immediately.
fn spawn_network_watcher(commands_tx: mpsc::UnboundedSender<EngineCommand>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_addrs: Option<HashSet<std::net::IpAddr>> = None;
        loop {
            tokio::time::sleep(NETWORK_CHECK_INTERVAL).await;

            let current: HashSet<std::net::IpAddr> = match if_addrs::get_if_addrs() {
                Ok(interfaces) => interfaces
                    .into_iter()
                    .filter(|iface| !iface.is_loopback())
                    .map(|iface| iface.ip())
                    .collect(),
                Err(e) => {
                    tracing::debug!("couldn't enumerate network interfaces: {e}");
                    continue;
                }
            };

            // `None` only on the very first check — skip notifying then,
            // since discovery was just freshly set up by `start()` and
            // there's nothing to react to yet.
            if let Some(previous) = &last_addrs {
                if *previous != current {
                    let _ = commands_tx.send(EngineCommand::NetworkChanged);
                }
            }
            last_addrs = Some(current);
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

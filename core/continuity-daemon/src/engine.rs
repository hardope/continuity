use crate::clipboard::ClipboardBackend;
use crate::events::{EngineCommand, FileTransferDirection, RemoteControlRole, SyncEvent};
use crate::media::MediaController;
use crate::remote_control::RemoteControlHost;
use crate::screen_lock::{ScreenLockController, ScreenLockError};
use continuity_crypto::{
    content_hash, generate_self_signed, Identity, IncrementalHash, TlsIdentity, TrustStore,
    TrustedDevice,
};
use continuity_net::{
    announce_and_identify, announce_and_identify_after, connect, peer_from_service_info, read_frame, read_message,
    start_pairing, start_pairing_after, write_frame, write_message, Connection, Discovery, FramingError, Listener,
    ServiceEvent,
};
use continuity_proto::{
    DeviceInfo, Message, NowPlayingInfo, Platform, ScreenLockAction, ScreenLockOutcome, PROTOCOL_VERSION,
};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;

/// The v3 file data connection: dialing it, streaming over it, receiving
/// from it, and the resume state that outlives it — see that module's own
/// docs. Everything below this point that still talks about `FileChunk`s is
/// the in-band path, kept unchanged for peers older than v3.
mod transfer;

/// In-band (pre-v3) `FileChunk` size — see `transfer::FRAME_SIZE` for the
/// data connection's.
const CHUNK_SIZE: usize = 64 * 1024;
const FILE_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// How much progress has to accumulate before another `FileTransferProgress`
/// is worth emitting — one per 64KB chunk would be hundreds of events for
/// a real file, far more than any UI needs to redraw a progress bar at a
/// rate a human can perceive.
const PROGRESS_EVENT_BYTES: u64 = 1024 * 1024;
/// Caps how many read-but-not-yet-written chunks `send_file` is allowed
/// to get ahead of the writer loop, in units of `CHUNK_SIZE` — 32 * 64KB
/// = 2MB. Below this, `send_file`'s read loop blocks instead of reading
/// more of the file. **Real bug fixed here**: `send_file` used to read
/// the *entire* file into memory up front, then enqueue every chunk into
/// an unbounded channel with no limit on how far that could outrun the
/// network — for a 260MB file that meant up to ~2x the file size in
/// transient memory (the whole buffer, plus a full duplicate queued
/// ahead of the actual writes), enough to OOM a memory-constrained
/// device like Android (no `android:largeHeap`, so a few hundred MB of
/// default headroom). This bounds memory to a small, fixed window
/// regardless of file size, and doubles as real backpressure: a slow
/// peer now measurably slows down how fast this side reads the file,
/// rather than racing ahead of it.
const SEND_WINDOW_CHUNKS: usize = 32;
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

/// How long the controlling side waits, after its screen stream closes, for
/// the controlled side's own `RemoteControlEnded` (which says why) before
/// calling the end unexpected — see `handle_screen_stream_connection`.
const SCREEN_STREAM_END_GRACE: Duration = Duration::from_millis(750);

/// How long accepting a remote-control request waits for this side's own
/// mDNS to resolve the requester's address, when it hasn't yet — see
/// `accept_remote_control_session`.
const DIAL_BACK_ADDRESS_WAIT: Duration = Duration::from_secs(5);

/// How long an inbound connection may take over its TLS handshake. Each one
/// runs in a task of its own (see the accept loop), so a slow one only
/// holds up itself. **Real bug fixed here**: the accept loop used to do
/// each handshake itself, with no limit, so one connection that never sent
/// its hello — anyone on the network can open one, no pairing needed —
/// stopped this device accepting anything ever again: reconnects, file
/// data connections, screen streams. A handshake on a LAN takes
/// milliseconds; this only has to outlast a very busy machine.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Inbound TLS handshakes allowed under way at once. Each holds a socket
/// for up to `TLS_HANDSHAKE_TIMEOUT`; past this, new connections are
/// dropped straight away rather than let a flood of idle ones use up this
/// process's file descriptors.
const MAX_PENDING_HANDSHAKES: usize = 64;

pub struct EngineConfig {
    pub identity: Identity,
    pub device_name: String,
    pub trust_store: TrustStore,
    pub clipboard: Arc<dyn ClipboardBackend>,
    pub media: Arc<dyn MediaController>,
    pub remote_control: Arc<dyn RemoteControlHost>,
    /// Acts on `Message::ScreenLockRequest`s from peers — see
    /// `ScreenLockController`. `NoopScreenLockController` for any shell
    /// that can't (or shouldn't) be locked/unlocked remotely.
    pub screen_lock: Arc<dyn ScreenLockController>,
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

    /// Stops the engine. Dropping the handle does the same — see `Drop`.
    pub fn shutdown(self) {
        drop(self);
    }

    /// Switches for this crate's own integration tests — see `TestHooks`.
    /// Not part of the API any shell should use.
    #[doc(hidden)]
    pub fn test_hooks(&self) -> &TestHooks {
        &self.state.test_hooks
    }

    /// Whether this engine has an mDNS-resolved address for
    /// `peer_crypto_id` — what a v3 transfer needs to dial its data
    /// connection, falling back to in-band without it — for this crate's
    /// own integration tests.
    #[doc(hidden)]
    pub fn knows_address_for_test(&self, peer_crypto_id: &str) -> bool {
        self.state.known_addresses.lock().unwrap().contains_key(peer_crypto_id)
    }

    /// Drops the mesh connection to `peer_crypto_id` the way a network
    /// failure would — no manual-disconnect flag, transfers left to stall
    /// and resume — for this crate's own integration tests.
    #[doc(hidden)]
    pub fn drop_connection_for_test(&self, peer_crypto_id: &str) {
        if let Some(handle) = self.state.connection_handles.lock().unwrap().remove(peer_crypto_id) {
            handle.abort();
        }
        self.state.connected.lock().unwrap().remove(peer_crypto_id);
        self.state.peer_senders.lock().unwrap().remove(peer_crypto_id);
    }
}

/// Knobs and counters that let `tests/` drive the v3 file transfer paths
/// two real engines can't otherwise be made to take on demand (a data
/// connection cut mid-file, a receiver that won't use one), and see which
/// path a transfer actually took. All off / zero by default, and nothing
/// outside tests sets them.
#[doc(hidden)]
#[derive(Default)]
pub struct TestHooks {
    /// Sender: treat every peer as pre-v3 — in-band `FileChunk`s only.
    pub force_in_band: AtomicBool,
    /// Receiver: answer data-channel offers with `data_channel: false`.
    pub refuse_data_channel: AtomicBool,
    /// Sender: drop the next data connection once this many bytes of file
    /// data have gone out on it. One-shot (reset to 0 when used); 0 = off.
    pub cut_data_connection_after: AtomicU64,
    /// Receiver: the next data connection's disk writes fail, as on a full
    /// disk, once the file would grow past this many bytes. One-shot (reset
    /// to 0 when used); 0 = off.
    pub fail_writes_after: AtomicU64,
    /// Receiver: files verified and saved after arriving over a data
    /// connection.
    pub data_channel_files_received: AtomicU64,
    /// Receiver: files received over the in-band path.
    pub in_band_files_received: AtomicU64,
    /// Both: don't dial anyone on an mDNS resolution or a redial —
    /// `ReconnectPeer` still does — so a test can keep a dropped mesh
    /// connection down while it looks at what happens meanwhile.
    pub auto_dial_off: AtomicBool,
    /// Receiver: data connections a resume offer ended because this side
    /// hadn't seen them end yet (see `transfer::answer_offer`).
    pub stale_data_connections_ended: AtomicU64,
    /// Receiver: the last non-zero offset it answered a resume offer with.
    pub last_resume_offset: AtomicU64,
    /// Inbound mesh connections that had to be read before they could be
    /// told apart from a data connection (`transfer::route_inbound`).
    pub mesh_connections_read_first: AtomicU64,
    /// Announce this `protocol_version` instead of the real one on
    /// connections made from now on (0 = the real one) — to stand in for
    /// an older peer.
    pub announce_protocol_version: std::sync::atomic::AtomicU32,
}

/// Dropping the handle shuts the engine down, so a handle that goes away
/// without `shutdown()` — a test that panics, say — doesn't leave the
/// clipboard and now-playing watchers running forever. They run on tokio's
/// blocking pool, which a runtime waits for when it's dropped, so before
/// this a failed engine test hung instead of failing.
impl Drop for EngineHandle {
    fn drop(&mut self) {
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
        // Same gap, for v3 file transfers: each one runs in its own task
        // (and its data connection in another), tracked in the transfer
        // maps rather than `tasks` or `connection_handles`.
        transfer::shut_down_transfers(&self.state);
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
    /// What each peer announced as its `protocol_version` on its most
    /// recent connection — read by `send_file` to decide whether it can
    /// offer a v3 data connection. Kept after a disconnect (it's only ever
    /// read for a connected peer, and a reconnect overwrites it).
    peer_protocol_versions: Mutex<HashMap<String, u32>>,
    /// Notified (`notify_waiters`) whenever a peer's mesh connection comes
    /// up, right after its `peer_senders` entry exists — a v3 transfer
    /// stalled because the mesh dropped waits on this to re-offer.
    peer_connected: Notify,
    last_programmatic_hash: Mutex<Option<String>>,
    pending_pairings: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    /// Outstanding `FileOffer`s, keyed by `transfer_id`: the peer the offer
    /// went to (a `FileAccept` from anyone else is ignored) and where its
    /// answer goes.
    pending_file_accepts: Mutex<HashMap<String, (String, oneshot::Sender<transfer::FileAcceptReply>)>>,
    /// Count of file transfers currently in flight with each peer (sending
    /// *or* receiving; a peer could in principle have one of each at
    /// once), keyed by `peer_crypto_id` — see the reader loop's ping-tick
    /// staleness check for why this exists: `CONNECTION_READ_TIMEOUT`
    /// disconnects a connection that's gone quiet, but "quiet" and "a
    /// large transfer's writes are legitimately blocked on TCP
    /// backpressure because the peer (slow disk, saturated Wi-Fi, ...)
    /// isn't draining as fast as this end is sending" look identical from
    /// `last_activity` alone — neither a read nor a write *completes*
    /// during that stall either way. A peer that's actually gone dead
    /// still gets caught: `SO_KEEPALIVE` (see `continuity-net::connection`)
    /// operates under this entirely, at the OS/TCP level, and will fail
    /// the write outright once it gives up — that's a real error, handled
    /// by the writer loop's own `break` on failure, completely
    /// independent of this staleness heuristic.
    ///
    /// In-band transfers only. A v3 transfer's data goes over its own
    /// connection, so nothing it sends ever queues ahead of this mesh
    /// connection's pings, and the staleness check stays meaningful while
    /// one runs. **Real bug fixed here**: v3 data connections used to count
    /// toward this suppression too (a separate `data_streams_in_flight`
    /// counter). A sender whose receiver vanished mid-file (new address
    /// after docking, out of range) then kept its equally dead mesh
    /// connection open for as long as the data write stayed blocked, which
    /// with unacknowledged data in flight is TCP's own retransmission limit,
    /// about 15 minutes, since keepalive probes don't run then. The
    /// receiver's reconnect from its new address was refused as "already
    /// connected" for all that time. Dropping the mesh connection mid-file
    /// costs a v3 transfer nothing: its data connection carries on by
    /// itself, and the mesh is only needed again to re-offer after an
    /// interruption, which waits for the reconnect.
    file_transfers_in_flight: Mutex<HashMap<String, u32>>,
    /// Engine-wide (not per-connection) v3 transfer state — see the
    /// `transfer` module. Incoming keyed by (peer id, transfer id), since a
    /// transfer id is only unique per sender; outgoing by transfer id.
    incoming_transfers: Mutex<HashMap<(String, String), transfer::IncomingTransfer>>,
    outgoing_transfers: Mutex<HashMap<String, transfer::OutgoingTransfer>>,
    /// Per peer, the turns for sending it files over data connections —
    /// `transfer::SENDS_PER_PEER` at a time, the rest waiting (see
    /// `transfer::send_over_data_channel`).
    outgoing_slots: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    /// Space promised to every file being received, either path — see
    /// `transfer::SpaceReservations`.
    reserved_space: transfer::SpaceReservations,
    /// Notified (`notify_waiters`) whenever an incoming transfer changes
    /// phase — a resume offer that found its transfer still attached to a
    /// dying data connection waits on this for it to let go.
    incoming_transfers_changed: Notify,
    /// Unique id for each `IncomingTransfer` entry, so a data connection's
    /// writer that finishes late never touches a *newer* entry under the
    /// same key.
    next_transfer_epoch: AtomicU64,
    test_hooks: TestHooks,
    /// One `Semaphore` per outbound transfer currently streaming, holding
    /// `SEND_WINDOW_CHUNKS` permits — `send_file`'s read loop acquires
    /// one per chunk (blocking, i.e. pausing both reading *and* sending,
    /// once the window's full) and the writer loop's `Message::FileChunk`
    /// handling returns one every time a chunk is actually written. A
    /// connection dying closes the semaphore (see `handle_connection_inner`'s
    /// cleanup) so a `send_file` task blocked waiting for room doesn't
    /// hang forever waiting on a writer that's already gone.
    file_send_windows: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    clipboard: Arc<dyn ClipboardBackend>,
    media: Arc<dyn MediaController>,
    remote_control: Arc<dyn RemoteControlHost>,
    screen_lock: Arc<dyn ScreenLockController>,
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
    /// Behind a `Mutex` since `EngineCommand::SetReceivedFilesDir` can
    /// change it while running. Read once per offer — a transfer keeps the
    /// directory it started in.
    received_files_dir: Mutex<PathBuf>,
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

    /// Marks one file transfer with `peer_crypto_id` as in flight — see
    /// `file_transfers_in_flight`'s doc comment. Must be paired with
    /// exactly one `end_file_transfer` call for the same peer, on every
    /// exit path (success, failure, or the connection just dying
    /// mid-transfer) — an unpaired call would leave that peer's
    /// staleness timeout permanently suppressed.
    fn begin_file_transfer(&self, peer_crypto_id: &str) {
        *self.file_transfers_in_flight.lock().unwrap().entry(peer_crypto_id.to_string()).or_insert(0) += 1;
    }

    fn end_file_transfer(&self, peer_crypto_id: &str) {
        let mut in_flight = self.file_transfers_in_flight.lock().unwrap();
        if let Some(count) = in_flight.get_mut(peer_crypto_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                in_flight.remove(peer_crypto_id);
            }
        }
    }

    fn has_file_transfer_in_flight(&self, peer_crypto_id: &str) -> bool {
        self.file_transfers_in_flight.lock().unwrap().contains_key(peer_crypto_id)
    }

    /// Unconditionally zeroes out `peer_crypto_id`'s in-flight count —
    /// a backstop for `handle_connection_inner`'s cleanup, not the
    /// normal way transfers end (see `end_file_transfer` for that).
    /// Covers the one gap precise per-transfer accounting can't: a
    /// connection that dies mid-write, after `send_file` already
    /// enqueued every chunk successfully but before the writer loop
    /// ever reaches (and successfully writes) `FileComplete` — nothing
    /// would otherwise call `end_file_transfer` for that send, leaking
    /// its count forever. Safe to call unconditionally on every
    /// connection teardown because this codebase never has more than
    /// one live connection per peer at a time (see `connected`'s own
    /// dedup invariant), so there's never a second, still-legitimate
    /// transfer on some other connection to the same peer that this
    /// could wrongly clear.
    fn clear_file_transfers(&self, peer_crypto_id: &str) {
        self.file_transfers_in_flight.lock().unwrap().remove(peer_crypto_id);
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
            let _ = tx.send(Message::RemoteControlEnded { session_id: session.session_id.clone(), reason: reason.clone() });
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
/// Where an inbound connection goes once its TLS handshake is done — to
/// the screen stream, a file's data connection, or the mesh — by who it
/// is (and, where that isn't enough, by its first message).
fn route_accepted(state: &Arc<SharedState>, conn: Connection, addr: SocketAddr) {
    if state.paused.load(Ordering::Relaxed) {
        tracing::debug!("dropping inbound connection from {addr}: paused");
        return;
    }
    let peer_crypto_id = match conn.peer_device_id() {
        Ok(id) => id,
        Err(e) => {
            tracing::debug!("couldn't read peer id for inbound conn from {addr}: {e}");
            return;
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
        return;
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
    //
    // Unless that peer may also be dialing a file data
    // connection (below): the two could arrive in
    // either order, so then the screen stream is told
    // apart by its first message like everything else.
    let may_dial_data = transfer::may_dial_data_connection(&state, &peer_crypto_id);
    if !may_dial_data && state.pending_screen_stream_peers.lock().unwrap().remove(&peer_crypto_id) {
        spawn_screen_stream_connection(&state, conn, peer_crypto_id, addr, None);
        return;
    }
    // A v3 file data connection can't be told apart by
    // identity alone the way a screen stream is: the
    // same peer can dial this device for its mesh
    // connection at any moment too — and is especially
    // likely to while one of its transfers waits to
    // resume after the mesh dropped. So while a
    // transfer from this peer exists at all, its
    // connections are read first and sorted by their
    // first message (`transfer::route_inbound`); every
    // other inbound connection still goes straight to
    // `handle_connection`, unread.
    if may_dial_data {
        let conn_state = state.clone();
        tokio::spawn(async move {
            transfer::route_inbound(conn, conn_state, peer_crypto_id, addr).await;
        });
        return;
    }
    let conn_state = state.clone();
    let id_for_registry = peer_crypto_id.clone();
    let join = tokio::spawn(async move {
        if let Err(e) = handle_connection(conn, conn_state, peer_crypto_id, None).await {
            tracing::debug!("inbound connection from {addr} ended: {e}");
        }
    });
    state
        .connection_handles
        .lock()
        .unwrap()
        .insert(id_for_registry, join.abort_handle());
}

/// Runs `handle_screen_stream_connection` in its own task, registered so
/// ending the session can abort it. `first` is its handshake, if
/// `transfer::route_inbound` already read it.
fn spawn_screen_stream_connection(state: &Arc<SharedState>, conn: Connection, peer_crypto_id: String, addr: SocketAddr, first: Option<Message>) {
    let conn_state = state.clone();
    let id_for_registry = peer_crypto_id.clone();
    let join = tokio::spawn(async move {
        if let Err(e) = handle_screen_stream_connection(conn, conn_state, peer_crypto_id, first).await {
            tracing::debug!("screen-stream connection from {addr} ended: {e}");
        }
    });
    state.remote_control_stream_handles.lock().unwrap().insert(id_for_registry, join.abort_handle());
}

async fn handle_screen_stream_connection(
    mut conn: Connection,
    state: Arc<SharedState>,
    peer_crypto_id: String,
    first: Option<Message>,
) -> anyhow::Result<()> {
    let first = match first {
        Some(message) => message,
        None => read_message(&mut conn).await?,
    };
    let Message::ScreenStreamHandshake { session_id } = first else {
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
    // gets this far in that case. When the controlled side stops on its
    // own (its capture ended — say, someone pressed "Stop sharing" on
    // that desktop), it explains why in a `RemoteControlEnded` sent over
    // the *mesh* connection — a different TCP connection, which can lose
    // the race to this stream's close. A short grace period lets that
    // explanation land first; its handling ends the session and aborts
    // this task mid-sleep. Only if nothing arrives is this "unexpected".
    tokio::time::sleep(SCREEN_STREAM_END_GRACE).await;
    end_remote_control_session(&state, &peer_crypto_id, Some("screen stream ended unexpectedly".to_string()), true);
    result
}

/// Actually accepts a remote-control request and starts serving it:
/// enforces the one-active-`Controlled`-session-at-a-time rule, starts
/// capture, resolves the peer's dial-back address, and — if all of that
/// succeeds — tells the peer, records the session, and spawns the
/// screen-stream push task; each failure point declines with a specific
/// reason instead. Shared by two call sites with the exact same accept
/// logic: a human clicking "Allow" (`EngineCommand::RespondToRemoteControlRequest`
/// above) and a repeat request from a peer already remembered via
/// `TrustStore::is_remote_control_allowed` (the accept loop's handling of
/// `Message::RemoteControlRequest`) — the only difference between them is
/// *whether* a prompt was shown first, never what accepting itself does.
fn accept_remote_control_session(state: &Arc<SharedState>, peer_crypto_id: String, session_id: String) {
    let Some(tx) = state.peer_senders.lock().unwrap().get(&peer_crypto_id).cloned() else {
        return;
    };

    // This device's keyboard/mouse/screen are one shared physical
    // resource — two different peers controlling it at once would mean
    // both fighting over the same cursor, and `RemoteControlHost`'s
    // capture start/stop isn't session-scoped, just a single on/off
    // switch. Accepting a second Controlled-role session while one's
    // already active would silently corrupt that shared state (the
    // first session's eventual `stop_capture` would kill the second
    // one's stream too) — declined outright instead.
    let already_controlled =
        state.remote_control_sessions.lock().unwrap().values().any(|s| s.role == RemoteControlRole::Controlled);
    if already_controlled {
        let _ = tx.send(Message::RemoteControlResponse { session_id: session_id.clone(), accepted: false });
        // This decline previously left both sides guessing why "accept"
        // didn't work — the requester just saw a plain decline, and this
        // device showed nothing at all. A stuck/stale session entry that
        // never got cleaned up would silently decline *every* future
        // request the exact same way, indistinguishable from an outright
        // bug — this at least makes that state visible instead of a
        // mysterious permanent "no" regardless of what's clicked.
        state.emit(SyncEvent::RemoteControlSessionEnded {
            peer_id: peer_crypto_id.clone(),
            peer_name: state.trust_store.lock().unwrap().get(&peer_crypto_id).map(|d| d.name.clone()).unwrap_or_else(|| peer_crypto_id.clone()),
            session_id,
            reason: Some("already controlling another device".to_string()),
        });
        return;
    }

    // The screen stream is dialed back to the address this side's own
    // mDNS resolved for the requester (see the decline below for why it
    // can't come from the mesh connection). A request can arrive the
    // instant a connection opens, a moment before that resolution has
    // happened — so when the address isn't known yet, wait a few seconds
    // for it rather than declining a request that would have worked. The
    // address is checked before capture starts, so a session that can't
    // be dialed back never pops up the desktop's own screen-sharing prompt.
    if let Some(addr) = state.known_addresses.lock().unwrap().get(&peer_crypto_id).copied() {
        start_controlled_session(state, peer_crypto_id, session_id, tx, addr);
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + DIAL_BACK_ADDRESS_WAIT;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let addr = state.known_addresses.lock().unwrap().get(&peer_crypto_id).copied();
            if let Some(addr) = addr {
                // Still connected? The request is moot otherwise.
                if state.peer_senders.lock().unwrap().contains_key(&peer_crypto_id) {
                    start_controlled_session(&state, peer_crypto_id, session_id, tx, addr);
                }
                return;
            }
        }
        let _ = tx.send(Message::RemoteControlResponse { session_id: session_id.clone(), accepted: false });
        // Same silent-failure shape as the `already_controlled` decline
        // above, and just as real: `known_addresses` only ever gets
        // populated by mDNS discovery, not by the mesh connection this
        // very request arrived over (deliberately — an inbound
        // connection's remote port is the peer's *ephemeral* one, not
        // one that would ever accept a dial-back; see the accept loop's
        // own comment for the bug that taught us that). If mDNS never
        // delivered this peer's real address here — most plausibly a
        // firewall dropping inbound multicast on this machine —
        // accepting would only ever end up right back here with nothing
        // to actually dial, so this declines instead of accepting into a
        // session that can never send a frame.
        state.emit(SyncEvent::RemoteControlSessionEnded {
            peer_id: peer_crypto_id.clone(),
            peer_name: state.trust_store.lock().unwrap().get(&peer_crypto_id).map(|d| d.name.clone()).unwrap_or_else(|| peer_crypto_id.clone()),
            session_id,
            reason: Some("couldn't find a network address to reach this device back on".to_string()),
        });
    });
}

/// The rest of accepting, once the address to dial the screen stream back
/// to is known: starts capture, records the session, tells the peer, and
/// spawns the screen-stream push. Re-checks the one-controlled-session
/// rule, since `accept_remote_control_session` may have waited for the
/// address in the meantime.
fn start_controlled_session(
    state: &Arc<SharedState>,
    peer_crypto_id: String,
    session_id: String,
    tx: mpsc::UnboundedSender<Message>,
    addr: SocketAddr,
) {
    let peer_name = || state.trust_store.lock().unwrap().get(&peer_crypto_id).map(|d| d.name.clone()).unwrap_or_else(|| peer_crypto_id.clone());
    let already_controlled =
        state.remote_control_sessions.lock().unwrap().values().any(|s| s.role == RemoteControlRole::Controlled);
    if already_controlled {
        let _ = tx.send(Message::RemoteControlResponse { session_id: session_id.clone(), accepted: false });
        state.emit(SyncEvent::RemoteControlSessionEnded {
            peer_id: peer_crypto_id.clone(),
            peer_name: peer_name(),
            session_id,
            reason: Some("already controlling another device".to_string()),
        });
        return;
    }

    // Accepting starts capture *before* telling the peer it succeeded —
    // if it fails (no Screen Recording permission, say), the peer hears
    // about a session that never really started instead of one that
    // silently never sends any frames.
    let Some(frames) = state.remote_control.start_capture() else {
        let _ = tx.send(Message::RemoteControlResponse { session_id: session_id.clone(), accepted: false });
        state.emit(SyncEvent::RemoteControlSessionEnded {
            peer_id: peer_crypto_id.clone(),
            peer_name: peer_name(),
            session_id,
            reason: Some("couldn't start screen capture".to_string()),
        });
        return;
    };

    state.remote_control_sessions.lock().unwrap().insert(
        peer_crypto_id.clone(),
        RemoteControlSession { session_id: session_id.clone(), role: RemoteControlRole::Controlled, active: true },
    );
    let _ = tx.send(Message::RemoteControlResponse { session_id: session_id.clone(), accepted: true });
    state.emit(SyncEvent::RemoteControlSessionStarted {
        peer_id: peer_crypto_id.clone(),
        peer_name: peer_name(),
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

/// The controlled side's half of the screen stream: waits for the first
/// frame `start_capture` produces, dials a fresh connection to the
/// controlling peer, identifies it with `ScreenStreamHandshake`, then
/// pushes every frame until the channel closes (capture stopped — the
/// engine's own `stop_capture`, or the host giving up) or a write fails
/// (the connection died). Always ends by tearing the session down —
/// there's no path back to "idle but still accepted," a stream that stops
/// is a session that's over.
///
/// **Why wait for a frame before dialing:** a host's capture can still
/// fail after `start_capture` returned a channel. On Linux the desktop
/// asks its own user to approve screen sharing first, and they can say no
/// or not be there at all (`LinuxRemoteControlHost` gives up after a
/// while). Ending such a session over the mesh connection, with the
/// host's reason, reaches the controlling device intact; the old order
/// (dial first, then wait) left it only an unexplained closed stream, and
/// before the Linux host learned to give up, a spinner that never ended.
async fn push_screen_stream(
    state: Arc<SharedState>,
    peer_crypto_id: String,
    session_id: String,
    addr: SocketAddr,
    mut frames: mpsc::Receiver<Vec<u8>>,
) {
    let Some(first_frame) = frames.recv().await else {
        let reason = state
            .remote_control
            .capture_failure_reason()
            .unwrap_or_else(|| "screen capture stopped before sending anything".to_string());
        tracing::debug!("screen capture for {peer_crypto_id} ended before its first frame: {reason}");
        end_remote_control_session(&state, &peer_crypto_id, Some(reason), true);
        return;
    };

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
    if write_frame(&mut conn, &first_frame).await.is_ok() {
        while let Some(frame) = frames.recv().await {
            if write_frame(&mut conn, &frame).await.is_err() {
                break;
            }
        }
    }
    // An *intentional* end aborts this task from outside (see
    // `end_remote_control_session`), so arriving here means capture
    // stopped on its own or the stream broke. Tell the controlling side
    // now, with whatever the host knows — this goes out over the mesh
    // connection before `conn` closes, and the controlling side gives it
    // a moment to arrive (`SCREEN_STREAM_END_GRACE`).
    end_remote_control_session(&state, &peer_crypto_id, state.remote_control.capture_failure_reason(), true);
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
        peer_protocol_versions: Mutex::new(HashMap::new()),
        peer_connected: Notify::new(),
        last_programmatic_hash: Mutex::new(None),
        pending_pairings: Mutex::new(HashMap::new()),
        pending_file_accepts: Mutex::new(HashMap::new()),
        file_transfers_in_flight: Mutex::new(HashMap::new()),
        incoming_transfers: Mutex::new(HashMap::new()),
        outgoing_transfers: Mutex::new(HashMap::new()),
        outgoing_slots: Mutex::new(HashMap::new()),
        reserved_space: transfer::SpaceReservations::default(),
        incoming_transfers_changed: Notify::new(),
        next_transfer_epoch: AtomicU64::new(1),
        test_hooks: TestHooks::default(),
        file_send_windows: Mutex::new(HashMap::new()),
        clipboard: config.clipboard,
        media: config.media,
        remote_control: config.remote_control,
        screen_lock: config.screen_lock,
        remote_control_sessions: Mutex::new(HashMap::new()),
        pending_remote_control_requests: Mutex::new(HashMap::new()),
        pending_screen_stream_peers: Mutex::new(HashSet::new()),
        remote_control_stream_handles: Mutex::new(HashMap::new()),
        events_tx,
        received_files_dir: Mutex::new(config.received_files_dir),
        paused: AtomicBool::new(false),
        shutting_down: AtomicBool::new(false),
    });

    state.emit(SyncEvent::Listening { port });
    emit_initial_unlock_permissions(&state);

    let mut tasks = Vec::new();

    tasks.push({
        let state = state.clone();
        tokio::spawn(async move {
            let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES));
            loop {
                match listener.accept_tcp().await {
                    Ok(incoming) => {
                        let addr = incoming.peer_addr();
                        // Over the cap, the connection is just dropped:
                        // more half-open ones than any real set of devices
                        // makes is someone holding sockets open.
                        let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                            tracing::debug!("dropping inbound connection from {addr}: {MAX_PENDING_HANDSHAKES} handshakes already under way");
                            continue;
                        };
                        let state = state.clone();
                        tokio::spawn(async move {
                            let handshake = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, incoming.handshake()).await;
                            drop(permit);
                            match handshake {
                                Ok(Ok(conn)) => route_accepted(&state, conn, addr),
                                Ok(Err(e)) => tracing::debug!("TLS handshake with {addr} failed: {e}"),
                                Err(_) => tracing::debug!("{addr} didn't finish its TLS handshake within {TLS_HANDSHAKE_TIMEOUT:?}, dropping it"),
                            }
                        });
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
                if state.test_hooks.auto_dial_off.load(Ordering::Relaxed) {
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
                            if let Err(e) = handle_connection(conn, conn_state, peer_id, None).await {
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
                    EngineCommand::SetReceivedFilesDir { path } => {
                        // Checked on the blocking pool — creating a
                        // directory and probing it is synchronous file
                        // I/O, possibly on a slow or network volume — but
                        // awaited here, so a command sent after this one
                        // already sees the new directory.
                        let dir = PathBuf::from(&path);
                        let probe = dir.clone();
                        let usable = tokio::task::spawn_blocking(move || transfer::check_usable_dir(&probe))
                            .await
                            .unwrap_or_else(|e| Err(e.to_string()));
                        match usable {
                            Ok(()) => {
                                tracing::info!("received files now go to {}", dir.display());
                                *state.received_files_dir.lock().unwrap() = dir;
                            }
                            Err(e) => {
                                tracing::warn!("can't use {path} for received files: {e}");
                                state.emit(SyncEvent::Error(format!("Couldn't use '{path}' for received files: {e}")));
                            }
                        }
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
                        // v3 transfers outlive their mesh connections by
                        // design (that's what lets them resume), so
                        // aborting those connections doesn't stop them —
                        // and nothing should keep streaming to or from a
                        // device this one just forgot.
                        transfer::cancel_transfers(&state, None, "cancelled: all paired devices were forgotten");
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
                        // Same reasoning as `Reset`'s cancel: a v3
                        // transfer's own data connection doesn't go down
                        // with the mesh one, and a manual disconnect means
                        // nothing more moves until the user reconnects.
                        transfer::cancel_transfers(
                            &state,
                            Some(&peer_crypto_id),
                            &format!("cancelled: disconnected from '{peer_name}'"),
                        );

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
                        transfer::cancel_transfers(
                            &state,
                            Some(&peer_crypto_id),
                            &format!("cancelled: '{peer_name}' was forgotten"),
                        );
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
                                    if let Err(e) = handle_connection(conn, conn_state, peer_crypto_id, None).await {
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

                        // A human just said yes to this specific peer —
                        // remember it so a *future* request from the same
                        // peer skips this prompt entirely (see
                        // `accept_remote_control_session`'s call site in
                        // the accept loop below, and
                        // `TrustStore::allow_remote_control`'s doc comment
                        // for the security tradeoff this accepts).
                        if let Err(e) = state.trust_store.lock().unwrap().allow_remote_control(&peer_crypto_id) {
                            tracing::warn!("couldn't persist remote-control trust for '{peer_crypto_id}': {e}");
                        }
                        accept_remote_control_session(&state, peer_crypto_id, session_id);
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
                    EngineCommand::RequestScreenLock { peer_crypto_id, action } => {
                        if let Some(tx) = state.peer_senders.lock().unwrap().get(&peer_crypto_id) {
                            let _ = tx.send(Message::ScreenLockRequest { action });
                        }
                    }
                    EngineCommand::SetRemoteControlAllowed { peer_crypto_id, allowed } => {
                        if let Err(e) = state.trust_store.lock().unwrap().set_remote_control_allowed(&peer_crypto_id, allowed) {
                            tracing::warn!("couldn't persist remote-control consent for '{peer_crypto_id}': {e}");
                            state.emit(SyncEvent::Error(format!("Couldn't save the remote control setting: {e}")));
                        }
                    }
                    EngineCommand::SetUnlockAllowed { peer_crypto_id, allowed } => {
                        let (result, peer_name, now_allowed) = {
                            let mut trust_store = state.trust_store.lock().unwrap();
                            let result = trust_store.set_unlock_allowed(&peer_crypto_id, allowed);
                            let peer_name = trust_store.get(&peer_crypto_id).map(|d| d.name.clone());
                            (result, peer_name, trust_store.is_unlock_allowed(&peer_crypto_id))
                        };
                        if let Err(e) = result {
                            tracing::warn!("couldn't persist unlock permission for '{peer_crypto_id}': {e}");
                            state.emit(SyncEvent::Error(format!("Couldn't save the remote unlock setting: {e}")));
                        }
                        // Reports what the trust store actually holds now,
                        // not what was asked for — allowing an unpaired
                        // peer is refused there, and a failed save above
                        // shouldn't leave a shell showing a checkmark the
                        // next restart would quietly drop.
                        if let Some(peer_name) = peer_name {
                            if state.screen_lock.is_available() {
                                state.emit(SyncEvent::UnlockPermissionChanged {
                                    peer_id: peer_crypto_id,
                                    peer_name,
                                    allowed: now_allowed,
                                });
                            }
                        }
                    }
                    EngineCommand::SendText { peer_crypto_id, text } => {
                        if text.is_empty() {
                            continue;
                        }
                        if let Some(tx) = state.peer_senders.lock().unwrap().get(&peer_crypto_id) {
                            let _ = tx.send(Message::ClipboardUpdate {
                                origin_device: state.my_device.id.clone(),
                                content_hash: content_hash(text.as_bytes()),
                                mime: "text/plain".to_string(),
                                data: text.into_bytes(),
                            });
                        }
                    }
                }
            }
        })
    });

    tasks.push(spawn_clipboard_watcher(state.clone()));
    tasks.push(spawn_now_playing_watcher(state.clone()));
    tasks.push(spawn_reconnect_ticker(state.clone()));
    tasks.push(spawn_network_watcher(commands_tx.clone()));
    tasks.push(transfer::spawn_expiry_sweeper(state.clone()));

    Ok(EngineHandle {
        events: events_rx,
        commands_tx,
        tasks,
        state,
    })
}

/// Tells the shell about every peer already allowed to unlock this device,
/// right at startup — see `SyncEvent::UnlockPermissionChanged` for why this
/// can't wait for each one to connect.
fn emit_initial_unlock_permissions(state: &Arc<SharedState>) {
    if !state.screen_lock.is_available() {
        return;
    }
    let allowed: Vec<(String, String)> = {
        let trust_store = state.trust_store.lock().unwrap();
        trust_store
            .unlock_allowed_devices()
            .filter_map(|id| trust_store.get(id).map(|d| (id.clone(), d.name.clone())))
            .collect()
    };
    for (peer_id, peer_name) in allowed {
        state.emit(SyncEvent::UnlockPermissionChanged { peer_id, peer_name, allowed: true });
    }
}

/// Acts on a peer's `ScreenLockRequest` and answers it. Refusals (nothing
/// to do it with, or unlock not allowed for this peer) are decided right
/// here; an allowed request runs the controller on the blocking pool,
/// since it makes synchronous D-Bus calls and then waits to see whether
/// the lock screen actually responded — none of which can hold up this
/// connection's reader loop.
fn handle_screen_lock_request(state: &Arc<SharedState>, peer: &DeviceInfo, action: ScreenLockAction) {
    let refusal = if !state.screen_lock.is_available() || (action == ScreenLockAction::Unlock && !state.screen_lock.can_unlock()) {
        Some(ScreenLockOutcome::Unsupported)
    } else if action == ScreenLockAction::Unlock && !state.trust_store.lock().unwrap().is_unlock_allowed(&peer.id) {
        Some(ScreenLockOutcome::NotAllowed)
    } else {
        None
    };
    if let Some(outcome) = refusal {
        finish_screen_lock_request(state, peer, action, outcome);
        return;
    }

    let state = state.clone();
    let peer = peer.clone();
    tokio::spawn(async move {
        let controller = state.screen_lock.clone();
        let result = tokio::task::spawn_blocking(move || match action {
            ScreenLockAction::Lock => controller.lock(),
            ScreenLockAction::Unlock => controller.unlock(),
        })
        .await;
        let outcome = match result {
            Ok(Ok(())) => ScreenLockOutcome::Done,
            Ok(Err(ScreenLockError::Unsupported(reason))) => {
                tracing::debug!("screen {action:?} for '{}' unsupported: {reason}", peer.name);
                ScreenLockOutcome::Unsupported
            }
            Ok(Err(ScreenLockError::Failed(reason))) => ScreenLockOutcome::Failed { reason },
            Err(e) => ScreenLockOutcome::Failed { reason: format!("the {action:?} attempt crashed: {e}") },
        };
        finish_screen_lock_request(&state, &peer, action, outcome);
    });
}

fn finish_screen_lock_request(state: &Arc<SharedState>, peer: &DeviceInfo, action: ScreenLockAction, outcome: ScreenLockOutcome) {
    if let Some(tx) = state.peer_senders.lock().unwrap().get(&peer.id) {
        let _ = tx.send(Message::ScreenLockResult { action, outcome: outcome.clone() });
    }
    state.emit(SyncEvent::ScreenLockRequested { peer_id: peer.id.clone(), peer_name: peer.name.clone(), action, outcome });
}

/// `first` is the connection's first message when the accept loop already
/// had to read it to tell this connection from a v3 file data connection
/// (see `transfer::route_inbound`) — `None` everywhere else.
async fn handle_connection(
    conn: Connection,
    state: Arc<SharedState>,
    peer_crypto_id: String,
    first: Option<Message>,
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

    let result = handle_connection_inner(conn, &state, &peer_crypto_id, first).await;

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
    first: Option<Message>,
) -> anyhow::Result<()> {
    let is_trusted = state.trust_store.lock().unwrap().is_trusted(peer_crypto_id);

    tracing::debug!("handle_connection_inner: peer={peer_crypto_id} is_trusted={is_trusted}");

    let mut my_device = state.my_device.clone();
    match state.test_hooks.announce_protocol_version.load(Ordering::Relaxed) {
        0 => {}
        version => my_device.protocol_version = version,
    }
    let peer = if is_trusted {
        match first {
            Some(first) => announce_and_identify_after(&mut conn, &my_device, first).await?,
            None => announce_and_identify(&mut conn, &my_device).await?,
        }
    } else {
        tracing::debug!("calling start_pairing for {peer_crypto_id}");
        let pending = match first {
            Some(first) => start_pairing_after(conn, &my_device, first).await?,
            None => start_pairing(conn, &my_device).await?,
        };
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
    if state.screen_lock.is_available() {
        let allowed = state.trust_store.lock().unwrap().is_unlock_allowed(&peer.id);
        state.emit(SyncEvent::UnlockPermissionChanged { peer_id: peer.id.clone(), peer_name: peer.name.clone(), allowed });
    }

    let (mut read_half, mut write_half) = tokio::io::split(conn);
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    state.peer_protocol_versions.lock().unwrap().insert(peer.id.clone(), peer.protocol_version);
    state
        .peer_senders
        .lock()
        .unwrap()
        .insert(peer_crypto_id.to_string(), tx.clone());
    // After `peer_senders` has the entry — a stalled v3 transfer woken by
    // this looks it up straight away to re-offer.
    state.peer_connected.notify_waiters();

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
                    // A large/slow file transfer can legitimately go
                    // `elapsed` past the timeout with neither a read nor
                    // a write *completing* — not because the peer is
                    // gone, but because a write is sitting blocked on TCP
                    // backpressure (peer's disk, or its Wi-Fi, just isn't
                    // draining as fast as this end is sending). Treating
                    // that as "dead" was a real, reproducible bug: a
                    // large file transfer disconnecting itself partway
                    // through on a merely-slow link. While a transfer's
                    // actually in flight with this peer, skip the
                    // staleness disconnect and let `SO_KEEPALIVE` (see
                    // `continuity-net::connection`) be the one to notice
                    // if the peer is *actually* gone — that surfaces as a
                    // real write error, handled by the writer loop below,
                    // independent of this check entirely.
                    if elapsed > CONNECTION_READ_TIMEOUT && !state.has_file_transfer_in_flight(peer_crypto_id) {
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
                    data_channel,
                    resume,
                }) => {
                    // The name is the peer's to choose, so it's reduced to
                    // one plain file name before anything joins it onto the
                    // received-files folder — see `safe_received_file_name`.
                    let file_name = safe_received_file_name(&file_name);
                    if data_channel && !state.test_hooks.refuse_data_channel.load(Ordering::Relaxed) {
                        // Answered from its own task (free-space check,
                        // creating the `.part` file, possibly waiting for
                        // an old data connection to let go of it), so
                        // none of that holds up this connection's reader.
                        transfer::handle_data_channel_offer(state, &tx, &peer, transfer_id, file_name, size_bytes, resume);
                    } else {
                        handle_file_offer(state, &tx, &peer, &mut receiving, transfer_id, file_name, size_bytes).await;
                    }
                }
                Ok(Message::FileAccept { transfer_id, accepted, data_channel, offset, reason }) => {
                    let waiter = {
                        let mut pending = state.pending_file_accepts.lock().unwrap();
                        // Only the peer the offer went to gets to answer it.
                        match pending.get(&transfer_id) {
                            Some((to, _)) if *to == peer.id => pending.remove(&transfer_id).map(|(_, waiter)| waiter),
                            _ => None,
                        }
                    };
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(transfer::FileAcceptReply { accepted, data_channel, offset, reason });
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
                        // (Android/iOS, or a "lite" desktop build), so
                        // auto-decline instead of surfacing a prompt for
                        // a capability that doesn't exist.
                        if let Some(tx) = state.peer_senders.lock().unwrap().get(&peer.id) {
                            let _ = tx.send(Message::RemoteControlResponse { session_id, accepted: false });
                        }
                        continue;
                    }
                    // This exact peer already got an explicit "Allow"
                    // from a human in a past session (see
                    // `TrustStore::allow_remote_control`) — skip the
                    // prompt and accept straight away, the same way a
                    // reconnect from an already-paired device skips
                    // re-pairing instead of asking again every time.
                    if state.trust_store.lock().unwrap().is_remote_control_allowed(&peer.id) {
                        accept_remote_control_session(state, peer.id.clone(), session_id);
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
                Ok(Message::RemoteControlEnded { session_id, reason }) => {
                    let matches_active =
                        state.remote_control_sessions.lock().unwrap().get(&peer.id).is_some_and(|s| s.session_id == session_id);
                    if matches_active {
                        end_remote_control_session(state, &peer.id, reason, false);
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
                Ok(Message::FileStreamHandshake { .. } | Message::FileVerified { .. }) => {
                    // Same as `ScreenStreamHandshake` just above: these
                    // only belong on a v3 file data connection.
                    tracing::debug!("ignoring a data-connection message on the mesh connection from '{}'", peer.name);
                }
                Ok(Message::ScreenLockRequest { action }) => {
                    handle_screen_lock_request(state, &peer, action);
                }
                Ok(Message::ScreenLockResult { action, outcome }) => {
                    state.emit(SyncEvent::ScreenLockResult {
                        peer_id: peer.id.clone(),
                        peer_name: peer.name.clone(),
                        action,
                        outcome,
                    });
                }
                Ok(other) => tracing::debug!("ignoring unhandled message from '{}': {other:?}", peer.name),
                // A well-framed message this version can't parse — almost
                // always a message type from a newer peer. The length
                // prefix means the whole payload is already consumed, so
                // the stream is still in sync; skipping it here (instead
                // of the old blanket `break`) is what lets a newer peer
                // add message types without disconnecting every older
                // peer that receives one (see `PROTOCOL_VERSION`).
                Err(FramingError::Json(e)) => {
                    tracing::debug!("skipping a message from '{}' this version doesn't understand: {e}", peer.name);
                }
                Err(_) => break,
            }
        }
    };

    // Per-transfer bookkeeping for outbound `FileTransferProgress` —
    // local to this connection's writer loop (mirrors `receiving` being
    // local to the reader loop) since it only ever needs to track *this
    // connection's* outbound sends. Keyed by `transfer_id`: total size
    // (learned from the `FileOffer` this loop also writes, since nothing
    // else here has it) and bytes actually written so far.
    let mut sending_totals: HashMap<String, u64> = HashMap::new();
    let mut sending_progress: HashMap<String, u64> = HashMap::new();
    let mut sending_names: HashMap<String, String> = HashMap::new();
    // v3 data-channel offers written by this loop — size and name, for the
    // rare one the receiver turns down in favour of in-band chunks.
    let mut data_channel_offers: HashMap<String, (u64, String)> = HashMap::new();

    let writer = async {
        while let Some(msg) = rx.recv().await {
            // `write_message` only borrows `msg`, so it's still available
            // afterward — inspecting it post-write (not pre-) matters
            // here: everything below reacts to what was *actually*
            // written, not what was merely about to be attempted.
            // In-band offers only: a v3 data-channel transfer's data never
            // passes through this loop, and its own task reports its end —
            // tracking it here would fail it at this connection's teardown
            // even though it's meant to survive (and resume past) exactly
            // that. A data-channel offer is only remembered on the side,
            // in case the receiver answers it with "in-band, please" and
            // its `FileChunk`s turn up here after all (see below).
            match &msg {
                Message::FileOffer { transfer_id, size_bytes, file_name, data_channel: false, .. } => {
                    sending_totals.insert(transfer_id.clone(), *size_bytes);
                    sending_names.insert(transfer_id.clone(), file_name.clone());
                }
                Message::FileOffer { transfer_id, size_bytes, file_name, data_channel: true, .. } => {
                    data_channel_offers.insert(transfer_id.clone(), (*size_bytes, file_name.clone()));
                }
                Message::FileChunk { transfer_id, .. } if !sending_totals.contains_key(transfer_id) => {
                    if let Some((size_bytes, file_name)) = data_channel_offers.remove(transfer_id) {
                        sending_totals.insert(transfer_id.clone(), size_bytes);
                        sending_names.insert(transfer_id.clone(), file_name);
                    }
                }
                _ => {}
            }
            if write_message(&mut write_half, &msg).await.is_err() {
                break;
            }
            *last_activity.lock().unwrap() = std::time::Instant::now();

            match &msg {
                Message::FileChunk { transfer_id, data, .. } => {
                    // Returns the permit `send_file`'s read loop acquired
                    // for this exact chunk — see `SEND_WINDOW_CHUNKS`'s
                    // doc comment. Unconditional (not folded into the
                    // progress-tracking `let else` below it): every
                    // written `FileChunk` consumed exactly one permit
                    // regardless of whether this loop happens to still
                    // have progress-tracking state for its transfer.
                    if let Some(window) = state.file_send_windows.lock().unwrap().get(transfer_id) {
                        window.add_permits(1);
                    }
                    // What actually reflects real wire progress:
                    // `send_file`'s own chunk loop only *enqueues* into
                    // this unbounded channel, which (for a large file)
                    // returns almost instantly regardless of real
                    // transfer speed — emitting progress there would
                    // jump straight to "100%" the moment sending starts,
                    // not track the real, possibly minutes-long drain.
                    // This loop is where bytes actually leave the
                    // process.
                    let Some(total_bytes) = sending_totals.get(transfer_id).copied() else {
                        continue;
                    };
                    let sent = sending_progress.entry(transfer_id.clone()).or_insert(0);
                    let previous = *sent;
                    *sent += data.len() as u64;
                    let sent = *sent;
                    // Fires the first time `sent` crosses each
                    // `PROGRESS_EVENT_BYTES` boundary (or on the final
                    // chunk) rather than on every write — see
                    // `PROGRESS_EVENT_BYTES`'s doc comment.
                    if sent >= total_bytes || previous / PROGRESS_EVENT_BYTES != sent / PROGRESS_EVENT_BYTES {
                        state.emit(SyncEvent::FileTransferProgress {
                            transfer_id: transfer_id.clone(),
                            bytes_transferred: sent,
                            total_bytes,
                            direction: FileTransferDirection::Sending,
                        });
                    }
                }
                Message::FileComplete { transfer_id, .. } => {
                    // The *real* end of an outbound transfer, as far as
                    // `file_transfers_in_flight` cares — see the
                    // enqueue-vs-actually-written reasoning above; ending
                    // the marker in `send_file`'s own loop instead would
                    // leave the real drain window unprotected, exactly
                    // the window a slow/backpressured send needs
                    // covered. See `file_transfers_in_flight`'s doc
                    // comment.
                    state.end_file_transfer(peer_crypto_id);
                    state.file_send_windows.lock().unwrap().remove(transfer_id);
                    sending_totals.remove(transfer_id);
                    sending_progress.remove(transfer_id);
                    let file_name = sending_names.remove(transfer_id).unwrap_or_else(|| transfer_id.clone());
                    // The *real* "sent" signal — see `send_file`'s own
                    // (deliberately absent) `FileSent` emission for why
                    // this can't just happen there instead.
                    state.emit(SyncEvent::FileSent { transfer_id: transfer_id.clone(), file_name, to_name: peer.name.clone() });
                }
                _ => {}
            }
        }
    };

    // Whichever finishes first — the reader deciding the connection is
    // dead, or the writer hitting a write error — the other is dropped
    // right along with it, taking its half of the split stream with it.
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }

    // `receiving` is local to this function — without this, a transfer
    // that was still in progress when the connection died just vanished:
    // no `FileTransferFailed`, and an orphaned partial file left sitting
    // in the destination folder looking like a real, complete download.
    for (transfer_id, rf) in receiving.drain() {
        state.emit(SyncEvent::FileTransferFailed {
            transfer_id,
            reason: "connection closed mid-transfer".into(),
        });
        let _ = tokio::fs::remove_file(&rf.path).await;
    }
    // Same idea, for the writer's own per-transfer bookkeeping: any
    // transfer_id still here never reached a successfully-written
    // `FileComplete`, so nothing else has reported or cleaned it up yet.
    // Closing the send window also unblocks a `send_file` task that
    // might still be blocked in `window.acquire()` waiting for room a
    // now-dead writer will never make — without this it would hang
    // forever instead of failing the transfer.
    for transfer_id in sending_names.into_keys() {
        if let Some(window) = state.file_send_windows.lock().unwrap().remove(&transfer_id) {
            window.close();
        }
        state.emit(SyncEvent::FileTransferFailed {
            transfer_id,
            reason: "connection closed mid-transfer".into(),
        });
    }
    // Backstop for `file_transfers_in_flight` specifically (a separate
    // counter from the cleanup above) covering both directions at once
    // — see `clear_file_transfers`'s doc comment for why a wholesale
    // removal is safe here.
    state.clear_file_transfers(&peer.id);

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
    /// Bytes still to come, counted against new offers while this exists
    /// (see `transfer::SpaceReservations`).
    reserved: Arc<AtomicU64>,
    /// Who this transfer is with — `handle_file_chunk`/`handle_file_complete`
    /// need it to call `end_file_transfer`, and neither gets a `peer`
    /// parameter of their own (just `transfer_id`/`data`), so it rides
    /// along on the struct instead of threading an extra argument through
    /// every call site.
    peer_crypto_id: String,
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
    // A v3 sender that couldn't open its data connection falls back to
    // this path by re-offering the same transfer in-band — drop whatever
    // was set up for the data connection first (an empty `.part` file).
    // The user already saw this transfer start, so it isn't announced twice.
    let already_announced = transfer::discard_for_in_band_fallback(state, &peer.id, &transfer_id);

    // No size cap any more (there used to be a 500 MB auto-accept limit):
    // the only reason to refuse a paired device's file is that it can't
    // fit. The reason only reaches a v3 sender — older ones ignore the
    // field and report a plain decline.
    let dir = state.received_files_dir.lock().unwrap().clone();
    let reserved = match state.reserved_space.reserve(&dir, &state.my_device.name, size_bytes) {
        Ok(reserved) => reserved,
        Err(reason) => {
            tracing::warn!("refusing '{file_name}' from '{}': {reason}", peer.name);
            let _ = tx.send(Message::FileAccept {
                transfer_id,
                accepted: false,
                data_channel: false,
                offset: 0,
                reason: Some(reason),
            });
            return;
        }
    };

    let path = unique_destination_path(&dir, &file_name);
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
                data_channel: false,
                offset: 0,
                reason: Some(format!("{} couldn't create the file: {e}", state.my_device.name)),
            });
            return;
        }
    };

    if !already_announced {
        state.emit(SyncEvent::FileReceiving {
            transfer_id: transfer_id.clone(),
            from_name: peer.name.clone(),
            file_name: file_name.clone(),
            size_bytes,
        });
    }

    receiving.insert(
        transfer_id.clone(),
        ReceivingFile {
            file,
            hasher: IncrementalHash::new(),
            path,
            file_name,
            written: 0,
            expected_size: size_bytes,
            reserved,
            peer_crypto_id: peer.id.clone(),
        },
    );
    state.begin_file_transfer(&peer.id);

    let _ = tx.send(Message::FileAccept {
        transfer_id,
        accepted: true,
        data_channel: false,
        offset: 0,
        reason: None,
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
        let rf = receiving.remove(transfer_id).expect("just matched above");
        state.end_file_transfer(&rf.peer_crypto_id);
        return;
    }
    rf.hasher.update(&data);
    let previous_written = rf.written;
    rf.written += data.len() as u64;
    rf.reserved.store(rf.expected_size.saturating_sub(rf.written), Ordering::Relaxed);
    // Same throttling as the sending side's writer-loop hook — one event
    // per `PROGRESS_EVENT_BYTES` crossed (or the final chunk), not one
    // per chunk. Unlike sending, there's no enqueue-vs-actually-written
    // gap to worry about here: this function only ever runs once a
    // `FileChunk` has genuinely been read off the wire.
    if rf.written >= rf.expected_size || previous_written / PROGRESS_EVENT_BYTES != rf.written / PROGRESS_EVENT_BYTES {
        state.emit(SyncEvent::FileTransferProgress {
            transfer_id: transfer_id.to_string(),
            bytes_transferred: rf.written,
            total_bytes: rf.expected_size,
            direction: FileTransferDirection::Receiving,
        });
    }
}

async fn handle_file_complete(
    state: &Arc<SharedState>,
    receiving: &mut HashMap<String, ReceivingFile>,
    transfer_id: &str,
    expected_hash: String,
) {
    let Some(mut rf) = receiving.remove(transfer_id) else {
        return;
    };
    state.end_file_transfer(&rf.peer_crypto_id);
    // tokio's `File` finishes a write on its blocking pool after
    // `write_all` returns; only `flush` waits for that. **Real bug fixed
    // here**: without it, `FileReceived` could go out while the last chunk
    // was still being written, so whatever acted on it straight away — the
    // notification's Open, Android moving the file into a chosen folder,
    // a test reading it back — could get the file short.
    if let Err(e) = rf.file.flush().await {
        state.emit(SyncEvent::FileTransferFailed {
            transfer_id: transfer_id.to_string(),
            reason: format!("couldn't write the file: {e}"),
        });
        drop(rf.file);
        let _ = tokio::fs::remove_file(&rf.path).await;
        return;
    }
    let actual_hash = rf.hasher.finalize_hex();
    if actual_hash != expected_hash || rf.written != rf.expected_size {
        state.emit(SyncEvent::FileTransferFailed {
            transfer_id: transfer_id.to_string(),
            reason: "integrity check failed after transfer".into(),
        });
        let _ = tokio::fs::remove_file(&rf.path).await;
        return;
    }
    state.test_hooks.in_band_files_received.fetch_add(1, Ordering::Relaxed);
    state.emit(SyncEvent::FileReceived {
        transfer_id: transfer_id.to_string(),
        file_name: rf.file_name,
        path: rf.path.display().to_string(),
    });
}

/// Longest name, in bytes, a received file is saved under. File systems cap
/// a name at 255 bytes (or UTF-16 units), and the v3 path first writes
/// `<name>.part` and may later add ` (n)` to avoid a clash — a name right at
/// the limit on the sender would fail on both. 200 leaves room for those.
const MAX_RECEIVED_NAME_BYTES: usize = 200;

/// Reduces a peer-supplied file name to one plain name that is safe to join
/// onto the received-files folder. **Real bug fixed here**: the name from a
/// `FileOffer` used to be joined as-is, so a paired device sending
/// `../../.config/autostart/x.desktop` — or an absolute path, which
/// `Path::join` takes *instead of* the folder — could write anywhere the
/// user can, startup folders included. Only the last path component is
/// kept (split on both `/` and `\`, whatever this platform's separator),
/// control characters and the characters Windows and Android's shared
/// storage refuse (`<>:"|?*` — `:` would also open an NTFS alternate data
/// stream) become `_`, Windows' reserved device names get a `_` prefix,
/// trailing dots and spaces (which Windows silently drops) are trimmed,
/// over-long names are shortened keeping their extension, and anything left
/// empty becomes "file".
pub(crate) fn safe_received_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).find(|part| !part.is_empty()).unwrap_or("");
    let mut cleaned: String = last
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return "file".to_string();
    }

    let stem_upper = cleaned.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem_upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem_upper.starts_with("COM") || stem_upper.starts_with("LPT"))
            && stem_upper.len() == 4
            && stem_upper.as_bytes()[3].is_ascii_digit());
    if reserved {
        cleaned.insert(0, '_');
    }

    if cleaned.len() > MAX_RECEIVED_NAME_BYTES {
        let path = Path::new(&cleaned);
        let ext = path.extension().and_then(|e| e.to_str()).filter(|e| e.len() <= 20).map(|e| format!(".{e}"));
        let ext = ext.unwrap_or_default();
        let budget = MAX_RECEIVED_NAME_BYTES - ext.len();
        let stem = if ext.is_empty() { cleaned.as_str() } else { &cleaned[..cleaned.len() - ext.len()] };
        let mut cut = budget.min(stem.len());
        while !stem.is_char_boundary(cut) {
            cut -= 1;
        }
        cleaned = format!("{}{ext}", stem[..cut].trim_end());
    }
    cleaned
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

    // Neither the size nor the content is read into memory here — see
    // `SEND_WINDOW_CHUNKS`'s doc comment for why that used to be a real
    // bug (an OOM risk on memory-constrained devices for a large file).
    // `File::open` happens up front, same as before: if this device
    // can't even read the file, better to find out before bothering the
    // peer with an offer for it at all.
    let size_bytes = match tokio::fs::metadata(&path).await {
        Ok(m) => m.len(),
        Err(e) => {
            state.emit(SyncEvent::FileTransferFailed {
                transfer_id,
                reason: format!("couldn't read '{}': {e}", path.display()),
            });
            return;
        }
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            state.emit(SyncEvent::FileTransferFailed {
                transfer_id,
                reason: format!("couldn't read '{}': {e}", path.display()),
            });
            return;
        }
    };

    let Some(mut tx) = state
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

    let peer_name = state
        .trust_store
        .lock()
        .unwrap()
        .get(peer_crypto_id)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| peer_crypto_id.to_string());

    // v3: the file goes over a dedicated data connection this side dials
    // to the peer — which needs a peer that understands one, and an
    // address to dial (the same mDNS-resolved cache the screen stream's
    // dial-back uses; a mesh connection's own remote address is only
    // dialable when this side was the one that dialed it, so it isn't one
    // to rely on). Without either, it's the in-band path from the start,
    // exactly as before v3.
    let peer_version = state.peer_protocol_versions.lock().unwrap().get(peer_crypto_id).copied().unwrap_or(0);
    let force_in_band = state.test_hooks.force_in_band.load(Ordering::Relaxed);
    let has_address = || state.known_addresses.lock().unwrap().contains_key(peer_crypto_id);
    let mut has_address_now = has_address();
    if peer_version >= 3 && !force_in_band && !has_address_now {
        // Same wait as accepting a remote-control request does for its
        // dial-back address (`accept_remote_control_session`): a peer
        // that dialed this device can be connected a moment before this
        // side's own mDNS has resolved it, and a file sent in that moment
        // shouldn't miss out on the data connection for it.
        let deadline = tokio::time::Instant::now() + DIAL_BACK_ADDRESS_WAIT;
        while !has_address_now && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            has_address_now = has_address();
        }
    }
    let has_address = has_address_now;
    let mut announced = false;
    if peer_version >= 3 && has_address && !force_in_band {
        drop(file);
        let outgoing = transfer::Outgoing {
            transfer_id: transfer_id.clone(),
            peer_id: peer_crypto_id.to_string(),
            peer_name: peer_name.clone(),
            path: path.clone(),
            file_name: file_name.clone(),
            size_bytes,
        };
        let transfer::DataChannelEnd::InBand { tx: in_band_tx, reoffer, announced: was_announced } =
            transfer::send_over_data_channel(state, outgoing).await
        else {
            return;
        };
        // The data connection didn't happen: either the receiver chose
        // in-band itself, or this side couldn't open the connection before
        // any data went over it — the in-band path below, from the start.
        announced = was_announced;
        file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                state.emit(SyncEvent::FileTransferFailed {
                    transfer_id,
                    reason: format!("couldn't read '{}': {e}", path.display()),
                });
                return;
            }
        };
        if !reoffer {
            send_in_band(state, peer_crypto_id, &in_band_tx, transfer_id, file_name, size_bytes, file, &path, &peer_name, announced)
                .await;
            return;
        }
        tx = in_band_tx;
    }

    let reply = transfer::offer(state, &tx, peer_crypto_id, &transfer_id, &file_name, size_bytes, false, false).await;
    match reply {
        Some(reply) if reply.accepted => {}
        reply => {
            let reason = reply
                .and_then(|r| r.reason)
                .unwrap_or_else(|| "declined, or the peer didn't respond in time".to_string());
            state.emit(SyncEvent::FileTransferFailed { transfer_id, reason });
            return;
        }
    }

    send_in_band(state, peer_crypto_id, &tx, transfer_id, file_name, size_bytes, file, &path, &peer_name, announced).await;
}

/// The in-band (pre-v3) send, once the peer has accepted: every chunk as a
/// `FileChunk` on the mesh connection. Unchanged from before v3 apart from
/// being split out of `send_file` — `announced` skips `FileSending` when a
/// v3 attempt that fell back to this already emitted it.
#[allow(clippy::too_many_arguments)]
async fn send_in_band(
    state: &Arc<SharedState>,
    peer_crypto_id: &str,
    tx: &mpsc::UnboundedSender<Message>,
    transfer_id: String,
    file_name: String,
    size_bytes: u64,
    mut file: tokio::fs::File,
    path: &Path,
    peer_name: &str,
    announced: bool,
) {
    // Marks this transfer as in flight until the *writer loop* actually
    // finishes putting `FileComplete` on the wire (see the `Message::
    // FileComplete` match arm there) — deliberately not balanced by an
    // `end_file_transfer` right here after this function's own loop below.
    // `tx.send` only enqueues into the unbounded channel; for a large
    // file that returns almost instantly regardless of real network
    // throughput, well before the writer has actually flushed any of it.
    // Ending the marker here would only cover that brief enqueue instant,
    // not the real minutes-long window afterward where the writer could
    // still be slowly draining the backlog — exactly the window this
    // exists to protect. See `file_transfers_in_flight`'s doc comment.
    state.begin_file_transfer(peer_crypto_id);
    if !announced {
        state.emit(SyncEvent::FileSending {
            transfer_id: transfer_id.clone(),
            to_name: peer_name.to_string(),
            file_name: file_name.clone(),
            size_bytes,
        });
    }

    let window = Arc::new(tokio::sync::Semaphore::new(SEND_WINDOW_CHUNKS));
    state.file_send_windows.lock().unwrap().insert(transfer_id.clone(), window.clone());

    // Every abnormal exit below (a read error, the send window closing
    // because the connection died, a chunk/`FileComplete` failing to
    // even enqueue) needs the exact same cleanup — unlike the success
    // path, which leaves ending `file_transfers_in_flight` and emitting
    // `FileSent` to the writer loop (see below), a failure here is
    // final and this function is the only thing that will ever report
    // or clean it up.
    macro_rules! fail_transfer {
        ($reason:expr) => {{
            state.end_file_transfer(peer_crypto_id);
            state.file_send_windows.lock().unwrap().remove(&transfer_id);
            state.emit(SyncEvent::FileTransferFailed { transfer_id: transfer_id.clone(), reason: $reason });
            return;
        }};
    }

    // Streams the file in `CHUNK_SIZE` windows straight from disk — never
    // more than one chunk in memory on top of whatever's currently
    // in-flight in `window` — and hashes incrementally alongside sending,
    // the same pattern the receiving side already uses, rather than
    // hashing the whole file up front the way this used to.
    let mut hasher = IncrementalHash::new();
    let mut seq: u64 = 0;
    loop {
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut filled = 0;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]).await {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => fail_transfer!(format!("error reading '{}': {e}", path.display())),
            }
        }
        if filled == 0 {
            break;
        }
        buf.truncate(filled);

        // Blocks here once `SEND_WINDOW_CHUNKS` chunks are read but not
        // yet actually written — real backpressure: reading more of the
        // file pauses along with sending, instead of racing ahead of a
        // slow peer. `Err` means the writer loop closed the window
        // because the connection died (see `handle_connection_inner`'s
        // cleanup), not that this should hang forever waiting on it.
        let Ok(permit) = window.acquire().await else {
            fail_transfer!("connection closed mid-transfer".to_string());
        };
        permit.forget();

        hasher.update(&buf);
        if tx
            .send(Message::FileChunk { transfer_id: transfer_id.clone(), seq, data: buf })
            .is_err()
        {
            // The writer side is already gone — nothing will ever write
            // (or fail to write) the `FileComplete` this would otherwise
            // wait on, so this is the one send-side path that does need
            // to fail the transfer itself rather than leaving it to the
            // writer loop.
            fail_transfer!("connection closed mid-transfer".to_string());
        }
        seq += 1;
    }

    let hash = hasher.finalize_hex();
    if tx
        .send(Message::FileComplete { transfer_id: transfer_id.clone(), content_hash: hash })
        .is_err()
    {
        // Same "writer's gone, nothing will ever balance this" case as
        // the per-chunk send failure above — just caught one message
        // later, after the very last chunk happened to get enqueued
        // successfully but the connection died before `FileComplete`
        // could follow it.
        fail_transfer!("connection closed mid-transfer".to_string());
    }
    // `FileSent`, `end_file_transfer`, and removing this transfer's send
    // window are *not* done here — `tx.send` above only enqueues
    // `FileComplete`, which (behind a large backlog of still-unwritten
    // `FileChunk`s in this unbounded channel) can return long before the
    // writer loop actually reaches it. Emitting "sent" that early was a
    // real bug: it could reach a shell before every `FileTransferProgress`
    // for the tail of the file had arrived, so a UI that removed its
    // progress row on `FileSent` would see it reappear a moment later
    // when the next (now orphaned) progress event showed up with nothing
    // left to ever remove it again. The writer loop's own `Message::
    // FileComplete` handling below does all three once it's actually
    // been written.
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
    if state.test_hooks.auto_dial_off.load(Ordering::Relaxed) {
        return;
    }
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
                    if let Err(e) = handle_connection(conn, conn_state, peer_crypto_id, None).await {
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

/// What this device announces itself as (mDNS TXT record and
/// `DeviceAnnounce`). Peers decide what to offer from it — media controls,
/// remote control and lock/unlock only make sense for a desktop — so a
/// phone must never fall through to `Linux`. It used to: Android's
/// `target_os` is `"android"`, not `"linux"`, and this only checked for
/// macOS and Windows, so every phone announced itself as a Linux desktop
/// and other phones offered lock/unlock and remote control for it.
fn detect_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else if cfg!(target_os = "android") {
        Platform::Android
    } else if cfg!(target_os = "ios") {
        Platform::Ios
    } else {
        Platform::Linux
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn received_names_cannot_leave_the_folder() {
        assert_eq!(safe_received_file_name("../../.config/autostart/x.desktop"), "x.desktop");
        assert_eq!(safe_received_file_name("/home/alice/.config/autostart/x.desktop"), "x.desktop");
        assert_eq!(safe_received_file_name(r"..\..\AppData\Roaming\Startup\x.bat"), "x.bat");
        assert_eq!(safe_received_file_name(r"C:\Windows\x.bat"), "x.bat");
        assert_eq!(safe_received_file_name("photos/"), "photos");
        for empty in ["", "/", "..", ".", "../", "  ", "...", r"\\"] {
            assert_eq!(safe_received_file_name(empty), "file", "{empty:?}");
        }
        let dir = Path::new("/tmp/received");
        for hostile in ["../x", "/etc/x", r"..\x", "a/../../b", "..", "C:x"] {
            let joined = dir.join(safe_received_file_name(hostile));
            assert_eq!(joined.parent(), Some(dir), "{hostile:?} escaped as {joined:?}");
        }
    }

    #[test]
    fn received_names_lose_characters_other_systems_refuse() {
        assert_eq!(safe_received_file_name("Meeting: notes?.txt"), "Meeting_ notes_.txt");
        assert_eq!(safe_received_file_name("a\u{0}b\nc.txt"), "a_b_c.txt");
        assert_eq!(safe_received_file_name("notes.txt. . "), "notes.txt");
        assert_eq!(safe_received_file_name("con.txt"), "_con.txt");
        assert_eq!(safe_received_file_name("LPT1"), "_LPT1");
        assert_eq!(safe_received_file_name("console.txt"), "console.txt");
        assert_eq!(safe_received_file_name("COM10.txt"), "COM10.txt");
        assert_eq!(safe_received_file_name("résumé 2026.pdf"), "résumé 2026.pdf");
        assert_eq!(safe_received_file_name(".hidden"), ".hidden");
    }

    #[test]
    fn long_received_names_are_shortened_keeping_the_extension() {
        let long = format!("{}.mp4", "動".repeat(84));
        let short = safe_received_file_name(&long);
        assert!(short.len() <= MAX_RECEIVED_NAME_BYTES, "{} bytes", short.len());
        assert!(short.ends_with(".mp4"));
        assert!(short.starts_with('動'));
        assert!(format!("{short}.part").len() <= 255);

        let no_ext = "x".repeat(300);
        assert_eq!(safe_received_file_name(&no_ext).len(), MAX_RECEIVED_NAME_BYTES);
        assert_eq!(safe_received_file_name("short.txt"), "short.txt");
    }
}

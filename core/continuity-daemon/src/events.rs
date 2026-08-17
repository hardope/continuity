use continuity_proto::{DeviceInfo, MediaCommand, NowPlayingInfo};

/// Everything a shell (CLI, tray app, mobile UI) might want to react to.
/// The engine never blocks waiting for a shell to notice one of these —
/// where a response is actually needed (pairing confirmation), the shell
/// replies asynchronously via `EngineCommand` instead.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    Listening { port: u16 },
    /// A new (untrusted) peer wants to pair. Show `code` to the user and
    /// ask them to confirm it matches what's on the peer's screen, then
    /// send back `EngineCommand::ConfirmPairing`.
    PairingRequested { peer: DeviceInfo, code: String },
    Paired { peer: DeviceInfo },
    PairingDeclined { peer_name: String },
    Connected { peer: DeviceInfo },
    Disconnected { peer_id: String, peer_name: String },
    ClipboardReceived { from_name: String },
    ClipboardBroadcast { peer_count: usize },
    FileReceiving { transfer_id: String, from_name: String, file_name: String, size_bytes: u64 },
    FileReceived { transfer_id: String, file_name: String, path: String },
    FileSent { transfer_id: String, file_name: String, to_name: String },
    FileTransferFailed { transfer_id: String, reason: String },
    Error(String),
    /// Trust store cleared and every active connection dropped, in
    /// response to `EngineCommand::Reset` — confirms it actually happened
    /// so a shell can show "All devices forgotten" rather than assuming.
    WasReset,
    /// Reflects the current pause state after `EngineCommand::SetPaused`,
    /// so a shell's own UI (tray menu label, toggle) can follow the
    /// engine's actual state instead of tracking it independently.
    PausedStateChanged { paused: bool },
    /// `EngineCommand::ReconnectPeer` had no cached address to dial — the
    /// peer has never been seen over mDNS or connected inbound since this
    /// engine started. Distinct from `Disconnected` since no connection
    /// was ever attempted.
    ReconnectFailed { peer_id: String },
    /// A connected peer's now-playing state changed (or this is the first
    /// update since connecting) — pushed unprompted by the peer, not a
    /// response to a command. `info` is the full current snapshot, not a
    /// diff.
    NowPlayingChanged { peer_id: String, peer_name: String, info: NowPlayingInfo },
    /// An untrusted (never-paired) device was seen on the network. Purely
    /// informational — the engine does *not* auto-dial or auto-pair with
    /// it; a shell shows it in a "Nearby Devices" list and the user
    /// explicitly triggers the connection with `EngineCommand::ReconnectPeer`
    /// (works the same for "connect to a never-seen device" as it does for
    /// "reconnect a previously-disconnected one" — both just need a cached
    /// address to dial). May fire more than once for the same device as
    /// mDNS re-announces it; a shell should upsert by id, not append.
    PeerDiscovered { device: DeviceInfo },
    /// A connection health heartbeat for a connected peer, so a shell can
    /// show something like "active just now" / "idle 2m" instead of just a
    /// binary connected/disconnected dot. Piggybacks on the connection's
    /// own internal keepalive ping tick (see `PING_INTERVAL` in
    /// `engine.rs`), so it fires at that same bounded rate per peer — no
    /// separate timer, no risk of flooding a host UI.
    PeerActivity { peer_id: String, seconds_since_activity: u64 },
}

/// Requests a shell makes of the engine. Sent over the channel returned by
/// `EngineHandle::commands`.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    ConfirmPairing { peer_crypto_id: String, accept: bool },
    SendFile { peer_crypto_id: String, path: String },
    /// Clears the trust store and disconnects every active peer, as if
    /// freshly installed. Every previously paired device will need to be
    /// paired again.
    Reset,
    /// Temporarily stops accepting new connections, dialing discovered
    /// peers, and syncing the clipboard (in either direction), without
    /// shutting the engine down — existing connections stay open but go
    /// idle. Toggle back with `SetPaused(false)`.
    SetPaused(bool),
    /// Drops the active connection to one specific peer without touching
    /// the trust store — unlike `Reset`, they stay paired and can be
    /// reconnected. The disconnect sticks (neither side will silently
    /// reconnect) until `ReconnectPeer` is sent for the same id.
    DisconnectPeer { peer_crypto_id: String },
    /// Dials a peer using its last-known network address — either to
    /// reconnect one previously dropped with `DisconnectPeer`, or to
    /// connect to an untrusted device for the first time after the user
    /// picks it from a `PeerDiscovered` "Nearby Devices" list (dialing an
    /// untrusted peer runs the normal pairing handshake on both ends, same
    /// as any other first connection). A no-op (well, emits
    /// `ReconnectFailed`) if the engine has never seen that peer's
    /// address — e.g. it hasn't been on the network since this engine
    /// started.
    ReconnectPeer { peer_crypto_id: String },
    /// Fire-and-forget remote-control command to a connected peer's media
    /// playback — no acknowledgement, and only acted on by a peer whose
    /// shell wires in a real `MediaController` (macOS only for now; every
    /// other platform's `NoopMediaController` silently drops it).
    SendMediaCommand { peer_crypto_id: String, command: MediaCommand },
    /// Re-issues an mDNS query right now instead of waiting for the
    /// background querier's own timer — for a manual "Refresh" action when
    /// a device that should be nearby isn't showing up (a missed
    /// broadcast/multicast hiccup, not something this can diagnose or fix
    /// on its own, just retry sooner than the automatic interval would).
    RefreshDiscovery,
    /// Sent internally by the network-interface watcher when the host's
    /// set of local IP addresses changes (Wi-Fi network switch, VPN
    /// connect/disconnect, cable plugged/unplugged) — the same signal
    /// `RefreshDiscovery` gives a user-initiated button for, but firing on
    /// its own instead of waiting for either the user or the periodic
    /// reconnect ticker to notice. Also prompts the mDNS browse task to
    /// recreate its `ServiceDaemon`, since a `ServiceDaemon` created before
    /// a network change can keep multicasting on an interface that no
    /// longer exists instead of the new one.
    NetworkChanged,
}

use continuity_proto::{DeviceInfo, InputEventKind, MediaCommand, NowPlayingInfo};

/// Which side of an active remote-control session this device is on —
/// tells a shell which UI to show: `Controlling` gets the live screen
/// view plus input controls to send, `Controlled` gets a "you're being
/// controlled by X" indicator and an end button, nothing else (the
/// controlled side doesn't need its own screen re-rendered to itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteControlRole {
    Controlling,
    Controlled,
}

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
    /// Confirms `EngineCommand::RevokeDevice` actually happened — a shell
    /// removes the peer from its own device list off this event, not off
    /// the click that sent the command (same reasoning as `WasReset`).
    WasRevoked { peer_id: String, peer_name: String },
    /// A connected peer sent `Message::Revoked` just before closing the
    /// connection — they removed *this* device from *their* trust store.
    /// This side's own trust of them is untouched (revocation doesn't
    /// propagate automatically — see `Message::Revoked`); a shell should
    /// surface this distinctly from a plain `Disconnected` (which still
    /// fires right after) so the user understands *why* the peer dropped
    /// and stopped reconnecting, rather than reading it as a network
    /// hiccup.
    RevokedByPeer { peer_id: String, peer_name: String },

    /// A paired peer wants to remotely control this device's keyboard,
    /// mouse, and screen. Distinct from `PairingRequested` — being
    /// already paired only proves identity, this is a fresh, separate
    /// consent step every time. Show a clear prompt (who's asking, what
    /// they'd be able to do) and answer with `EngineCommand::
    /// RespondToRemoteControlRequest`. If this device's `RemoteControlHost`
    /// reports itself unavailable (`is_available() == false` — Android/
    /// iOS, Linux, or a "lite" build), the engine auto-declines before
    /// this event is ever emitted, so a shell only ever sees this when
    /// accepting is actually possible.
    RemoteControlRequested { peer_id: String, peer_name: String, session_id: String },
    /// This device's own `EngineCommand::RequestRemoteControl` was
    /// declined by the peer (or the peer has no remote-control capability
    /// at all and auto-declined).
    RemoteControlDeclined { peer_id: String, peer_name: String },
    /// A session is now active — on the controlling side, once the peer
    /// accepts; on the controlled side, immediately after the local user
    /// approves (no need to wait on a round trip to know it's "really"
    /// started, unlike the controlling side). A shell keys its remote-
    /// control UI off this event, not off the click/accept that led to
    /// it.
    RemoteControlSessionStarted { peer_id: String, peer_name: String, session_id: String, role: RemoteControlRole },
    /// A session ended — the peer explicitly ended it, this side ended
    /// it, the connection dropped mid-session, or (controlled side only)
    /// screen capture failed to start right after accepting. `reason` is
    /// `None` for a clean, expected end (either side's own `EndSession`
    /// command); `Some(...)` when it ended because something went wrong,
    /// so a shell can distinguish "you clicked End" from "this broke."
    RemoteControlSessionEnded { peer_id: String, peer_name: String, session_id: String, reason: Option<String> },
    /// One still-JPEG-encoded frame of the controlled peer's screen, for
    /// the controlling side's live view. Pushed as fast as the controlled
    /// side's `RemoteControlHost::
    /// start_capture` produces them — a shell decodes and displays each
    /// one as it arrives; there's no buffering/ordering guarantee beyond
    /// "arrives over one TCP connection," so a shell should just always
    /// show the latest one, not queue a backlog.
    ScreenFrameReceived { peer_id: String, session_id: String, frame: Vec<u8> },
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
    /// Forgets one specific paired device — unlike `Reset` (which forgets
    /// everyone), this removes just `peer_crypto_id` from the trust store.
    /// If currently connected, sends `Message::Revoked` first (best
    /// effort — the peer might not receive it if the connection is
    /// already half-dead) and then force-closes the connection
    /// immediately, rather than leaving it open until it happens to drop
    /// on its own. The peer's own trust of this device is untouched; if
    /// they're still nearby afterward, mDNS will surface them again as an
    /// untrusted `PeerDiscovered`, same as any other never-paired device.
    RevokeDevice { peer_crypto_id: String },

    /// Asks a connected, trusted peer for permission to remotely control
    /// its keyboard, mouse, and screen. The engine generates and tracks
    /// the session id internally — a shell only ever deals with
    /// `peer_crypto_id`, never the id itself, for every remote-control
    /// command below too. Emits `SyncEvent::RemoteControlDeclined` if the
    /// peer says no (or can't at all); `SyncEvent::
    /// RemoteControlSessionStarted` once accepted.
    RequestRemoteControl { peer_crypto_id: String },
    /// Answers an inbound `SyncEvent::RemoteControlRequested` from
    /// `peer_crypto_id`. Accepting starts local screen capture
    /// immediately (via this device's `RemoteControlHost`) and dials the
    /// peer back on a dedicated connection for the screen stream — if
    /// capture fails to start, the session ends right away with a reason
    /// rather than accepting into a session that can never actually send
    /// frames.
    RespondToRemoteControlRequest { peer_crypto_id: String, accept: bool },
    /// Sends one input event to inject on `peer_crypto_id`, which must
    /// have an active session with this device *in the `Controlling`
    /// role* — silently dropped otherwise (there's nothing to inject
    /// into, or this side isn't the one meant to be sending). Fire-and-
    /// forget, like `SendMediaCommand` — a real-time input stream has no
    /// use for a per-event acknowledgement.
    SendInputEvent { peer_crypto_id: String, event: InputEventKind },
    /// Ends whichever remote-control session is currently active with
    /// `peer_crypto_id`, regardless of which role this device is playing
    /// in it — the controlling side stopping cleanly and the controlled
    /// side revoking access mid-session both go through this same
    /// command. A no-op if no session is active with that peer.
    EndRemoteControlSession { peer_crypto_id: String },
}

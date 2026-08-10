use continuity_proto::DeviceInfo;

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
}

use continuity_proto::{MediaCommand, NowPlayingInfo};

/// Acts on a `Message::MediaCommand` received from a peer, and reports this
/// device's own now-playing state so the engine's watcher (see
/// `spawn_now_playing_watcher` in `engine.rs`) can broadcast it when it
/// changes. Pluggable like `ClipboardBackend` — `continuityd` has real
/// implementations for macOS (`MacMediaController`: media keys + the
/// private MediaRemote framework), Windows (`WindowsMediaController`:
/// `SendInput` + WinRT media sessions) and Linux (`LinuxMediaController`:
/// MPRIS over D-Bus); every other shell (Android, iOS, `continuityctl`)
/// wires in `NoopMediaController`.
pub trait MediaController: Send + Sync {
    fn handle(&self, command: MediaCommand);
    /// `None` means either nothing is playing right now, or this platform
    /// has no real implementation — the watcher treats both the same way
    /// (nothing to broadcast).
    fn now_playing(&self) -> Option<NowPlayingInfo>;
}

pub struct NoopMediaController;

impl MediaController for NoopMediaController {
    fn handle(&self, _command: MediaCommand) {}

    fn now_playing(&self) -> Option<NowPlayingInfo> {
        None
    }
}

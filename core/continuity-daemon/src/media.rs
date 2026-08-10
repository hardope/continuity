use continuity_proto::{MediaCommand, NowPlayingInfo};

/// Acts on a `Message::MediaCommand` received from a peer, and reports this
/// device's own now-playing state so the engine's watcher (see
/// `spawn_now_playing_watcher` in `engine.rs`) can broadcast it when it
/// changes. Pluggable like `ClipboardBackend` — only macOS has a real
/// implementation today (global media-key injection + private MediaRemote
/// framework reads, via `continuityd`'s `MacMediaController`); every other
/// shell (Windows/Linux desktop, Android, iOS, `continuityctl`) wires in
/// `NoopMediaController` until support lands there too.
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

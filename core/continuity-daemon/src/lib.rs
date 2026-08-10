//! The shared orchestration engine driving discovery, pairing, clipboard
//! sync, and file transfer — everything a shell (the `continuityctl` CLI,
//! `continuityd`'s tray app, and later the mobile FFI layer) needs, so
//! none of them have to reimplement it. Shells differ only in *how* they
//! surface `SyncEvent`s and collect the human response a pairing request
//! needs (`EngineCommand::ConfirmPairing`).

mod clipboard;
mod engine;
mod events;
mod media;

pub use clipboard::ClipboardBackend;
#[cfg(feature = "arboard-clipboard")]
pub use clipboard::ArboardClipboard;
pub use engine::{start, EngineConfig, EngineHandle};
pub use events::{EngineCommand, SyncEvent};
pub use media::{MediaController, NoopMediaController};

pub fn default_device_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown-device".to_string())
}

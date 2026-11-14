//! The desktop clipboard, for the engine: arboard for the reading and
//! writing, plus what arboard doesn't expose and syncing needs to know.
//!
//! **Concealed content stays here.** A password manager marks what it copies
//! so clipboard tools leave it alone — `org.nspasteboard.ConcealedType` on
//! macOS (and `TransientType`, for content meant to be pasted once),
//! `ExcludeClipboardContentFromMonitorProcessing` on Windows. Every one of
//! those used to go to every device like any other copy. Linux has no
//! common convention (KDE's `x-kde-passwordManagerHint` isn't readable
//! through arboard), so there everything is still synced.
//!
//! **Received text stays off the cloud clipboard** on Windows: text that
//! arrives from another device is written with `CanUploadToCloudClipboard`
//! off, so a copy made on a Mac doesn't end up in a Microsoft account
//! because a PC in the same room has cloud clipboard on. It still goes into
//! the local Win+V history.
//!
//! **A change count** where the platform keeps one (macOS's pasteboard
//! change count, Windows's clipboard sequence number): the engine's watcher
//! reads and hashes the text only when it has changed, instead of twice a
//! second whatever it holds.

use continuity_daemon::ClipboardBackend;

pub struct DesktopClipboard;

impl ClipboardBackend for DesktopClipboard {
    fn get_text(&self) -> Option<String> {
        if platform::is_concealed() {
            tracing::trace!("not reading clipboard content marked as concealed");
            return None;
        }
        arboard::Clipboard::new().ok()?.get_text().ok()
    }

    fn set_text(&self, text: &str) {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = platform::set_received_text(&mut clipboard, text);
        }
    }

    fn change_token(&self) -> Option<u64> {
        platform::change_token()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2_app_kit::NSPasteboard;

    /// What a password manager marks its copies with (an informal standard
    /// most of them follow, see nspasteboard.org), and the mark for content
    /// to be pasted once and forgotten.
    const CONCEALED_TYPES: [&str; 2] = ["org.nspasteboard.ConcealedType", "org.nspasteboard.TransientType"];

    pub fn is_concealed() -> bool {
        let pasteboard = NSPasteboard::generalPasteboard();
        let Some(types) = pasteboard.types() else { return false };
        types.iter().any(|kind| CONCEALED_TYPES.contains(&kind.to_string().as_str()))
    }

    pub fn set_received_text(clipboard: &mut arboard::Clipboard, text: &str) -> Result<(), arboard::Error> {
        clipboard.set_text(text.to_string())
    }

    pub fn change_token() -> Option<u64> {
        Some(NSPasteboard::generalPasteboard().changeCount() as u64)
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use arboard::SetExtWindows;
    use windows::core::HSTRING;
    use windows::Win32::System::DataExchange::{GetClipboardSequenceNumber, IsClipboardFormatAvailable, RegisterClipboardFormatW};

    /// The format a program adds to say clipboard tools should leave the
    /// content alone — password managers do.
    const EXCLUDE_FROM_MONITORING: &str = "ExcludeClipboardContentFromMonitorProcessing";

    pub fn is_concealed() -> bool {
        // Checking a format's presence needs no clipboard open; registering
        // a name that's already registered just returns its id.
        let format = unsafe { RegisterClipboardFormatW(&HSTRING::from(EXCLUDE_FROM_MONITORING)) };
        format != 0 && unsafe { IsClipboardFormatAvailable(format) }.is_ok()
    }

    pub fn set_received_text(clipboard: &mut arboard::Clipboard, text: &str) -> Result<(), arboard::Error> {
        clipboard.set().exclude_from_cloud().text(text.to_string())
    }

    pub fn change_token() -> Option<u64> {
        Some(unsafe { GetClipboardSequenceNumber() } as u64)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    pub fn is_concealed() -> bool {
        false
    }

    pub fn set_received_text(clipboard: &mut arboard::Clipboard, text: &str) -> Result<(), arboard::Error> {
        clipboard.set_text(text.to_string())
    }

    pub fn change_token() -> Option<u64> {
        None
    }
}

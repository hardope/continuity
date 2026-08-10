/// Platform clipboard access, pluggable so desktop shells can use the
/// built-in `arboard` backend while mobile FFI shells (Phase 2/3) supply
/// their own — `ClipboardManager` on Android and the pasteboard APIs on
/// iOS aren't accessible from a background Rust thread the way a desktop
/// OS clipboard is, so those platforms need to route through their own
/// native code either way.
pub trait ClipboardBackend: Send + Sync {
    fn get_text(&self) -> Option<String>;
    fn set_text(&self, text: &str);
}

#[cfg(feature = "arboard-clipboard")]
pub struct ArboardClipboard;

#[cfg(feature = "arboard-clipboard")]
impl ClipboardBackend for ArboardClipboard {
    fn get_text(&self) -> Option<String> {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }

    fn set_text(&self, text: &str) {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(text.to_string());
        }
    }
}

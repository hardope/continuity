package app.continuity.android

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import uniffi.continuity_ffi.ClipboardProvider

/**
 * Bridges the engine's clipboard polling to Android's `ClipboardManager`.
 *
 * Important platform constraint: since Android 10, only the foreground
 * app (or the default input method) may *read* clipboard content —
 * running as a foreground service is not enough on its own. In practice
 * this means the engine reliably picks up local clipboard changes only
 * while this app is in the foreground, mirroring the same "sync is live
 * while active" limitation documented for iOS in docs/protocol.md.
 * *Writing* the clipboard (applying an update a peer sent) is not
 * restricted the same way and works from the background service.
 */
class AndroidClipboardProvider(private val context: Context) : ClipboardProvider {
    private val clipboardManager =
        context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager

    override fun getText(): String? {
        // Real devices are less consistent than the docs about *how* an
        // unfocused-app clipboard read is denied — most return null
        // silently, but some OEM builds throw a SecurityException instead,
        // and coerceToText() on a non-text ClipData item (e.g. a content://
        // URI another app copied) can throw while resolving it. Any of
        // these, left uncaught, would cross the FFI boundary as a Rust
        // panic and permanently kill the poll loop on the other side — so
        // treat every failure here the same as "nothing to sync yet."
        return try {
            val clip = clipboardManager.primaryClip ?: return null
            if (clip.itemCount == 0) return null
            clip.getItemAt(0).coerceToText(context)?.toString()
        } catch (e: Exception) {
            null
        }
    }

    override fun setText(text: String) {
        clipboardManager.setPrimaryClip(ClipData.newPlainText("Continue", text))
    }
}

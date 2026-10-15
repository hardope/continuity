package app.continuity.android

import android.content.Context
import android.util.Base64
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import uniffi.continuity_ffi.generateIdentityDer

/**
 * Persists this device's identity (PKCS8 DER bytes) and a chosen display
 * name via Android Keystore-backed encrypted storage. `continuity-crypto`
 * has no Android backend of its own — see the FFI layer's doc comment on
 * `ContinuityEngine.start` for why storage lives here instead. The app's
 * one other setting, the folder received files are moved to (see
 * [ReceivedFiles]), is kept here too rather than in a second preferences
 * file.
 */
object SecureIdentity {
    private const val PREFS_FILE = "continuity_identity"
    private const val KEY_IDENTITY_DER = "identity_der"
    private const val KEY_DEVICE_NAME = "device_name"
    private const val KEY_RECEIVED_FILES_TREE = "received_files_tree_uri"

    // Synchronized: the service (starting the engine) and the activity
    // (showing the received-files folder) can both open this at once on a
    // first run, and two EncryptedSharedPreferences created concurrently
    // can each generate and write their own keyset — the loser's values
    // then can't be decrypted.
    @Synchronized
    private fun prefs(context: Context) = EncryptedSharedPreferences.create(
        context,
        PREFS_FILE,
        MasterKey.Builder(context).setKeyScheme(MasterKey.KeyScheme.AES256_GCM).build(),
        EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
        EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
    )

    /** Loads the stored identity, generating and persisting a new one on first run. */
    fun loadOrCreateIdentityDer(context: Context): ByteArray {
        val prefs = prefs(context)
        val existing = prefs.getString(KEY_IDENTITY_DER, null)
        if (existing != null) {
            return Base64.decode(existing, Base64.NO_WRAP)
        }
        val fresh = generateIdentityDer()
        prefs.edit().putString(KEY_IDENTITY_DER, Base64.encodeToString(fresh, Base64.NO_WRAP)).apply()
        return fresh
    }

    fun deviceName(context: Context): String {
        val prefs = prefs(context)
        return prefs.getString(KEY_DEVICE_NAME, null) ?: run {
            val default = android.os.Build.MODEL ?: "Android Device"
            prefs.edit().putString(KEY_DEVICE_NAME, default).apply()
            default
        }
    }

    fun setDeviceName(context: Context, name: String) {
        prefs(context).edit().putString(KEY_DEVICE_NAME, name).apply()
    }

    /** The SAF tree Uri chosen with "Change folder", or null for the default (Download/Continuity). */
    fun receivedFilesTreeUri(context: Context): String? =
        prefs(context).getString(KEY_RECEIVED_FILES_TREE, null)

    fun setReceivedFilesTreeUri(context: Context, uri: String?) {
        prefs(context).edit().apply { if (uri == null) remove(KEY_RECEIVED_FILES_TREE) else putString(KEY_RECEIVED_FILES_TREE, uri) }.apply()
    }
}

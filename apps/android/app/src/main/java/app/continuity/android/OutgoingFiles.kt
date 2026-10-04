package app.continuity.android

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.util.UUID

/**
 * Picked and shared files arrive as `content://` Uris, but the engine
 * streams from a real file path — so each one is copied into app-private
 * cache first. The copy runs on the IO dispatcher: the old in-activity
 * version copied on the main thread, which froze the whole UI for as long
 * as a large video took to copy.
 *
 * Each copy gets its own random subdirectory, so two different files that
 * happen to share a name (common for "IMG_0001.jpg") can't overwrite each
 * other while both are still being sent. Copies stay for a day — the
 * engine only opens the file once the other side accepts, which can take
 * a while — and [deleteStale] clears older ones out at service start.
 */
object OutgoingFiles {
    private const val DIR = "outgoing"
    private const val STALE_AFTER_MILLIS = 24L * 60 * 60 * 1000

    /** Outlives any one screen, so a send started from the file picker
     * still goes out if the user leaves the app while it's being copied. */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)

    /** Copies each Uri, skipping (not failing on) any that can't be read —
     * a revoked permission or a provider that's gone away. */
    suspend fun stage(context: Context, uris: List<Uri>): List<File> = withContext(Dispatchers.IO) {
        uris.mapNotNull { stageOne(context, it) }
    }

    /** Copies, then sends every file to every peer in [peerIds]. Problems
     * are reported as a [ContinuityStore.notice]. Fine for the app's own
     * file picker, whose read grant lasts as long as the app does; a share
     * from another app (see [ShareActivity]) must finish copying while its
     * activity is still open instead, since that grant ends with it. */
    fun stageAndSend(context: Context, uris: List<Uri>, peerIds: List<String>) {
        val appContext = context.applicationContext
        scope.launch {
            val files = stage(appContext, uris)
            val engine = EngineHolder.engine
            when {
                files.isEmpty() -> ContinuityStore.notice("Couldn't read the selected file")
                engine == null -> ContinuityStore.notice("Continuity isn't running")
                else -> {
                    for (file in files) {
                        for (peerId in peerIds) engine.sendFile(peerId, file.absolutePath)
                    }
                    if (files.size < uris.size) {
                        ContinuityStore.notice("Couldn't read ${uris.size - files.size} of the selected files")
                    }
                }
            }
        }
    }

    fun deleteStale(context: Context) {
        val cutoff = System.currentTimeMillis() - STALE_AFTER_MILLIS
        File(context.cacheDir, DIR).listFiles()?.forEach { if (it.lastModified() < cutoff) it.deleteRecursively() }
    }

    fun displayName(context: Context, uri: Uri): String? = try {
        context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { cursor ->
            val index = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            if (index >= 0 && cursor.moveToFirst()) cursor.getString(index) else null
        }
    } catch (e: Exception) {
        null
    }

    private fun stageOne(context: Context, uri: Uri): File? {
        val dir = File(File(context.cacheDir, DIR), UUID.randomUUID().toString())
        return try {
            dir.mkdirs()
            val dest = File(dir, safeName(displayName(context, uri) ?: uri.lastPathSegment))
            val copied = context.contentResolver.openInputStream(uri)?.use { input ->
                dest.outputStream().use { output -> input.copyTo(output) }
                true
            } ?: false
            if (copied) dest else null.also { dir.deleteRecursively() }
        } catch (e: Exception) {
            android.util.Log.w("OutgoingFiles", "couldn't read $uri", e)
            dir.deleteRecursively()
            null
        }
    }

    /** A display name is whatever the providing app says it is — keep it
     * from naming a path outside the staging directory. */
    private fun safeName(name: String?): String {
        val cleaned = name.orEmpty().replace('/', '_').replace('\u0000', '_').trim().trimStart('.')
        return cleaned.ifEmpty { "file" }
    }
}

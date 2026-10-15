package app.continuity.android

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.MediaScannerConnection
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.DocumentsContract
import android.webkit.MimeTypeMap
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import java.io.File
import java.io.IOException
import java.util.UUID

/** What the home screen's "Received files" row shows. */
data class ReceivedFolder(
    /** Where the engine is writing incoming files, for display — the
     * public `Download/Continuity` normally. Null until first worked out. */
    val engineDirLabel: String? = null,
    /** Set when `Download/Continuity` couldn't be used and the engine was
     * pointed at the app's own folder instead: says why, and where files
     * are going. */
    val fallbackNote: String? = null,
    /** The folder picked with "Change", if any — finished files are moved
     * there from the engine's folder. */
    val customTree: Uri? = null,
    val customTreeLabel: String? = null,
    /** Files saved by versions from before `Download/Continuity` are still
     * in the app's own folder — worth one line saying where. */
    val oldFilesLabel: String? = null,
)

/** Where a just-received file ended up, for its notification. */
data class SavedFile(
    /** A content:// Uri another app can open it through, or null if none
     * could be made. */
    val openUri: Uri?,
    /** Null when the extension doesn't map to a known type. */
    val mimeType: String?,
    /** The chosen folder's name, when the file was moved there. */
    val movedTo: String? = null,
    /** The chosen folder's name, when moving it there failed — the file
     * stayed where the engine saved it, [location]. */
    val couldNotMoveTo: String? = null,
    val location: String,
)

/**
 * Where received files go on Android.
 *
 * The engine writes into a plain directory path (it streams into a
 * `.part` file and renames it once verified), so that has to be a real
 * folder the app can write to by path. The default is the public
 * `Download/Continuity` — the old app-specific folder
 * (`Android/data/<package>/files/Continuity`) was close to impossible to
 * find, and on Android 11+ the Files app won't even show it:
 *
 * - API 26–28: needs WRITE_EXTERNAL_STORAGE, requested by MainActivity.
 * - API 29: same, together with `requestLegacyExternalStorage` in the
 *   manifest — scoped storage on Android 10 has no path access to
 *   Download/ at all, only MediaStore.
 * - API 30+: any app may create files in Download/ by path, no
 *   permission needed.
 *
 * When it can't be used (permission denied, the folder can't be created)
 * the engine gets the app-specific folder instead, and the home screen
 * says so.
 *
 * "Change folder" picks a folder through the Storage Access Framework,
 * which has no file paths at all — so the engine keeps writing into the
 * folder above, and each file is copied into the chosen one once it's
 * complete (and only then deleted from the first). A failed copy leaves
 * the file where it was, and its notification says so.
 */
object ReceivedFiles {
    private const val TAG = "ReceivedFiles"
    private const val COPY_BUFFER_BYTES = 1 shl 20
    private const val EXTERNAL_STORAGE_AUTHORITY = "com.android.externalstorage.documents"
    private const val DOWNLOADS_AUTHORITY = "com.android.providers.downloads.documents"
    private const val TREE_FLAGS = Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION

    /** As shown to the user, matching [downloadsDir]. */
    const val DOWNLOADS_LABEL = "Download/Continuity"

    private val _folder = MutableStateFlow(ReceivedFolder())
    val folder: StateFlow<ReceivedFolder> = _folder.asStateFlow()

    /** Everything here is file or provider I/O, never for the main thread;
     * this outlives any one screen so a pick isn't lost on rotation. */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    /** Guards [loaded], [appliedDir] and the read-modify-write of the
     * chosen tree, which the service and the activity both reach. */
    private val lock = Any()
    private var loaded = false

    /** The folder the running engine was last given, at start or through
     * `setReceivedFilesDir` — so a re-check only tells it when the answer
     * actually changed. */
    private var appliedDir: String? = null

    /** Where files from before `Download/Continuity` are, if any are. */
    private var oldFilesDirLabel: String? = null

    @Suppress("DEPRECATION") // The path-based API is the point; see the class comment.
    private fun downloadsDir(): File =
        File(Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS), "Continuity")

    /** The pre-`Download/Continuity` location, kept as the fallback. */
    private fun appDir(context: Context): File = context.getExternalFilesDir("Continuity") ?: context.filesDir

    /** For the foreground service, right before it starts the engine (which
     * needs the folder to exist and be writable, or it fails to start at
     * all). Blocking I/O — call it off the main thread. */
    fun dirForEngineStart(context: Context): File = synchronized(lock) {
        loadLocked(context)
        chooseLocked(context).also { appliedDir = it.path }
    }

    /** Works out again whether `Download/Continuity` can be used — the
     * storage permission may have been granted since — and points a running
     * engine at the result if that changed, so it writes where the home
     * screen says it does. Blocking I/O — call it off the main thread. */
    fun refresh(context: Context) {
        synchronized(lock) {
            loadLocked(context)
            val dir = chooseLocked(context)
            val engine = EngineHolder.engine
            if (engine != null && dir.path != appliedDir) {
                engine.setReceivedFilesDir(dir.path)
                appliedDir = dir.path
            }
        }
    }

    fun refreshAsync(context: Context) {
        val appContext = context.applicationContext
        scope.launch {
            try {
                refresh(appContext)
            } catch (e: Exception) {
                android.util.Log.w(TAG, "couldn't check the received-files folder", e)
            }
        }
    }

    /** For the "Change" button's folder picker. */
    fun chooseTree(context: Context, tree: Uri) {
        val appContext = context.applicationContext
        scope.launch {
            try {
                chooseTreeNow(appContext, tree)
            } catch (e: Exception) {
                android.util.Log.w(TAG, "couldn't use $tree for received files", e)
                ContinuityStore.notice("Couldn't use that folder: ${e.message ?: e.javaClass.simpleName}")
            }
        }
    }

    /** For "Use Downloads": back to leaving files where the engine saves them. */
    fun useDownloads(context: Context) {
        val appContext = context.applicationContext
        scope.launch {
            try {
                clearTree(appContext)
            } catch (e: Exception) {
                android.util.Log.w(TAG, "couldn't clear the received-files folder", e)
            }
        }
    }

    /**
     * Called once a file has been received completely, with the path the
     * engine saved it at. With a folder chosen through "Change", moves the
     * file there; otherwise (or if that fails) has it scanned, so it shows
     * up in Files/Downloads/Gallery right away rather than whenever the
     * media scanner next gets to it. Blocking — run it on Dispatchers.IO.
     * Never throws: whatever happens, the file is somewhere, and the
     * returned [SavedFile] says where.
     */
    fun finishReceived(context: Context, path: String): SavedFile {
        val file = File(path)
        val mimeType = mimeTypeFor(file.name)
        val location = labelFor(file.parentFile ?: file)
        val current = _folder.value
        // A chosen folder that's the very one the file is in (picked while
        // the engine wrote elsewhere, and it's since moved there) needs no
        // move — one would only copy the file to "name (1).ext" beside it.
        val tree = current.customTree?.takeUnless { isFolder(it, file.parentFile) }
        var couldNotMoveTo: String? = null
        if (tree != null) {
            val label = current.customTreeLabel ?: "the chosen folder"
            try {
                val document = copyIntoTree(context, file, tree, mimeType ?: "application/octet-stream")
                if (!file.delete()) android.util.Log.w(TAG, "moved $file but couldn't delete the original")
                return SavedFile(openUri = document, mimeType = mimeType, movedTo = label, location = label)
            } catch (e: Exception) {
                android.util.Log.w(TAG, "couldn't move $file into $tree", e)
                couldNotMoveTo = label
            }
        }
        try {
            MediaScannerConnection.scanFile(context, arrayOf(file.path), arrayOf(mimeType), null)
        } catch (e: Exception) {
            android.util.Log.w(TAG, "couldn't have $file scanned", e)
        }
        return SavedFile(openUri = fileProviderUri(context, file), mimeType = mimeType, couldNotMoveTo = couldNotMoveTo, location = location)
    }

    private fun mimeTypeFor(fileName: String): String? {
        val extension = fileName.substringAfterLast('.', "").lowercase()
        if (extension.isEmpty()) return null
        return MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension)
    }

    /** Routed through FileProvider since handing another app a raw
     * file:// Uri throws FileUriExposedException on API 24+ — every folder
     * the engine can be given is listed in res/xml/file_paths.xml. */
    private fun fileProviderUri(context: Context, file: File): Uri? = try {
        FileProvider.getUriForFile(context, "${context.packageName}.fileprovider", file)
    } catch (e: Exception) {
        // IllegalArgumentException: no root in file_paths.xml covers it.
        android.util.Log.w(TAG, "no content Uri for $file", e)
        null
    }

    /** Creates a document in [tree] and streams [file] into it — under a
     * temporary "<name>.part" name, renamed to the real one only once all
     * of it is in, where the provider can rename. **Real bug fixed here**:
     * it was written under its final name from the start, so the process
     * being killed partway through a big file (Quit, or the system) left a
     * truncated copy in the chosen folder that looked complete, beside the
     * whole one still in [DOWNLOADS_LABEL]. On any failure the half-written
     * document is deleted again (best effort) and the exception rethrown,
     * leaving [file] itself untouched. */
    private fun copyIntoTree(context: Context, file: File, tree: Uri, mimeType: String): Uri {
        val resolver = context.contentResolver
        val parent = DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree))
        // The real type even for the temporary name, for providers that
        // keep the type a document was created with. (Some add the type's
        // extension to the name; it's renamed anyway.)
        val partial = DocumentsContract.createDocument(resolver, parent, mimeType, "${file.name}.part")
            ?: throw IOException("the folder didn't accept a new file")
        val renames = supportsRename(context, partial)
        val document = if (renames) {
            partial
        } else {
            // Nowhere to rename it later: straight under its own name, as
            // before.
            deleteQuietly(context, partial)
            DocumentsContract.createDocument(resolver, parent, mimeType, file.name)
                ?: throw IOException("the folder didn't accept a new file")
        }
        try {
            val copied = resolver.openOutputStream(document, "w")?.use { output ->
                file.inputStream().use { input -> input.copyTo(output, COPY_BUFFER_BYTES) }
            } ?: throw IOException("couldn't write to the new file")
            val expected = file.length()
            if (copied != expected) throw IOException("only $copied of $expected bytes were copied")
            if (!renames) return document
            // The provider picks a unique name ("name (1).ext") if this
            // one's taken, the same way the engine does in its own folder.
            // No Uri back means the document kept its id.
            return DocumentsContract.renameDocument(resolver, document, file.name) ?: document
        } catch (e: Exception) {
            deleteQuietly(context, document)
            throw e
        }
    }

    private fun supportsRename(context: Context, document: Uri): Boolean = try {
        context.contentResolver.query(document, arrayOf(DocumentsContract.Document.COLUMN_FLAGS), null, null, null)?.use { cursor ->
            cursor.moveToFirst() && (cursor.getInt(0) and DocumentsContract.Document.FLAG_SUPPORTS_RENAME) != 0
        } ?: false
    } catch (e: Exception) {
        false
    }

    private fun deleteQuietly(context: Context, document: Uri) {
        try {
            DocumentsContract.deleteDocument(context.contentResolver, document)
        } catch (e: Exception) {
            android.util.Log.w(TAG, "couldn't delete the partial copy $document", e)
        }
    }

    /** Reads the chosen folder from preferences, once per process. A folder
     * whose access grant is gone (revoked in Settings, or its provider
     * uninstalled) is forgotten rather than kept: every move into it would
     * fail, and the home screen would be naming a folder files never reach. */
    private fun loadLocked(context: Context) {
        if (loaded) return
        loaded = true
        val stored = SecureIdentity.receivedFilesTreeUri(context)?.let(Uri::parse)
        val tree = stored?.takeIf { hasTreeAccess(context, it) }
        if (stored != null && tree == null) {
            android.util.Log.w(TAG, "lost access to $stored, going back to $DOWNLOADS_LABEL")
            SecureIdentity.setReceivedFilesTreeUri(context, null)
        }
        val oldDir = context.getExternalFilesDir("Continuity")
        if (oldDir?.listFiles()?.any { it.isFile && !it.name.endsWith(".part") } == true) {
            oldFilesDirLabel = labelFor(oldDir)
        }
        _folder.update { it.copy(customTree = tree, customTreeLabel = tree?.let { t -> treeLabel(context, t) }) }
    }

    /** `Download/Continuity` when it's usable, the app's own folder when not. */
    private fun chooseLocked(context: Context): File {
        val downloads = downloadsDir()
        val problem = when {
            needsStoragePermission(context) -> "storage permission is off"
            !usable(downloads) -> "it couldn't be created"
            else -> null
        }
        val dir = if (problem == null) downloads else appDir(context).also { it.mkdirs() }
        val label = labelFor(dir)
        _folder.update {
            it.copy(
                engineDirLabel = label,
                fallbackNote = problem?.let { why -> "$DOWNLOADS_LABEL can't be used ($why), so files are saved in $label instead." },
                // Only worth mentioning while new files go somewhere else.
                oldFilesLabel = oldFilesDirLabel?.takeIf { old -> old != label },
            )
        }
        return dir
    }

    private fun needsStoragePermission(context: Context): Boolean =
        Build.VERSION.SDK_INT <= Build.VERSION_CODES.Q &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.WRITE_EXTERNAL_STORAGE) != PackageManager.PERMISSION_GRANTED

    /** Creates [dir] if needed and proves a file can be created in it — the
     * same check the engine makes (`check_usable_dir`): existing, or even
     * mkdirs() succeeding, doesn't mean writable. */
    private fun usable(dir: File): Boolean = try {
        dir.mkdirs()
        dir.isDirectory && run {
            val probe = File(dir, ".continuity-write-test-${UUID.randomUUID()}")
            val created = probe.createNewFile()
            probe.delete()
            created
        }
    } catch (e: Exception) {
        android.util.Log.w(TAG, "can't write to $dir", e)
        false
    }

    /** A folder as the user would find it in a file manager — relative to
     * the storage root ("Download/Continuity",
     * "Android/data/<package>/files/Continuity") — or a plain description
     * for internal app storage, which no file manager shows. */
    @Suppress("DEPRECATION")
    private fun labelFor(dir: File): String {
        val root = Environment.getExternalStorageDirectory().path.trimEnd('/')
        return if (dir.path.startsWith("$root/")) dir.path.removePrefix("$root/") else "Continuity's private storage"
    }

    private fun chooseTreeNow(context: Context, tree: Uri) {
        // Only the folder the engine really is saving to. **Real bug fixed
        // here**: picking Download/Continuity always took this way out — but
        // while the engine is on the app's own folder instead (no storage
        // permission on Android 8–10), choosing it here is the one way to
        // get files there, and the notice said they already went there
        // while every file kept landing in the hidden folder.
        val engineDir = synchronized(lock) { appliedDir }?.let { File(it) }
        if (engineDir != null && isFolder(tree, engineDir)) {
            // Moving each file "into" the folder it's already in would only
            // copy it to "name (1).ext".
            clearTree(context)
            ContinuityStore.notice("Received files already go to ${labelFor(engineDir)}")
            return
        }
        // The picker's result carries a persistable grant; taking it is
        // what keeps access across restarts. Throws if it can't be.
        context.contentResolver.takePersistableUriPermission(tree, TREE_FLAGS)
        val label = treeLabel(context, tree)
        synchronized(lock) {
            loadLocked(context)
            val previous = _folder.value.customTree
            SecureIdentity.setReceivedFilesTreeUri(context, tree.toString())
            if (previous != null && previous != tree) releaseQuietly(context, previous)
            _folder.update { it.copy(customTree = tree, customTreeLabel = label) }
        }
        ContinuityStore.notice("Received files will be moved to $label")
    }

    private fun clearTree(context: Context) {
        synchronized(lock) {
            loadLocked(context)
            val previous = _folder.value.customTree ?: return
            SecureIdentity.setReceivedFilesTreeUri(context, null)
            releaseQuietly(context, previous)
            _folder.update { it.copy(customTree = null, customTreeLabel = null) }
        }
    }

    private fun releaseQuietly(context: Context, tree: Uri) {
        try {
            context.contentResolver.releasePersistableUriPermission(tree, TREE_FLAGS)
        } catch (e: SecurityException) {
            android.util.Log.w(TAG, "couldn't release $tree", e)
        }
    }

    private fun hasTreeAccess(context: Context, tree: Uri): Boolean =
        context.contentResolver.persistedUriPermissions.any { it.uri == tree && it.isReadPermission && it.isWritePermission }

    /** Whether [tree] is [dir] itself, as far as its provider lets that be
     * told: a folder on the phone's own storage has a tree id like
     * "primary:Download/Continuity", and one picked under the picker's
     * Downloads entry "raw:/storage/emulated/0/Download/Continuity".
     * **Real bug fixed here**: only the first was recognised, so
     * Download/Continuity picked from the Downloads entry was taken for
     * another folder, and every received file was copied into the folder
     * it was already in, as "name (1).ext". Any other provider's folder
     * (an SD card's, a cloud drive's) is never one the engine writes to by
     * path. */
    @Suppress("DEPRECATION") // The path-based API is the point; see the class comment.
    private fun isFolder(tree: Uri, dir: File?): Boolean {
        if (dir == null) return false
        val documentId = try {
            DocumentsContract.getTreeDocumentId(tree)
        } catch (e: IllegalArgumentException) {
            return false
        }
        val path = when (tree.authority) {
            EXTERNAL_STORAGE_AUTHORITY ->
                documentId.takeIf { it.startsWith("primary:") }?.let { File(Environment.getExternalStorageDirectory(), it.removePrefix("primary:")) }
            DOWNLOADS_AUTHORITY -> documentId.takeIf { it.startsWith("raw:") }?.let { File(it.removePrefix("raw:")) }
            else -> null
        } ?: return false
        return try {
            path.canonicalPath.equals(dir.canonicalPath, ignoreCase = true)
        } catch (e: IOException) {
            false
        }
    }

    /** "Documents/Phone" for a folder on the phone's own storage (whose
     * tree ids are "primary:Documents/Phone"); the folder's display name
     * from its provider for anything else (an SD card, a cloud drive). */
    private fun treeLabel(context: Context, tree: Uri): String {
        val documentId = try {
            DocumentsContract.getTreeDocumentId(tree)
        } catch (e: IllegalArgumentException) {
            return "the chosen folder"
        }
        if (tree.authority == EXTERNAL_STORAGE_AUTHORITY) {
            val path = documentId.substringAfter(':', "")
            if (path.isNotEmpty()) return path
        }
        val displayName = try {
            val document = DocumentsContract.buildDocumentUriUsingTree(tree, documentId)
            context.contentResolver.query(document, arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME), null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) cursor.getString(0) else null
            }
        } catch (e: Exception) {
            null
        }
        return displayName?.takeIf { it.isNotBlank() } ?: "the chosen folder"
    }
}

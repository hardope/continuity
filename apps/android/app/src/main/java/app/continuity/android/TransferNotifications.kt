package app.continuity.android

import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.SystemClock
import androidx.core.app.NotificationCompat
import app.continuity.android.ui.formatBytes
import uniffi.continuity_ffi.FfiFileTransferDirection

/**
 * One system notification per file transfer, sending or receiving, on its
 * own high-importance channel ([CHANNEL_ID]) so it shows as a heads-up
 * popup — the old one-line "Received 'x'" on the default-importance events
 * channel went straight to the shade, easy to never notice (issue #2).
 * A channel's importance can't be raised once created, hence a new one
 * rather than changing that.
 *
 * - Start: "Receiving 'x'" / "Sending 'x'" pops up once.
 * - Then a determinate progress bar, updated from `FileTransferProgress`
 *   at most twice a second per transfer and four times a second overall —
 *   the system drops a package's notification *updates* beyond about five
 *   a second, so faster wouldn't show anyway and would crowd out the
 *   updates that matter. `setOnlyAlertOnce`, so updates don't pop up again;
 *   ongoing, so it can't be swiped away mid-transfer.
 * - Stalled: "Waiting for 'device' to reconnect…" until progress resumes.
 * - Done or failed: the progress notification is cancelled and a new one
 *   posted, which alerts again. New rather than an update of the same one
 *   because that rate limit only ever sheds updates: under several busy
 *   transfers an update saying "Received" could be the one dropped, and
 *   that's the one notification that has to arrive.
 *
 * Both are keyed by tag (derived from the transfer id) with a fixed id, so
 * a transfer always maps to the same notification without hashing ids
 * into a range shared with other notifications.
 *
 * Called from the engine's callback thread and from the service's IO
 * coroutines both; [transfers] (and the [Transfer]s in it) is only
 * touched under its own lock. With the app's notifications turned off
 * (POST_NOTIFICATIONS denied) nothing is posted — and a channel the user
 * blocked, the system drops by itself. The in-app Transfers card is the
 * live view either way.
 */
class TransferNotifications(private val context: Context) {

    private class Transfer(
        val direction: FfiFileTransferDirection,
        val fileName: String,
        val peerName: String,
        var totalBytes: Long,
    ) {
        var bytesDone = 0L
        var stalled = false
        var savingTo: String? = null
        var lastPostedAt = 0L
    }

    private val manager = context.getSystemService(NotificationManager::class.java)
    private val transfers = HashMap<String, Transfer>()
    private var lastProgressPostedAt = 0L

    /** Tapping an in-progress (or failed) transfer brings up the app, where
     * the Transfers card shows it — the launcher's own intent, so an
     * already-open app comes to the front instead of opening twice. */
    private val openApp: PendingIntent? by lazy {
        context.packageManager.getLaunchIntentForPackage(context.packageName)?.let { intent ->
            PendingIntent.getActivity(context, 0, intent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        }
    }

    fun started(transferId: String, direction: FfiFileTransferDirection, fileName: String, peerName: String, totalBytes: Long) {
        synchronized(transfers) { transfers[transferId] = Transfer(direction, fileName, peerName, totalBytes) }
        updateActive(transferId) { true }
    }

    fun progress(transferId: String, bytesDone: Long, totalBytes: Long) {
        val now = SystemClock.elapsedRealtime()
        updateActive(transferId) { transfer ->
            val resumed = transfer.stalled
            transfer.bytesDone = bytesDone
            transfer.totalBytes = totalBytes
            transfer.stalled = false
            // Coming back from a stall always shows at once — "Waiting to
            // reconnect" shouldn't linger after it has.
            val due = resumed ||
                (now - transfer.lastPostedAt >= PER_TRANSFER_INTERVAL_MS && now - lastProgressPostedAt >= OVERALL_INTERVAL_MS)
            if (due) {
                transfer.lastPostedAt = now
                lastProgressPostedAt = now
            }
            due
        }
    }

    fun stalled(transferId: String) {
        updateActive(transferId) { transfer ->
            transfer.stalled = true
            true
        }
    }

    /** Between "received" and the file landing in the folder chosen with
     * "Change folder" — the move is a full copy, long enough for a big file
     * that the notification should say what's going on. */
    fun saving(transferId: String, folderLabel: String) {
        updateActive(transferId) { transfer ->
            transfer.savingTo = folderLabel
            true
        }
    }

    fun received(transferId: String, fileName: String, saved: SavedFile) {
        val transfer = finish(transferId)
        if (!manager.areNotificationsEnabled()) return
        val from = transfer?.let { "From '${it.peerName}'" }
        val detail = when {
            saved.movedTo != null -> "Saved to ${saved.movedTo}"
            saved.couldNotMoveTo != null -> "Couldn't move it to ${saved.couldNotMoveTo}, so it's in ${saved.location}"
            else -> "Saved to ${saved.location}"
        }
        val text = listOfNotNull(from, detail).joinToString(" · ")
        val open = saved.openUri?.let { viewIntent(transferId, it, saved.mimeType) }
        val builder = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle("Received '$fileName'")
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setAutoCancel(true)
            .setContentIntent(open ?: openApp)
        if (open != null) builder.addAction(0, "Open", open)
        postDone(transferId, builder.build())
    }

    fun sent(transferId: String, fileName: String, toName: String) {
        finish(transferId)
        if (!manager.areNotificationsEnabled()) return
        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle("Sent '$fileName'")
            .setContentText("To '$toName'")
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setAutoCancel(true)
            .setContentIntent(openApp)
            .build()
        postDone(transferId, notification)
    }

    /** Also covers a send refused before it ever started — say, not enough
     * free space on the other side — which never had a "Sending" to
     * replace, so there's no file name to go on. */
    fun failed(transferId: String, reason: String) {
        val transfer = finish(transferId)
        if (!manager.areNotificationsEnabled()) return
        val title = when {
            transfer == null -> "File transfer failed"
            transfer.direction == FfiFileTransferDirection.RECEIVING -> "Couldn't receive '${transfer.fileName}'"
            else -> "Couldn't send '${transfer.fileName}'"
        }
        val text = reason.replaceFirstChar { it.uppercase() }
        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setAutoCancel(true)
            .setContentIntent(openApp)
            .build()
        postDone(transferId, notification)
    }

    /** Posts a finished transfer's notification, first taking down the
     * oldest finished ones beyond [MAX_DONE_SHOWN] — including any left from
     * before a restart. **Real bug fixed here**: each one stayed until
     * tapped, and Android stops showing an app's new notifications once it
     * has 50 up (the status one and any in progress count too), so in a
     * share of 60 photos, from about the 48th on neither progress nor
     * "Received" notifications appeared at all. */
    private fun postDone(transferId: String, notification: Notification) {
        try {
            manager.activeNotifications
                .filter { it.id == NOTIFICATION_ID && it.tag?.startsWith(DONE_TAG_PREFIX) == true && it.tag != doneTag(transferId) }
                .sortedBy { it.postTime }
                .dropLast(MAX_DONE_SHOWN - 1)
                .forEach { manager.cancel(it.tag, it.id) }
        } catch (e: Exception) {
            android.util.Log.w("TransferNotifications", "couldn't take down old transfer notifications", e)
        }
        manager.notify(doneTag(transferId), NOTIFICATION_ID, notification)
    }

    /** Forgets a transfer and takes its progress notification down. */
    private fun finish(transferId: String): Transfer? {
        val transfer = synchronized(transfers) { transfers.remove(transferId) }
        manager.cancel(activeTag(transferId), NOTIFICATION_ID)
        return transfer
    }

    /** Applies [change] to a known transfer under the lock and, if it says
     * the notification should be (re)posted, builds it there too — from a
     * consistent state — then posts it outside the lock. Whether
     * notifications are allowed at all is a call into the system, so it's
     * only asked once a post is actually due, not on every progress event. */
    private fun updateActive(transferId: String, change: (Transfer) -> Boolean) {
        val notification = synchronized(transfers) {
            val transfer = transfers[transferId] ?: return
            if (!change(transfer)) return
            activeNotification(transfer)
        }
        if (manager.areNotificationsEnabled()) manager.notify(activeTag(transferId), NOTIFICATION_ID, notification)
    }

    private fun activeNotification(transfer: Transfer): Notification {
        val receiving = transfer.direction == FfiFileTransferDirection.RECEIVING
        val total = transfer.totalBytes
        val done = transfer.bytesDone.coerceIn(0, total.coerceAtLeast(0))
        val savingTo = transfer.savingTo
        val stalled = transfer.stalled
        val peer = if (receiving) "From '${transfer.peerName}'" else "To '${transfer.peerName}'"
        val text = when {
            savingTo != null -> "Saving to $savingTo…"
            stalled -> "Waiting for '${transfer.peerName}' to reconnect…"
            total > 0 -> "$peer · ${formatBytes(done)} of ${formatBytes(total)}"
            else -> peer
        }
        val builder = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(if (receiving) "Receiving '${transfer.fileName}'" else "Sending '${transfer.fileName}'")
            .setContentText(text)
            .setCategory(NotificationCompat.CATEGORY_PROGRESS)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setContentIntent(openApp)
        if (savingTo != null || total <= 0) {
            builder.setProgress(0, 0, true)
        } else {
            builder.setProgress(PROGRESS_MAX, (done * PROGRESS_MAX / total).toInt(), false)
        }
        return builder.build()
    }

    private fun viewIntent(transferId: String, uri: Uri, mimeType: String?): PendingIntent {
        val intent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, mimeType ?: "*/*")
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        return PendingIntent.getActivity(
            context,
            transferId.hashCode(),
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    companion object {
        const val CHANNEL_ID = "continuity_transfers"

        /** Fixed; the tag tells transfers apart. Distinct from the service's
         * status (1) and events (1000 + hash) ids regardless. */
        private const val NOTIFICATION_ID = 2
        private const val ACTIVE_TAG_PREFIX = "transfer:"
        private const val DONE_TAG_PREFIX = "transfer-done:"
        private const val PROGRESS_MAX = 1000
        private const val PER_TRANSFER_INTERVAL_MS = 500L
        private const val OVERALL_INTERVAL_MS = 250L

        /** Finished-transfer notifications kept up at once (see
         * `postDone`): plenty to see what just arrived, well under the 50
         * an app may show. */
        private const val MAX_DONE_SHOWN = 20

        private fun activeTag(transferId: String) = ACTIVE_TAG_PREFIX + transferId
        private fun doneTag(transferId: String) = DONE_TAG_PREFIX + transferId

        /** Takes down every in-progress transfer notification. They're
         * ongoing, so the user can't swipe them away — and once the engine
         * stops (service destroyed, Quit, or a previous process that died
         * mid-transfer) nothing would ever update or end them. The finished
         * ones ("Received 'x'", with its Open action) are left alone. */
        fun cancelAllActive(context: Context) {
            val manager = context.getSystemService(NotificationManager::class.java)
            try {
                manager.activeNotifications
                    .filter { it.id == NOTIFICATION_ID && it.tag?.startsWith(ACTIVE_TAG_PREFIX) == true }
                    .forEach { manager.cancel(it.tag, it.id) }
            } catch (e: Exception) {
                android.util.Log.w("TransferNotifications", "couldn't clear transfer notifications", e)
            }
        }
    }
}

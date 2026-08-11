package app.continuity.android

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.IBinder
import android.webkit.MimeTypeMap
import androidx.core.app.NotificationCompat
import androidx.core.content.FileProvider
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import uniffi.continuity_ffi.ContinuityEngine
import uniffi.continuity_ffi.EventListener
import uniffi.continuity_ffi.FfiSyncEvent
import java.io.File

/**
 * Foreground service hosting the engine for as long as the app should be
 * reachable — Android kills background processes aggressively, so a
 * foreground service with a visible notification is the standard pattern
 * for "keep syncing while not in the foreground" (the same one KDE
 * Connect/GSConnect use). Clipboard *reading* is still foreground-only
 * regardless (see AndroidClipboardProvider); this keeps the mesh
 * connection alive and lets peers still push updates/files to this
 * device even when the app isn't on screen.
 */
class ContinuityForegroundService : Service() {

    private var multicastLock: WifiManager.MulticastLock? = null
    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())

    override fun onCreate() {
        super.onCreate()
        uniffi.continuity_ffi.initAndroidLogging()
        createNotificationChannels()
        startForeground(NOTIFICATION_ID_STATUS, statusNotification())
        acquireMulticastLock()
        // `ContinuityEngine.start` is a blocking FFI call (it waits for the
        // Rust side's tokio runtime and mDNS/TLS listener to come up) — on
        // a slow-to-initialize network stack that can take a real amount
        // of time, and Service.onCreate() runs on the main thread, so
        // calling it inline here freezes the whole app (no ANR crash, just
        // a silently frozen UI) until it finishes. Off the main thread instead.
        serviceScope.launch { startEngine() }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Sticky: if Android kills this process under memory pressure,
        // restart it (without redelivering the last intent — there's
        // nothing engine-specific in it) so sync resumes automatically.
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        serviceScope.cancel()
        EngineHolder.engine?.close()
        EngineHolder.engine = null
        multicastLock?.let { if (it.isHeld) it.release() }
        super.onDestroy()
    }

    private fun startEngine() {
        val identityDer = SecureIdentity.loadOrCreateIdentityDer(this)
        val deviceName = SecureIdentity.deviceName(this)
        val receivedDir = (getExternalFilesDir("Continuity") ?: filesDir).also { it.mkdirs() }

        // Set before starting the engine, not after — `ContinuityEngine.start`
        // doesn't return until its background thread is fully up, by which
        // point it may have already emitted (and a collector already
        // consumed) the first `Listening` event. Reading deviceId reactively
        // inside that event's handling would race against this assignment.
        EngineHolder.deviceId = uniffi.continuity_ffi.deviceIdFor(identityDer)

        val listener = object : EventListener {
            override fun onEvent(event: FfiSyncEvent) {
                EngineHolder.events.tryEmit(event)
                notifyForEvent(event)
            }
        }

        val engine = ContinuityEngine.start(
            identityDer,
            "default",
            deviceName,
            filesDir.path,
            receivedDir.path,
            AndroidClipboardProvider(this),
            listener,
        )

        EngineHolder.engine = engine
    }

    private fun acquireMulticastLock() {
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("continuity-mdns").apply {
            setReferenceCounted(true)
            acquire()
        }
    }

    private fun createNotificationChannels() {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_STATUS,
                getString(R.string.notification_channel_status),
                NotificationManager.IMPORTANCE_LOW,
            ),
        )
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_EVENTS,
                getString(R.string.notification_channel_events),
                NotificationManager.IMPORTANCE_DEFAULT,
            ),
        )
    }

    private fun statusNotification(): Notification =
        NotificationCompat.Builder(this, CHANNEL_STATUS)
            .setContentTitle(getString(R.string.foreground_notification_title))
            .setContentText(getString(R.string.foreground_notification_text))
            .setSmallIcon(R.drawable.ic_notification)
            .setOngoing(true)
            .build()

    private fun notifyForEvent(event: FfiSyncEvent) {
        // Paired/Connected/Disconnected/ClipboardReceived are routine —
        // on an active mesh they'd fire constantly, and a notification for
        // each one is just noise. They're still logged to the in-app
        // activity feed (see MainActivity), just not pushed as a system
        // notification. Reset/pause are self-initiated from within the
        // app, so there's no one to notify either.
        val text = when (event) {
            is FfiSyncEvent.PairingRequested ->
                "Pairing request from '${event.peer.name}' — open Continuity to confirm"
            is FfiSyncEvent.PairingDeclined -> "Pairing with '${event.peerName}' was declined"
            is FfiSyncEvent.FileReceiving -> "Receiving '${event.fileName}' from '${event.fromName}'..."
            is FfiSyncEvent.FileReceived -> {
                notifyFileReceived(event.fileName, event.path)
                return
            }
            is FfiSyncEvent.FileSent -> "Sent '${event.fileName}' to '${event.toName}'"
            is FfiSyncEvent.FileTransferFailed -> "File transfer failed: ${event.reason}"
            is FfiSyncEvent.Error -> "Error: ${event.message}"
            is FfiSyncEvent.Paired,
            is FfiSyncEvent.Connected,
            is FfiSyncEvent.Disconnected,
            is FfiSyncEvent.ClipboardReceived,
            is FfiSyncEvent.WasReset,
            is FfiSyncEvent.PausedStateChanged,
            is FfiSyncEvent.Listening,
            is FfiSyncEvent.ClipboardBroadcast,
            is FfiSyncEvent.ReconnectFailed,
            is FfiSyncEvent.NowPlayingChanged,
            is FfiSyncEvent.PeerDiscovered,
            -> return
        }

        val notification = NotificationCompat.Builder(this, CHANNEL_EVENTS)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setSmallIcon(R.drawable.ic_notification)
            .setAutoCancel(true)
            .build()

        getSystemService(NotificationManager::class.java)
            .notify(NOTIFICATION_ID_EVENT_BASE + text.hashCode(), notification)
    }

    /// A received file is worth acting on immediately, so it gets an
    /// "Open" action instead of just announcing itself — routes through
    /// FileProvider since handing another app a raw file:// Uri throws
    /// FileUriExposedException on API 24+.
    private fun notifyFileReceived(fileName: String, path: String) {
        val file = File(path)
        val uri = FileProvider.getUriForFile(this, "$packageName.fileprovider", file)
        val extension = fileName.substringAfterLast('.', "")
        val mimeType = MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension) ?: "*/*"

        val viewIntent = Intent(Intent.ACTION_VIEW).apply {
            setDataAndType(uri, mimeType)
            addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        val pendingIntent = PendingIntent.getActivity(
            this,
            path.hashCode(),
            viewIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        val notification = NotificationCompat.Builder(this, CHANNEL_EVENTS)
            .setContentTitle(getString(R.string.app_name))
            .setContentText("Received '$fileName'")
            .setSmallIcon(R.drawable.ic_notification)
            .setAutoCancel(true)
            .setContentIntent(pendingIntent)
            .addAction(0, "Open", pendingIntent)
            .build()

        getSystemService(NotificationManager::class.java)
            .notify(NOTIFICATION_ID_EVENT_BASE + fileName.hashCode(), notification)
    }

    companion object {
        private const val CHANNEL_STATUS = "continuity_status"
        private const val CHANNEL_EVENTS = "continuity_events"
        private const val NOTIFICATION_ID_STATUS = 1
        private const val NOTIFICATION_ID_EVENT_BASE = 1000
    }
}

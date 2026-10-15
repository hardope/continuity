package app.continuity.android

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.IBinder
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import uniffi.continuity_ffi.ContinuityEngine
import uniffi.continuity_ffi.EventListener
import uniffi.continuity_ffi.FfiFileTransferDirection
import uniffi.continuity_ffi.FfiSyncEvent

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
    private lateinit var transferNotifications: TransferNotifications

    override fun onCreate() {
        super.onCreate()
        uniffi.continuity_ffi.initAndroidLogging()
        createNotificationChannels()
        transferNotifications = TransferNotifications(this)
        startForeground(NOTIFICATION_ID_STATUS, statusNotification())
        // Left over from a previous process that died mid-transfer — that
        // transfer's engine state died with it.
        TransferNotifications.cancelAllActive(this)
        acquireMulticastLock()
        // `ContinuityEngine.start` is a blocking FFI call (it waits for the
        // Rust side's tokio runtime and mDNS/TLS listener to come up) — on
        // a slow-to-initialize network stack that can take a real amount
        // of time, and Service.onCreate() runs on the main thread, so
        // calling it inline here freezes the whole app (no ANR crash, just
        // a silently frozen UI) until it finishes. Off the main thread instead.
        // Staged copies of files sent earlier (see OutgoingFiles) — kept a
        // day in case a send was still waiting on the other side to accept.
        serviceScope.launch { OutgoingFiles.deleteStale(this@ContinuityForegroundService) }
        serviceScope.launch {
            try {
                startEngine()
            } catch (e: Exception) {
                // Anything here (Keystore access failing, the trust store
                // file being unreadable, the mDNS/TLS listener failing to
                // bind, ...) used to be an uncaught exception on this
                // coroutine — with no CoroutineExceptionHandler installed,
                // that propagates to the thread's default handler and
                // kills the whole process, which looks like "the app
                // launches and immediately closes" with no indication why.
                // Surface it as a normal in-app error instead.
                android.util.Log.e("ContinuityService", "engine startup failed", e)
                ContinuityStore.onEvent(
                    FfiSyncEvent.Error("Couldn't start Continuity: ${e.message ?: e.javaClass.simpleName}"),
                )
            }
        }
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
        TransferNotifications.cancelAllActive(this)
        multicastLock?.let { if (it.isHeld) it.release() }
        super.onDestroy()
    }

    private fun startEngine() {
        val identityDer = SecureIdentity.loadOrCreateIdentityDer(this)
        val deviceName = SecureIdentity.deviceName(this)
        // Download/Continuity, or the app's own folder if that can't be
        // used — see ReceivedFiles.
        val receivedDir = ReceivedFiles.dirForEngineStart(this)

        ContinuityStore.setDeviceId(uniffi.continuity_ffi.deviceIdFor(identityDer))

        val listener = object : EventListener {
            override fun onEvent(event: FfiSyncEvent) {
                ContinuityStore.onEvent(event)
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
        // The storage permission may have been granted while the engine
        // was starting (MainActivity asks for it at the same time this
        // runs), too late for `receivedDir` above but before there was an
        // engine to tell — so check once more now that there is.
        try {
            ReceivedFiles.refresh(this)
        } catch (e: Exception) {
            android.util.Log.w("ContinuityService", "couldn't re-check the received-files folder", e)
        }
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
        // High importance, so a transfer starting or finishing pops up as a
        // heads-up notification — see TransferNotifications.
        manager.createNotificationChannel(
            NotificationChannel(
                TransferNotifications.CHANNEL_ID,
                getString(R.string.notification_channel_transfers),
                NotificationManager.IMPORTANCE_HIGH,
            ).apply { description = getString(R.string.notification_channel_transfers_description) },
        )
        // A send you just started doesn't need to pop up to tell you so —
        // its progress goes here, quietly. How it ends still pops up.
        manager.createNotificationChannel(
            NotificationChannel(
                TransferNotifications.SENDING_CHANNEL_ID,
                getString(R.string.notification_channel_sending),
                NotificationManager.IMPORTANCE_LOW,
            ).apply { description = getString(R.string.notification_channel_sending_description) },
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
            // Each transfer has its own notification on its own channel,
            // start to finish — see TransferNotifications.
            is FfiSyncEvent.FileReceiving,
            is FfiSyncEvent.FileSending,
            is FfiSyncEvent.FileTransferProgress,
            is FfiSyncEvent.FileTransferStalled,
            is FfiSyncEvent.FileReceived,
            is FfiSyncEvent.FileSent,
            is FfiSyncEvent.FileTransferFailed,
            -> {
                onTransferEvent(event)
                return
            }
            is FfiSyncEvent.Error -> "Error: ${event.message}"
            // A peer forgetting *this* device is worth surfacing even when
            // the app isn't in the foreground — it explains why sync just
            // stopped, the same way PairingDeclined does above. WasRevoked
            // is the mirror of that but from this device's own deliberate
            // action (the user just tapped "Forget" in-app), so — like
            // WasReset below — there's no one who needs telling who isn't
            // already looking at the screen.
            is FfiSyncEvent.RevokedByPeer -> "'${event.peerName}' removed this device — pair again to reconnect"
            // A peer saying no to a remote-control request is worth a
            // notification for the same reason PairingDeclined is above
            // — you might not be looking at the app when the answer
            // comes back.
            is FfiSyncEvent.RemoteControlDeclined -> "'${event.peerName}' declined the remote control request"
            is FfiSyncEvent.Paired,
            is FfiSyncEvent.Connected,
            is FfiSyncEvent.Disconnected,
            is FfiSyncEvent.ClipboardReceived,
            is FfiSyncEvent.WasReset,
            is FfiSyncEvent.WasRevoked,
            is FfiSyncEvent.PausedStateChanged,
            is FfiSyncEvent.Listening,
            is FfiSyncEvent.ClipboardBroadcast,
            is FfiSyncEvent.ReconnectFailed,
            is FfiSyncEvent.NowPlayingChanged,
            is FfiSyncEvent.PeerDiscovered,
            is FfiSyncEvent.PeerActivity,
            // Android is a remote-control *controller* only (see
            // EngineHolder's `NoopRemoteControlHost` wiring) — an
            // inbound `RemoteControlRequested` against this device is
            // auto-declined by the engine before it's ever emitted, so
            // this arm exists only for exhaustiveness. Session
            // started/ended and frame delivery are all tied to the
            // interactive, foreground `RemoteControlScreen` in
            // MainActivity — nothing background-notification-worthy
            // about them.
            is FfiSyncEvent.RemoteControlRequested,
            is FfiSyncEvent.RemoteControlSessionStarted,
            is FfiSyncEvent.RemoteControlSessionEnded,
            is FfiSyncEvent.ScreenFrameReceived,
            // The answer to a lock/unlock tapped in the app a moment ago —
            // shown in-app (see ContinuityStore). The other two are only
            // ever about this device being unlocked, which a phone never is.
            is FfiSyncEvent.ScreenLockResult,
            is FfiSyncEvent.ScreenLockRequested,
            is FfiSyncEvent.UnlockPermissionChanged,
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

    private fun onTransferEvent(event: FfiSyncEvent) {
        when (event) {
            is FfiSyncEvent.FileReceiving -> transferNotifications.started(
                event.transferId, FfiFileTransferDirection.RECEIVING, event.fileName, event.fromName, event.sizeBytes.toLong(),
            )
            is FfiSyncEvent.FileSending -> transferNotifications.started(
                event.transferId, FfiFileTransferDirection.SENDING, event.fileName, event.toName, event.sizeBytes.toLong(),
            )
            is FfiSyncEvent.FileTransferProgress ->
                transferNotifications.progress(event.transferId, event.bytesTransferred.toLong(), event.totalBytes.toLong())
            is FfiSyncEvent.FileTransferStalled -> transferNotifications.stalled(event.transferId)
            // Off the engine's callback thread: moving the file into a
            // folder chosen with "Change folder" is a full copy, as slow as
            // the file is big.
            is FfiSyncEvent.FileReceived -> serviceScope.launch { finishReceived(event) }
            is FfiSyncEvent.FileSent -> transferNotifications.sent(event.transferId, event.fileName, event.toName)
            is FfiSyncEvent.FileTransferFailed -> transferNotifications.failed(event.transferId, event.reason)
            else -> {}
        }
    }

    /** Makes the received file visible to other apps (or moves it into the
     * chosen folder) — regardless of whether notifications are allowed —
     * then replaces its progress notification with one that opens it. */
    private fun finishReceived(event: FfiSyncEvent.FileReceived) {
        // Caught here: this runs on serviceScope, where an uncaught
        // exception would take the whole process down (see onCreate).
        try {
            ReceivedFiles.folder.value.customTreeLabel?.let { transferNotifications.saving(event.transferId, it) }
            val saved = ReceivedFiles.finishReceived(this, event.path)
            saved.couldNotMoveTo?.let { folder ->
                // In the activity feed too, for when notifications are off.
                ContinuityStore.logWarning("Couldn't move '${event.fileName}' to $folder — it's in ${saved.location}")
            }
            transferNotifications.received(event.transferId, event.fileName, saved)
        } catch (e: Exception) {
            android.util.Log.e("ContinuityService", "couldn't finish receiving ${event.path}", e)
        }
    }

    companion object {
        private const val CHANNEL_STATUS = "continuity_status"
        private const val CHANNEL_EVENTS = "continuity_events"
        private const val NOTIFICATION_ID_STATUS = 1
        private const val NOTIFICATION_ID_EVENT_BASE = 1000
    }
}

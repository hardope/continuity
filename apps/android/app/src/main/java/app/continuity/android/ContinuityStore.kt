package app.continuity.android

import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import uniffi.continuity_ffi.FfiDeviceInfo
import uniffi.continuity_ffi.FfiFileTransferDirection
import uniffi.continuity_ffi.FfiNowPlayingInfo
import uniffi.continuity_ffi.FfiRemoteControlRole
import uniffi.continuity_ffi.FfiScreenLockAction
import uniffi.continuity_ffi.FfiScreenLockOutcome
import uniffi.continuity_ffi.FfiSyncEvent

/** A device this process has seen connect at least once — stays listed
 * (marked disconnected) after dropping so it can be reconnected, rather
 * than disappearing the moment it's no longer active.
 *
 * [lastActivityAtMillis] is a wall-clock anchor derived from
 * `FfiSyncEvent.PeerActivity.secondsSinceActivity` (`now - secondsAgo *
 * 1000` at the moment the event arrived) rather than the raw seconds-ago
 * figure — that number is only accurate the instant the event arrives,
 * whereas an anchor lets the UI recompute "how long ago" on every
 * recomposition. */
data class DeviceStatus(
    val id: String,
    val name: String,
    val platform: String,
    val protocolVersion: UInt,
    val connected: Boolean,
    val lastActivityAtMillis: Long? = null,
) {
    val isDesktop: Boolean get() = platform == "mac_os" || platform == "windows" || platform == "linux"

    /** Media and remote control only have real implementations on desktop
     * peers (see core/continuityd/src/media_*.rs and remote_control_*.rs)
     * — an Android/iOS peer would silently ignore the one and auto-decline
     * the other, so neither is offered for those. */
    val supportsMedia: Boolean get() = isDesktop
    val supportsRemoteControl: Boolean get() = isDesktop

    /** Every desktop can be locked (core/continuityd/src/screen_lock_*.rs;
     * macOS and Windows since 0.1.6-beta.7 — an older one answers
     * "unsupported", which the result notice explains). Only a peer
     * announcing protocol v2+ understands the message at all — a v1 peer
     * drops the whole connection on a message it doesn't know (see
     * `continuity_proto::PROTOCOL_VERSION`). */
    val supportsLock: Boolean get() = isDesktop && protocolVersion >= 2u

    /** Unlock is Linux-only: macOS and Windows have no supported way for an
     * app to dismiss their lock screens. */
    val supportsUnlock: Boolean get() = platform == "linux" && protocolVersion >= 2u
}

/** An [FfiNowPlayingInfo] plus the wall-clock moment it arrived — the
 * player needs this to animate the progress bar forward between real
 * updates (the now-playing watcher only pushes a fresh one every 1.5s). */
data class NowPlayingSnapshot(
    val info: FfiNowPlayingInfo,
    val receivedAtMillis: Long,
)

/** One in-progress file transfer, sending or receiving — tracked from
 * `FfiSyncEvent.FileTransferProgress` (throttled server-side to roughly
 * once per MB) and removed the moment the transfer ends, successfully or
 * not. [fileName]/[peerName] come from the `FileSending`/`FileReceiving`
 * event that always precedes the first progress event; they're only null
 * if a progress event somehow won that race. */
data class TransferProgress(
    val id: String,
    val direction: FfiFileTransferDirection,
    val fileName: String?,
    val peerName: String?,
    val bytesTransferred: Long,
    val totalBytes: Long,
)

enum class ActivityKind { PAIRED, CONNECTED, DISCONNECTED, CLIPBOARD, FILE_INCOMING, FILE_RECEIVED, FILE_SENT, ERROR, RESET, FORGOTTEN, REMOTE_CONTROL, SCREEN_LOCK }

enum class Tone { NEUTRAL, SUCCESS, WARNING }

data class ActivityEntry(val kind: ActivityKind, val text: String, val tone: Tone)

data class PendingPairing(val peer: FfiDeviceInfo, val code: String)

/** Only ever the *controlling* side — Android never itself gets controlled
 * (see `NoopRemoteControlHost` in continuity-ffi). */
data class RemoteSession(val peerId: String, val peerName: String)

/**
 * Everything the UI shows, held for the life of the process rather than of
 * any one screen, and fed directly by the foreground service's engine
 * listener. Two things depend on that:
 *
 * - Reopening the app (after backing out, while the service kept running)
 *   shows the real current state. The activity used to rebuild its state
 *   from a replay buffer of the last 64 raw events, which periodic
 *   now-playing and keepalive events push older ones out of within a
 *   couple of minutes — a connected device could simply be missing.
 * - [ShareActivity], launched from another app's share sheet, needs the
 *   same connected-device list without the main screen ever having opened.
 *
 * [onEvent] runs on the engine's callback thread; every update goes through
 * `MutableStateFlow.update`, which is safe from any thread.
 */
object ContinuityStore {
    private const val MAX_ACTIVITY = 200

    private val _devices = MutableStateFlow<Map<String, DeviceStatus>>(emptyMap())
    val devices: StateFlow<Map<String, DeviceStatus>> = _devices.asStateFlow()

    /** Untrusted devices seen on the network, id -> name. */
    private val _nearby = MutableStateFlow<Map<String, String>>(emptyMap())
    val nearby: StateFlow<Map<String, String>> = _nearby.asStateFlow()

    private val _nowPlaying = MutableStateFlow<Map<String, NowPlayingSnapshot>>(emptyMap())
    val nowPlaying: StateFlow<Map<String, NowPlayingSnapshot>> = _nowPlaying.asStateFlow()

    private val _transfers = MutableStateFlow<Map<String, TransferProgress>>(emptyMap())
    val transfers: StateFlow<Map<String, TransferProgress>> = _transfers.asStateFlow()

    /** Newest first, capped — an always-on service would otherwise grow
     * this forever, one entry per clipboard sync. */
    private val _activity = MutableStateFlow<List<ActivityEntry>>(emptyList())
    val activity: StateFlow<List<ActivityEntry>> = _activity.asStateFlow()

    private val _paused = MutableStateFlow(false)
    val paused: StateFlow<Boolean> = _paused.asStateFlow()

    private val _deviceId = MutableStateFlow<String?>(null)
    val deviceId: StateFlow<String?> = _deviceId.asStateFlow()

    private val _pendingPairing = MutableStateFlow<PendingPairing?>(null)
    val pendingPairing: StateFlow<PendingPairing?> = _pendingPairing.asStateFlow()

    private val _remoteSession = MutableStateFlow<RemoteSession?>(null)
    val remoteSession: StateFlow<RemoteSession?> = _remoteSession.asStateFlow()

    private val _remoteFrame = MutableStateFlow<ByteArray?>(null)
    val remoteFrame: StateFlow<ByteArray?> = _remoteFrame.asStateFlow()

    /** One-off messages for whatever screen is showing (a snackbar) — the
     * answer to a lock/unlock request, say. Dropped if nothing's listening;
     * anything that matters later is in [activity] too. */
    private val _notices = MutableSharedFlow<String>(extraBufferCapacity = 8, onBufferOverflow = BufferOverflow.DROP_OLDEST)
    val notices: SharedFlow<String> = _notices.asSharedFlow()

    fun setDeviceId(id: String) {
        _deviceId.value = id
    }

    fun notice(text: String) {
        _notices.tryEmit(text)
    }

    fun clearPendingPairing() {
        _pendingPairing.value = null
    }

    private fun log(kind: ActivityKind, text: String, tone: Tone = Tone.NEUTRAL) {
        _activity.update { (listOf(ActivityEntry(kind, text, tone)) + it).take(MAX_ACTIVITY) }
    }

    fun onEvent(event: FfiSyncEvent) {
        when (event) {
            is FfiSyncEvent.PairingRequested -> _pendingPairing.value = PendingPairing(event.peer, event.code)
            is FfiSyncEvent.Paired -> log(ActivityKind.PAIRED, "Paired with '${event.peer.name}'", Tone.SUCCESS)
            is FfiSyncEvent.PairingDeclined -> log(ActivityKind.DISCONNECTED, "Pairing with '${event.peerName}' declined", Tone.WARNING)
            is FfiSyncEvent.Connected -> {
                val peer = event.peer
                _nearby.update { it - peer.id }
                _devices.update {
                    it + (peer.id to DeviceStatus(peer.id, peer.name, peer.platform, peer.protocolVersion, connected = true))
                }
                log(ActivityKind.CONNECTED, "Connected to '${peer.name}'", Tone.SUCCESS)
            }
            is FfiSyncEvent.PeerDiscovered -> {
                if (!_devices.value.containsKey(event.device.id)) {
                    _nearby.update { it + (event.device.id to event.device.name) }
                }
            }
            is FfiSyncEvent.Disconnected -> {
                _devices.update { devices -> devices[event.peerId]?.let { devices + (event.peerId to it.copy(connected = false)) } ?: devices }
                // Nothing to control on a disconnected device, and the last
                // snapshot would otherwise sit there looking live.
                _nowPlaying.update { it - event.peerId }
                log(ActivityKind.DISCONNECTED, "'${event.peerName}' disconnected")
            }
            is FfiSyncEvent.ClipboardReceived -> log(ActivityKind.CLIPBOARD, "Clipboard synced from '${event.fromName}'")
            is FfiSyncEvent.ClipboardBroadcast -> {
                if (event.peerCount > 0u) {
                    log(ActivityKind.CLIPBOARD, "Clipboard shared with ${event.peerCount} device(s)", Tone.SUCCESS)
                } else {
                    log(ActivityKind.CLIPBOARD, "Clipboard changed, but no device connected to send it to", Tone.WARNING)
                }
            }
            is FfiSyncEvent.FileReceiving -> {
                log(ActivityKind.FILE_INCOMING, "Receiving '${event.fileName}' from '${event.fromName}'...")
                _transfers.update {
                    it + (event.transferId to TransferProgress(event.transferId, FfiFileTransferDirection.RECEIVING, event.fileName, event.fromName, 0L, event.sizeBytes.toLong()))
                }
            }
            is FfiSyncEvent.FileSending -> {
                _transfers.update {
                    it + (event.transferId to TransferProgress(event.transferId, FfiFileTransferDirection.SENDING, event.fileName, event.toName, 0L, event.sizeBytes.toLong()))
                }
            }
            is FfiSyncEvent.FileTransferProgress -> {
                _transfers.update {
                    val existing = it[event.transferId]
                    val updated = existing?.copy(bytesTransferred = event.bytesTransferred.toLong(), totalBytes = event.totalBytes.toLong())
                        ?: TransferProgress(event.transferId, event.direction, null, null, event.bytesTransferred.toLong(), event.totalBytes.toLong())
                    it + (event.transferId to updated)
                }
            }
            is FfiSyncEvent.FileReceived -> {
                _transfers.update { it - event.transferId }
                log(ActivityKind.FILE_RECEIVED, "Received '${event.fileName}'", Tone.SUCCESS)
            }
            is FfiSyncEvent.FileSent -> {
                _transfers.update { it - event.transferId }
                log(ActivityKind.FILE_SENT, "Sent '${event.fileName}' to '${event.toName}'", Tone.SUCCESS)
            }
            is FfiSyncEvent.FileTransferFailed -> {
                _transfers.update { it - event.transferId }
                log(ActivityKind.ERROR, "Transfer failed: ${event.reason}", Tone.WARNING)
            }
            is FfiSyncEvent.Error -> log(ActivityKind.ERROR, event.message, Tone.WARNING)
            is FfiSyncEvent.WasReset -> {
                _devices.value = emptyMap()
                _nowPlaying.value = emptyMap()
                _pendingPairing.value = null
                log(ActivityKind.RESET, "All paired devices have been forgotten", Tone.WARNING)
            }
            is FfiSyncEvent.PausedStateChanged -> _paused.value = event.paused
            is FfiSyncEvent.ReconnectFailed -> {
                val name = _devices.value[event.peerId]?.name ?: event.peerId
                log(ActivityKind.ERROR, "Couldn't reconnect to '$name' — not seen on the network yet", Tone.WARNING)
            }
            is FfiSyncEvent.NowPlayingChanged -> {
                _nowPlaying.update { it + (event.peerId to NowPlayingSnapshot(event.info, System.currentTimeMillis())) }
            }
            is FfiSyncEvent.PeerActivity -> {
                val anchor = System.currentTimeMillis() - event.secondsSinceActivity.toLong() * 1000
                _devices.update { devices ->
                    devices[event.peerId]?.let { devices + (event.peerId to it.copy(lastActivityAtMillis = anchor)) } ?: devices
                }
            }
            is FfiSyncEvent.WasRevoked -> {
                // Removed outright rather than marked offline — the device
                // is forgotten, reconnecting now means pairing again from
                // scratch, so a "Reconnect" button for it would be a lie.
                _devices.update { it - event.peerId }
                _nowPlaying.update { it - event.peerId }
                log(ActivityKind.FORGOTTEN, "Forgot '${event.peerName}'", Tone.WARNING)
            }
            is FfiSyncEvent.RevokedByPeer -> {
                _devices.update { it - event.peerId }
                _nowPlaying.update { it - event.peerId }
                log(ActivityKind.FORGOTTEN, "'${event.peerName}' removed this device", Tone.WARNING)
            }
            is FfiSyncEvent.RemoteControlDeclined -> {
                log(ActivityKind.REMOTE_CONTROL, "'${event.peerName}' declined remote control", Tone.WARNING)
                notice("'${event.peerName}' declined remote control")
            }
            is FfiSyncEvent.RemoteControlSessionStarted -> {
                // A `Controlled`-role start would mean this phone itself was
                // accepted as a target, which can't happen (see
                // RemoteSession) — ignored rather than opening a
                // nonsensical "you're controlling yourself" view.
                if (event.role == FfiRemoteControlRole.CONTROLLING) {
                    _remoteFrame.value = null
                    _remoteSession.value = RemoteSession(event.peerId, event.peerName)
                    log(ActivityKind.REMOTE_CONTROL, "Controlling '${event.peerName}'", Tone.SUCCESS)
                }
            }
            is FfiSyncEvent.RemoteControlSessionEnded -> {
                if (_remoteSession.value?.peerId == event.peerId) {
                    _remoteSession.value = null
                    _remoteFrame.value = null
                }
                val reason = event.reason
                if (reason != null) {
                    log(ActivityKind.REMOTE_CONTROL, "Remote control of '${event.peerName}' ended: $reason", Tone.WARNING)
                    // The remote screen just closed under the user; say why
                    // right away (say, the computer's own sharing prompt was
                    // declined) instead of only in the activity feed.
                    notice("Remote control of '${event.peerName}' ended: $reason")
                } else {
                    log(ActivityKind.REMOTE_CONTROL, "Remote control of '${event.peerName}' ended")
                }
            }
            is FfiSyncEvent.ScreenFrameReceived -> {
                if (_remoteSession.value?.peerId == event.peerId) {
                    _remoteFrame.value = event.frame
                }
            }
            is FfiSyncEvent.ScreenLockResult -> {
                val text = screenLockResultText(event.peerName, event.action, event.outcome)
                log(ActivityKind.SCREEN_LOCK, text, if (event.outcome is FfiScreenLockOutcome.Done) Tone.SUCCESS else Tone.WARNING)
                notice(text)
            }
            // Listening needs no UI; RemoteControlRequested, ScreenLockRequested
            // and UnlockPermissionChanged are only ever about *this* device
            // being controlled/unlocked, which a phone never is.
            else -> {}
        }
    }

    private fun screenLockResultText(peerName: String, action: FfiScreenLockAction, outcome: FfiScreenLockOutcome): String {
        val verb = if (action == FfiScreenLockAction.LOCK) "lock" else "unlock"
        return when (outcome) {
            is FfiScreenLockOutcome.Done -> if (action == FfiScreenLockAction.LOCK) "Locked '$peerName'" else "Unlocked '$peerName'"
            is FfiScreenLockOutcome.NotAllowed ->
                "'$peerName' hasn't allowed this phone to unlock it. On that computer, open the Continuity menu and turn on Allow Remote Unlock for this phone."
            // Every desktop can lock as of 0.1.6-beta.7, so a lock coming
            // back unsupported means that computer's Continuity is older.
            is FfiScreenLockOutcome.Unsupported ->
                if (action == FfiScreenLockAction.LOCK) "'$peerName' can't be locked remotely yet. Update Continuity on it."
                else "'$peerName' can't be unlocked remotely"
            is FfiScreenLockOutcome.Failed -> "Couldn't $verb '$peerName': ${outcome.reason}"
        }
    }
}

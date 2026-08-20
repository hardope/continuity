package app.continuity.android

import android.Manifest
import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Keyboard
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.Error
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.FileUpload
import androidx.compose.material.icons.filled.FolderOpen
import androidx.compose.material.icons.filled.Groups
import androidx.compose.material.icons.filled.Inbox
import androidx.compose.material.icons.filled.Link
import androidx.compose.material.icons.filled.LinkOff
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.MusicNote
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PauseCircle
import androidx.compose.material.icons.filled.PersonRemove
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.ScreenShare
import androidx.compose.material.icons.filled.RestartAlt
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Sensors
import androidx.compose.material.icons.filled.TouchApp
import androidx.compose.material.icons.filled.SkipNext
import androidx.compose.material.icons.filled.SkipPrevious
import androidx.compose.material.icons.filled.Sync
import androidx.compose.material.icons.filled.VerifiedUser
import androidx.compose.material.icons.automirrored.filled.VolumeDown
import androidx.compose.material.icons.automirrored.filled.VolumeUp
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import kotlinx.coroutines.delay
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.PathParser
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.boundsInParent
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.core.content.ContextCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import app.continuity.android.ui.theme.ContinuityTheme
import app.continuity.android.ui.theme.SuccessGreen
import app.continuity.android.ui.theme.SuccessGreenDark
import app.continuity.android.ui.theme.WarningAmber
import app.continuity.android.ui.theme.WarningAmberDark
import uniffi.continuity_ffi.FfiSyncEvent
import java.io.File

class MainActivity : ComponentActivity() {

    private var pickFileCallback: ((Uri) -> Unit)? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        maybeRequestNotificationPermission()
        ContextCompat.startForegroundService(this, Intent(this, ContinuityForegroundService::class.java))

        setContent {
            ContinuityTheme {
                ContinuityScreen(onFilePickerRequested = { launchFilePicker(it) })
            }
        }
    }

    private val pickFile = registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) pickFileCallback?.invoke(uri)
    }

    private fun launchFilePicker(onPicked: (Uri) -> Unit) {
        pickFileCallback = onPicked
        pickFile.launch(arrayOf("*/*"))
    }

    private val requestNotifications =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { /* no-op either way */ }

    private fun maybeRequestNotificationPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            requestNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }
}

private data class ActivityEntry(val icon: ImageVector, val text: String, val tint: Color?)

/** A device this session has seen connect at least once — stays in the
 * list (marked disconnected) after dropping so it can be reconnected,
 * rather than disappearing the moment it's no longer active.
 *
 * [lastActivityAtMillis] is a wall-clock anchor derived from
 * `FfiSyncEvent.PeerActivity.secondsSinceActivity` (`now - secondsAgo *
 * 1000` at the moment the event arrived) rather than storing the raw
 * seconds-ago figure — that number is only accurate the instant the event
 * arrives and goes stale as time passes, whereas an anchor timestamp lets
 * the UI recompute "how long ago" fresh on every recomposition. */
private data class DeviceStatus(
    val name: String,
    val connected: Boolean,
    val platform: String,
    val lastActivityAtMillis: Long? = null,
)

/** A [uniffi.continuity_ffi.FfiNowPlayingInfo] plus the wall-clock moment
 * it arrived — [FullScreenNowPlayingDialog] needs this to animate the
 * progress bar forward between real updates (the now-playing watcher only
 * pushes a fresh one every 1.5s) without waiting on the next network
 * update to move the slider at all. */
private data class NowPlayingSnapshot(
    val info: uniffi.continuity_ffi.FfiNowPlayingInfo,
    val receivedAtMillis: Long,
)

/** Who a "Send File" action targets, chosen via the device picker when
 * more than one device is connected. */
private sealed class SendTarget {
    data class Single(val peerId: String, val name: String) : SendTarget()
    object All : SendTarget()
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ContinuityScreen(onFilePickerRequested: ((Uri) -> Unit) -> Unit) {
    val context = LocalContext.current
    var deviceId by remember { mutableStateOf<String?>(null) }
    val devices = remember { mutableStateMapOf<String, DeviceStatus>() }
    val nearby = remember { mutableStateMapOf<String, String>() }
    val nowPlaying = remember { mutableStateMapOf<String, NowPlayingSnapshot>() }
    var pendingPairing by remember { mutableStateOf<Pair<uniffi.continuity_ffi.FfiDeviceInfo, String>?>(null) }
    var isPaused by remember { mutableStateOf(false) }
    var showResetConfirm by remember { mutableStateOf(false) }
    var showDevicePicker by remember { mutableStateOf(false) }
    // (peerId, name) of the device a "Forget" tap is asking to confirm —
    // null when no confirmation is pending.
    var pendingForget by remember { mutableStateOf<Pair<String, String>?>(null) }
    var activityExpanded by remember { mutableStateOf(false) }
    var fullScreenPlayerPeerId by remember { mutableStateOf<String?>(null) }
    // Non-null only while this device is the *controlling* side of an
    // active remote-control session — Android never itself gets
    // controlled (see EngineHolder's `NoopRemoteControlHost` wiring), so
    // there's no separate "being controlled" state to track here.
    var remoteControlPeerId by remember { mutableStateOf<String?>(null) }
    var remoteControlPeerName by remember { mutableStateOf<String?>(null) }
    var latestRemoteFrame by remember { mutableStateOf<ByteArray?>(null) }
    val activity = remember { mutableStateListOf<ActivityEntry>() }
    // Drives the "active Ns ago" labels in DeviceListCard — they're
    // computed from a stored timestamp, not pushed on every tick, so
    // nothing else causes them to recompose on their own as time passes.
    var nowMillis by remember { mutableStateOf(System.currentTimeMillis()) }
    LaunchedEffect(Unit) {
        while (true) {
            delay(15_000)
            nowMillis = System.currentTimeMillis()
        }
    }

    val successColor = if (isSystemDark()) SuccessGreenDark else SuccessGreen
    val warningColor = if (isSystemDark()) WarningAmberDark else WarningAmber

    val connectedCount = devices.values.count { it.connected }

    fun startSend(target: SendTarget) {
        onFilePickerRequested { uri ->
            val name = queryDisplayName(context, uri) ?: "file"
            val dest = File(context.cacheDir, name)
            context.contentResolver.openInputStream(uri)?.use { input ->
                dest.outputStream().use { output -> input.copyTo(output) }
            }
            when (target) {
                is SendTarget.Single -> EngineHolder.engine?.sendFile(target.peerId, dest.absolutePath)
                is SendTarget.All -> devices.filter { it.value.connected }.keys.forEach { peerId ->
                    EngineHolder.engine?.sendFile(peerId, dest.absolutePath)
                }
            }
        }
    }

    LaunchedEffect(Unit) {
        EngineHolder.events.collect { event ->
            deviceId = EngineHolder.deviceId ?: deviceId
            when (event) {
                is FfiSyncEvent.PairingRequested -> pendingPairing = event.peer to event.code
                is FfiSyncEvent.Paired -> activity.add(0, ActivityEntry(Icons.Default.VerifiedUser, "Paired with '${event.peer.name}'", successColor))
                is FfiSyncEvent.PairingDeclined -> activity.add(0, ActivityEntry(Icons.Default.LinkOff, "Pairing with '${event.peerName}' declined", warningColor))
                is FfiSyncEvent.Connected -> {
                    nearby.remove(event.peer.id)
                    devices[event.peer.id] = DeviceStatus(event.peer.name, connected = true, platform = event.peer.platform)
                    activity.add(0, ActivityEntry(Icons.Default.Link, "Connected to '${event.peer.name}'", successColor))
                }
                is FfiSyncEvent.PeerDiscovered -> {
                    if (!devices.containsKey(event.device.id)) {
                        nearby[event.device.id] = event.device.name
                    }
                }
                is FfiSyncEvent.Disconnected -> {
                    devices[event.peerId]?.let { devices[event.peerId] = it.copy(connected = false) }
                    activity.add(0, ActivityEntry(Icons.Default.LinkOff, "'${event.peerName}' disconnected", null))
                }
                is FfiSyncEvent.ClipboardReceived -> activity.add(0, ActivityEntry(Icons.Default.Sync, "Clipboard synced from '${event.fromName}'", null))
                is FfiSyncEvent.ClipboardBroadcast -> activity.add(
                    0,
                    if (event.peerCount > 0u) {
                        ActivityEntry(Icons.Default.Sync, "Clipboard shared with ${event.peerCount} device(s)", successColor)
                    } else {
                        ActivityEntry(Icons.Default.Sync, "Clipboard changed, but no device connected to send it to", warningColor)
                    },
                )
                is FfiSyncEvent.FileReceiving -> activity.add(0, ActivityEntry(Icons.Default.FolderOpen, "Receiving '${event.fileName}' from '${event.fromName}'...", null))
                is FfiSyncEvent.FileReceived -> activity.add(0, ActivityEntry(Icons.Default.Inbox, "Received '${event.fileName}'", successColor))
                is FfiSyncEvent.FileSent -> activity.add(0, ActivityEntry(Icons.Default.FileUpload, "Sent '${event.fileName}' to '${event.toName}'", successColor))
                is FfiSyncEvent.FileTransferFailed -> activity.add(0, ActivityEntry(Icons.Default.Error, "Transfer failed: ${event.reason}", warningColor))
                is FfiSyncEvent.Error -> activity.add(0, ActivityEntry(Icons.Default.Error, event.message, warningColor))
                is FfiSyncEvent.WasReset -> {
                    devices.clear()
                    nowPlaying.clear()
                    pendingPairing = null
                    activity.add(0, ActivityEntry(Icons.Default.RestartAlt, "All paired devices have been forgotten", warningColor))
                }
                is FfiSyncEvent.PausedStateChanged -> isPaused = event.paused
                is FfiSyncEvent.ReconnectFailed -> {
                    val name = devices[event.peerId]?.name ?: event.peerId
                    activity.add(0, ActivityEntry(Icons.Default.Error, "Couldn't reconnect to '$name' — not seen on the network yet", warningColor))
                }
                is FfiSyncEvent.NowPlayingChanged -> {
                    nowPlaying[event.peerId] = NowPlayingSnapshot(event.info, System.currentTimeMillis())
                }
                is FfiSyncEvent.PeerActivity -> {
                    devices[event.peerId]?.let {
                        devices[event.peerId] = it.copy(
                            lastActivityAtMillis = System.currentTimeMillis() - event.secondsSinceActivity.toLong() * 1000,
                        )
                    }
                }
                is FfiSyncEvent.WasRevoked -> {
                    // Unlike a plain Disconnected, the entry is removed
                    // outright rather than marked offline — the device is
                    // forgotten, reconnecting now means pairing again from
                    // scratch, so a "Reconnect" button here would be a lie.
                    devices.remove(event.peerId)
                    nowPlaying.remove(event.peerId)
                    activity.add(0, ActivityEntry(Icons.Default.PersonRemove, "Forgot '${event.peerName}'", warningColor))
                }
                is FfiSyncEvent.RevokedByPeer -> {
                    devices.remove(event.peerId)
                    nowPlaying.remove(event.peerId)
                    activity.add(0, ActivityEntry(Icons.Default.PersonRemove, "'${event.peerName}' removed this device", warningColor))
                }
                is FfiSyncEvent.RemoteControlDeclined -> {
                    activity.add(0, ActivityEntry(Icons.Default.Error, "'${event.peerName}' declined remote control", warningColor))
                }
                is FfiSyncEvent.RemoteControlSessionStarted -> {
                    // Android is controller-only (see EngineHolder's
                    // `NoopRemoteControlHost`) — a `Controlled`-role
                    // start would mean this device itself was somehow
                    // accepted as a target, which should never happen;
                    // ignored defensively rather than opening a
                    // nonsensical "you're controlling yourself" view.
                    if (event.role == uniffi.continuity_ffi.FfiRemoteControlRole.CONTROLLING) {
                        remoteControlPeerId = event.peerId
                        remoteControlPeerName = event.peerName
                        latestRemoteFrame = null
                        activity.add(0, ActivityEntry(Icons.Default.VerifiedUser, "Controlling '${event.peerName}'", successColor))
                    }
                }
                is FfiSyncEvent.RemoteControlSessionEnded -> {
                    if (event.peerId == remoteControlPeerId) {
                        remoteControlPeerId = null
                        remoteControlPeerName = null
                        latestRemoteFrame = null
                    }
                    val text = event.reason?.let { "Remote control of '${event.peerName}' ended: $it" }
                        ?: "Remote control of '${event.peerName}' ended"
                    activity.add(0, ActivityEntry(Icons.Default.LinkOff, text, if (event.reason != null) warningColor else null))
                }
                is FfiSyncEvent.ScreenFrameReceived -> {
                    if (event.peerId == remoteControlPeerId) {
                        latestRemoteFrame = event.frame
                    }
                }
                else -> {}
            }
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Continuity", fontWeight = FontWeight.SemiBold) },
                actions = {
                    var menuExpanded by remember { mutableStateOf(false) }
                    IconButton(onClick = { menuExpanded = true }) {
                        Icon(Icons.Default.MoreVert, contentDescription = "More options")
                    }
                    DropdownMenu(expanded = menuExpanded, onDismissRequest = { menuExpanded = false }) {
                        DropdownMenuItem(
                            text = { Text("Refresh Nearby Devices") },
                            leadingIcon = { Icon(Icons.Default.Refresh, contentDescription = null) },
                            onClick = {
                                menuExpanded = false
                                EngineHolder.engine?.refreshDiscovery()
                            },
                        )
                        DropdownMenuItem(
                            text = { Text(if (isPaused) "Resume Syncing" else "Pause Syncing") },
                            leadingIcon = { Icon(Icons.Default.PauseCircle, contentDescription = null) },
                            onClick = {
                                menuExpanded = false
                                EngineHolder.engine?.setPaused(!isPaused)
                            },
                        )
                        DropdownMenuItem(
                            text = { Text("Reset...") },
                            leadingIcon = { Icon(Icons.Default.RestartAlt, contentDescription = null) },
                            onClick = {
                                menuExpanded = false
                                showResetConfirm = true
                            },
                        )
                        DropdownMenuItem(
                            text = { Text("Quit") },
                            leadingIcon = { Icon(Icons.Default.LinkOff, contentDescription = null) },
                            onClick = {
                                menuExpanded = false
                                context.stopService(Intent(context, ContinuityForegroundService::class.java))
                                (context as? Activity)?.finishAndRemoveTask()
                                // stopService()/finishAndRemoveTask() don't
                                // guarantee the process actually dies — Android
                                // may keep it cached, with the engine's async
                                // shutdown (mDNS daemon, multicast lock, tokio
                                // tasks) still mid-teardown. Reopening quickly
                                // then races a fresh engine startup against
                                // that still-in-flight cleanup — the app "doesn't
                                // launch", needing a retry once teardown finally
                                // finishes. Killing the process outright makes
                                // the OS reclaim every resource immediately and
                                // completely, so a relaunch always starts clean.
                                android.os.Process.killProcess(android.os.Process.myPid())
                            },
                        )
                    }
                },
                colors = androidx.compose.material3.TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.surface,
                ),
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 16.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Spacer(Modifier.height(4.dp))
            if (nearby.isNotEmpty()) {
                NearbyDevicesCard(
                    nearby = nearby,
                    onConnect = { peerId -> EngineHolder.engine?.reconnectPeer(peerId) },
                )
            }
            DeviceListCard(
                devices = devices,
                nowPlaying = nowPlaying,
                nowMillis = nowMillis,
                hasNearby = nearby.isNotEmpty(),
                successColor = successColor,
                warningColor = warningColor,
                onDisconnect = { peerId -> EngineHolder.engine?.disconnectPeer(peerId) },
                onReconnect = { peerId -> EngineHolder.engine?.reconnectPeer(peerId) },
                onForget = { peerId, name -> pendingForget = peerId to name },
                onMediaCommand = { peerId, command -> EngineHolder.engine?.sendMediaCommand(peerId, command) },
                onOpenFullScreenPlayer = { peerId -> fullScreenPlayerPeerId = peerId },
                onRequestRemoteControl = { peerId -> EngineHolder.engine?.requestRemoteControl(peerId) },
            )
            DeviceIdRow(deviceId = deviceId, onCopy = {
                val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                clipboard.setPrimaryClip(ClipData.newPlainText("Device ID", it))
            })

            FilledTonalButton(
                onClick = { if (connectedCount > 0) showDevicePicker = true },
                enabled = connectedCount > 0,
                modifier = Modifier.fillMaxWidth(),
            ) {
                Icon(Icons.Default.FileUpload, contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(8.dp))
                Text(if (connectedCount > 0) "Send file to..." else "Send file...")
            }

            HorizontalDivider()

            ActivityToggleRow(
                expanded = activityExpanded,
                count = activity.size,
                onToggle = { activityExpanded = !activityExpanded },
            )

            AnimatedVisibility(
                visible = activityExpanded,
                enter = fadeIn() + expandVertically(),
                exit = fadeOut() + shrinkVertically(),
            ) {
                if (activity.isEmpty()) {
                    Box(Modifier.fillMaxWidth().padding(vertical = 24.dp), contentAlignment = Alignment.Center) {
                        Text(
                            "No activity yet",
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                } else {
                    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                        activity.forEach { entry -> ActivityRow(entry) }
                    }
                }
            }

            Spacer(Modifier.height(8.dp))
        }
    }

    val pairing = pendingPairing
    if (pairing != null) {
        val (peer, code) = pairing
        AlertDialog(
            onDismissRequest = {},
            icon = { Icon(Icons.Default.VerifiedUser, contentDescription = null) },
            title = { Text("Pairing request") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("'${peer.name}' wants to pair.")
                    Surface(
                        color = MaterialTheme.colorScheme.primaryContainer,
                        shape = MaterialTheme.shapes.medium,
                    ) {
                        Text(
                            // Grouped into pairs ("12 34 56") rather than a
                            // solid run of six digits — a solid run is easy
                            // to skim-match on a superficial "same length,
                            // similar shape" glance instead of actually
                            // comparing every digit, which is the one thing
                            // that makes this confirmation step meaningful
                            // at all (see continuity-crypto::pairing).
                            code.chunked(2).joinToString(" "),
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(vertical = 16.dp),
                            style = MaterialTheme.typography.displaySmall,
                            fontWeight = androidx.compose.ui.text.font.FontWeight.Bold,
                            letterSpacing = 4.sp,
                            color = MaterialTheme.colorScheme.onPrimaryContainer,
                            textAlign = androidx.compose.ui.text.style.TextAlign.Center,
                        )
                    }
                    Text(
                        "Only confirm if every digit matches the code shown on '${peer.name}' — this is what proves it's really that device.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    EngineHolder.engine?.confirmPairing(peer.id, true)
                    pendingPairing = null
                }) { Text("Yes, it matches") }
            },
            dismissButton = {
                TextButton(onClick = {
                    EngineHolder.engine?.confirmPairing(peer.id, false)
                    pendingPairing = null
                }) { Text("No") }
            },
        )
    }

    if (showResetConfirm) {
        AlertDialog(
            onDismissRequest = { showResetConfirm = false },
            icon = { Icon(Icons.Default.RestartAlt, contentDescription = null) },
            title = { Text("Reset Continuity?") },
            text = {
                Text(
                    "This disconnects every paired device and forgets them all. " +
                        "Each one will need to be paired again from scratch.\n\nAre you sure?",
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    EngineHolder.engine?.reset()
                    showResetConfirm = false
                }) { Text("Reset") }
            },
            dismissButton = {
                TextButton(onClick = { showResetConfirm = false }) { Text("Cancel") }
            },
        )
    }

    pendingForget?.let { (peerId, name) ->
        AlertDialog(
            onDismissRequest = { pendingForget = null },
            icon = { Icon(Icons.Default.PersonRemove, contentDescription = null) },
            title = { Text("Forget '$name'?") },
            text = {
                Text(
                    "This closes the connection now and forgets this device. " +
                        "It'll need to be paired again from scratch to reconnect.\n\nAre you sure?",
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    EngineHolder.engine?.revokeDevice(peerId)
                    pendingForget = null
                }) { Text("Forget") }
            },
            dismissButton = {
                TextButton(onClick = { pendingForget = null }) { Text("Cancel") }
            },
        )
    }

    if (showDevicePicker) {
        AlertDialog(
            onDismissRequest = { showDevicePicker = false },
            icon = { Icon(Icons.Default.FileUpload, contentDescription = null) },
            title = { Text("Send file to...") },
            text = {
                Column {
                    devices.filter { it.value.connected }.forEach { (peerId, status) ->
                        TextButton(
                            onClick = {
                                showDevicePicker = false
                                startSend(SendTarget.Single(peerId, status.name))
                            },
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Icon(Icons.Default.Link, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(8.dp))
                            Text(status.name, modifier = Modifier.weight(1f))
                        }
                    }
                    if (connectedCount > 1) {
                        HorizontalDivider(Modifier.padding(vertical = 4.dp))
                        TextButton(
                            onClick = {
                                showDevicePicker = false
                                startSend(SendTarget.All)
                            },
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Icon(Icons.Default.Groups, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(8.dp))
                            Text("All connected devices ($connectedCount)", modifier = Modifier.weight(1f))
                        }
                    }
                }
            },
            confirmButton = {},
            dismissButton = {
                TextButton(onClick = { showDevicePicker = false }) { Text("Cancel") }
            },
        )
    }

    val fullScreenPeerId = fullScreenPlayerPeerId
    if (fullScreenPeerId != null) {
        val status = devices[fullScreenPeerId]
        if (status == null || !status.connected) {
            // Peer disconnected while the full-screen view was open.
            fullScreenPlayerPeerId = null
        } else {
            FullScreenNowPlayingDialog(
                deviceName = status.name,
                snapshot = nowPlaying[fullScreenPeerId],
                onDismiss = { fullScreenPlayerPeerId = null },
                onMediaCommand = { command -> EngineHolder.engine?.sendMediaCommand(fullScreenPeerId, command) },
            )
        }
    }

    val remoteControlPeer = remoteControlPeerId
    if (remoteControlPeer != null) {
        RemoteControlScreen(
            peerName = remoteControlPeerName ?: remoteControlPeer,
            platform = devices[remoteControlPeer]?.platform ?: "mac_os",
            frame = latestRemoteFrame,
            onInputEvent = { event -> EngineHolder.engine?.sendInputEvent(remoteControlPeer, event) },
            onEnd = { EngineHolder.engine?.endRemoteControlSession(remoteControlPeer) },
        )
    }
}

/// Untrusted devices seen on the network — the engine deliberately doesn't
/// auto-dial these anymore (that used to mean a pairing prompt for every
/// Continuity install anyone ever shared a LAN with), so this is the only
/// way to start pairing with a new device. Tapping "Connect" dials it,
/// which runs the normal pairing handshake and confirmation-code dialog on
/// both ends.
@Composable
private fun NearbyDevicesCard(nearby: Map<String, String>, onConnect: (String) -> Unit) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
    ) {
        Column(modifier = Modifier.padding(vertical = 4.dp)) {
            Text(
                "Nearby Devices",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
            )
            nearby.entries.toList().forEachIndexed { index, (peerId, name) ->
                if (index > 0) HorizontalDivider(Modifier.padding(horizontal = 16.dp))
                Row(
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Icon(Icons.Default.Sensors, contentDescription = null, tint = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text(name, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
                    TextButton(onClick = { onConnect(peerId) }) { Text("Connect") }
                }
            }
        }
    }
}

/** `lastActivityAtMillis == null` means no `PeerActivity` heartbeat has
 * arrived yet for this peer (e.g. it just connected, and the first one
 * only fires on the connection's own ping-interval tick) — "Connected" is
 * the honest label for that, not a fabricated "just now". */
private fun activityLabel(lastActivityAtMillis: Long?, nowMillis: Long): String {
    val anchor = lastActivityAtMillis ?: return "Connected"
    val secondsAgo = ((nowMillis - anchor) / 1000).coerceAtLeast(0)
    return when {
        secondsAgo < 20 -> "Active just now"
        secondsAgo < 60 -> "Active ${secondsAgo}s ago"
        secondsAgo < 3600 -> "Active ${secondsAgo / 60}m ago"
        else -> "Active ${secondsAgo / 3600}h ago"
    }
}

/** Three-tier status color, aligned with [activityLabel]'s own tiers so the
 * dot and the text it sits next to always agree: green while the label
 * would say "just now"/"Ns ago" (<60s since the last ping heartbeat),
 * amber once it's aged into "Nm ago"/"Nh ago", neutral when disconnected.
 * A connection that's actually gone stale doesn't linger in amber long —
 * `CONNECTION_READ_TIMEOUT` tears it down well before the label would
 * reach the hour mark. */
private fun activityColor(
    connected: Boolean,
    lastActivityAtMillis: Long?,
    nowMillis: Long,
    successColor: Color,
    warningColor: Color,
    neutralColor: Color,
): Color {
    if (!connected) return neutralColor
    val anchor = lastActivityAtMillis ?: return successColor
    val secondsAgo = (nowMillis - anchor) / 1000
    return if (secondsAgo < 60) successColor else warningColor
}

// Raw path data (24x24 viewBox) from Material Design Icons (Pictogrammers,
// https://materialdesignicons.com — Apache 2.0 / Pictogrammers Free
// License), not Google's own `material-icons-extended` — that library is
// Google's own icon set and doesn't carry other companies' brand marks.
// MDI's OS icons are a widely-used open source set built for exactly this
// "show which platform" case (nominative use to indicate compatibility,
// same as any dev tool's per-platform download badges), all in one
// visually consistent style so mixing five platforms doesn't look like
// five different icon sources bolted together.
private const val WINDOWS_ICON_PATH = "M3,12V6.75L9,5.43V11.91L3,12M20,3V11.75L10,11.9V5.21L20,3M3,13L9,13.09V19.9L3,18.75V13M20,13.25V22L10,20.09V13.1L20,13.25Z"
private const val APPLE_ICON_PATH =
    "M18.71,19.5C17.88,20.74 17,21.95 15.66,21.97C14.32,22 13.89,21.18 12.37,21.18C10.84,21.18 10.37,21.95 9.1,22C7.79,22.05 6.8,20.68 5.96,19.47C4.25,17 2.94,12.45 4.7,9.39C5.57,7.87 7.13,6.91 8.82,6.88C10.1,6.86 11.32,7.75 12.11,7.75C12.89,7.75 14.37,6.68 15.92,6.84C16.57,6.87 18.39,7.1 19.56,8.82C19.47,8.88 17.39,10.1 17.41,12.63C17.44,15.65 20.06,16.66 20.09,16.67C20.06,16.74 19.67,18.11 18.71,19.5M13,3.5C13.73,2.67 14.94,2.04 15.94,2C16.07,3.17 15.6,4.35 14.9,5.19C14.21,6.04 13.07,6.7 11.95,6.61C11.8,5.46 12.36,4.26 13,3.5Z"
private const val LINUX_ICON_PATH =
    "M14.62,8.35C14.2,8.63 12.87,9.39 12.67,9.54C12.28,9.85 11.92,9.83 11.53,9.53C11.33,9.37 10,8.61 9.58,8.34C9.1,8.03 9.13,7.64 9.66,7.42C11.3,6.73 12.94,6.78 14.57,7.45C15.06,7.66 15.08,8.05 14.62,8.35M21.84,15.63C20.91,13.54 19.64,11.64 18,9.97C17.47,9.42 17.14,8.8 16.94,8.09C16.84,7.76 16.77,7.42 16.7,7.08C16.5,6.2 16.41,5.3 16,4.47C15.27,2.89 14,2.07 12.16,2C10.35,2.05 9,2.81 8.21,4.4C8,4.83 7.85,5.28 7.75,5.74C7.58,6.5 7.43,7.29 7.25,8.06C7.1,8.71 6.8,9.27 6.29,9.77C4.68,11.34 3.39,13.14 2.41,15.12C2.27,15.41 2.13,15.7 2.04,16C1.85,16.66 2.33,17.12 3.03,16.96C3.47,16.87 3.91,16.78 4.33,16.65C4.74,16.5 4.9,16.6 5,17C5.65,19.15 7.07,20.66 9.24,21.5C13.36,23.06 18.17,20.84 19.21,16.92C19.28,16.65 19.38,16.55 19.68,16.65C20.14,16.79 20.61,16.89 21.08,17C21.57,17.09 21.93,16.84 22,16.36C22.03,16.1 21.94,15.87 21.84,15.63"
private const val ANDROID_ICON_PATH =
    "M16.61 15.15C16.15 15.15 15.77 14.78 15.77 14.32S16.15 13.5 16.61 13.5H16.61C17.07 13.5 17.45 13.86 17.45 14.32C17.45 14.78 17.07 15.15 16.61 15.15M7.41 15.15C6.95 15.15 6.57 14.78 6.57 14.32C6.57 13.86 6.95 13.5 7.41 13.5H7.41C7.87 13.5 8.24 13.86 8.24 14.32C8.24 14.78 7.87 15.15 7.41 15.15M16.91 10.14L18.58 7.26C18.67 7.09 18.61 6.88 18.45 6.79C18.28 6.69 18.07 6.75 18 6.92L16.29 9.83C14.95 9.22 13.5 8.9 12 8.91C10.47 8.91 9 9.24 7.73 9.82L6.04 6.91C5.95 6.74 5.74 6.68 5.57 6.78C5.4 6.87 5.35 7.08 5.44 7.25L7.1 10.13C4.25 11.69 2.29 14.58 2 18H22C21.72 14.59 19.77 11.7 16.91 10.14H16.91Z"
private const val IOS_ICON_PATH =
    "M2.09 16.8H3.75V9.76H2.09M2.92 8.84C3.44 8.84 3.84 8.44 3.84 7.94C3.84 7.44 3.44 7.04 2.92 7.04C2.4 7.04 2 7.44 2 7.94C2 8.44 2.4 8.84 2.92 8.84M9.25 7.06C6.46 7.06 4.7 8.96 4.7 12C4.7 15.06 6.46 16.96 9.25 16.96C12.04 16.96 13.8 15.06 13.8 12C13.8 8.96 12.04 7.06 9.25 7.06M9.25 8.5C10.96 8.5 12.05 9.87 12.05 12C12.05 14.15 10.96 15.5 9.25 15.5C7.54 15.5 6.46 14.15 6.46 12C6.46 9.87 7.54 8.5 9.25 8.5M14.5 14.11C14.57 15.87 16 16.96 18.22 16.96C20.54 16.96 22 15.82 22 14C22 12.57 21.18 11.77 19.23 11.32L18.13 11.07C16.95 10.79 16.47 10.42 16.47 9.78C16.47 9 17.2 8.45 18.28 8.45C19.38 8.45 20.13 9 20.21 9.89H21.84C21.8 8.2 20.41 7.06 18.29 7.06C16.21 7.06 14.73 8.21 14.73 9.91C14.73 11.28 15.56 12.13 17.33 12.53L18.57 12.82C19.78 13.11 20.27 13.5 20.27 14.2C20.27 15 19.47 15.57 18.31 15.57C17.15 15.57 16.26 15 16.16 14.11H14.5Z"

private fun platformIconPath(platform: String): String? = when (platform) {
    "mac_os" -> APPLE_ICON_PATH
    "windows" -> WINDOWS_ICON_PATH
    "linux" -> LINUX_ICON_PATH
    "android" -> ANDROID_ICON_PATH
    "ios" -> IOS_ICON_PATH
    else -> null
}

/** Builds a Compose `ImageVector` from raw MDI path data on demand —
 * `remember`ed by the caller since parsing is wasted work to repeat on
 * every recomposition for what's always the same handful of platform
 * strings. The fill color baked in here doesn't matter in practice:
 * `Icon`'s own `tint` parameter applies a color filter over the whole
 * rendered vector, replacing it. */
private fun buildPlatformIcon(pathData: String): ImageVector =
    ImageVector.Builder(defaultWidth = 24.dp, defaultHeight = 24.dp, viewportWidth = 24f, viewportHeight = 24f)
        .addPath(pathData = PathParser().parsePathString(pathData).toNodes(), fill = SolidColor(Color.Black))
        .build()

@Composable
private fun DeviceListCard(
    devices: Map<String, DeviceStatus>,
    nowPlaying: Map<String, NowPlayingSnapshot>,
    nowMillis: Long,
    hasNearby: Boolean,
    successColor: Color,
    warningColor: Color,
    onDisconnect: (String) -> Unit,
    onReconnect: (String) -> Unit,
    onForget: (peerId: String, name: String) -> Unit,
    onMediaCommand: (peerId: String, command: uniffi.continuity_ffi.FfiMediaCommand) -> Unit,
    onOpenFullScreenPlayer: (String) -> Unit,
    onRequestRemoteControl: (peerId: String) -> Unit,
) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
    ) {
        if (devices.isEmpty()) {
            // Two genuinely different situations were both rendering as the
            // same bare spinner before: "nothing paired, nothing nearby
            // either" (nothing is actually in progress — a spinner there
            // reads as broken, not as waiting) vs "a device is right there
            // in Nearby Devices, this card is just waiting on a tap." Only
            // the second one is actually "waiting" on something.
            if (hasNearby) {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(16.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    CircularProgressIndicator(modifier = Modifier.size(20.dp), strokeWidth = 2.dp)
                    Column {
                        Text("Waiting", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                        Text("Tap a nearby device above to connect", style = MaterialTheme.typography.titleMedium)
                    }
                }
            } else {
                Column(
                    modifier = Modifier.fillMaxWidth().padding(20.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    Icon(
                        Icons.Default.Sensors,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.size(28.dp),
                    )
                    Spacer(Modifier.height(8.dp))
                    Text(
                        "No devices nearby yet",
                        style = MaterialTheme.typography.titleMedium,
                        textAlign = androidx.compose.ui.text.style.TextAlign.Center,
                    )
                    Spacer(Modifier.height(2.dp))
                    Text(
                        "Open Continuity on another device on the same Wi-Fi network to get started.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        textAlign = androidx.compose.ui.text.style.TextAlign.Center,
                    )
                }
            }
        } else {
            Column(modifier = Modifier.padding(vertical = 4.dp)) {
                devices.entries.toList().forEachIndexed { index, (peerId, status) ->
                    if (index > 0) HorizontalDivider(Modifier.padding(horizontal = 16.dp))
                    Column(modifier = Modifier.padding(horizontal = 16.dp, vertical = 10.dp)) {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(12.dp),
                        ) {
                            Icon(
                                if (status.connected) Icons.Default.CheckCircle else Icons.Default.LinkOff,
                                contentDescription = null,
                                tint = activityColor(
                                    connected = status.connected,
                                    lastActivityAtMillis = status.lastActivityAtMillis,
                                    nowMillis = nowMillis,
                                    successColor = successColor,
                                    warningColor = warningColor,
                                    neutralColor = MaterialTheme.colorScheme.onSurfaceVariant,
                                ),
                            )
                            Column(modifier = Modifier.weight(1f)) {
                                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                    val iconPath = platformIconPath(status.platform)
                                    if (iconPath != null) {
                                        val icon = remember(iconPath) { buildPlatformIcon(iconPath) }
                                        Icon(
                                            icon,
                                            contentDescription = status.platform,
                                            modifier = Modifier.size(18.dp),
                                            tint = MaterialTheme.colorScheme.onSurfaceVariant,
                                        )
                                    }
                                    Text(status.name, style = MaterialTheme.typography.titleMedium)
                                }
                                Text(
                                    if (status.connected) activityLabel(status.lastActivityAtMillis, nowMillis) else "Disconnected",
                                    style = MaterialTheme.typography.labelSmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                            if (status.connected) {
                                TextButton(onClick = { onDisconnect(peerId) }) { Text("Disconnect") }
                            } else {
                                TextButton(onClick = { onReconnect(peerId) }) {
                                    Icon(Icons.Default.Refresh, contentDescription = null, modifier = Modifier.size(16.dp))
                                    Spacer(Modifier.width(4.dp))
                                    Text("Reconnect")
                                }
                            }
                            IconButton(onClick = { onForget(peerId, status.name) }) {
                                Icon(
                                    Icons.Default.PersonRemove,
                                    contentDescription = "Forget ${status.name}",
                                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                        // Remote control only has a real implementation on
                        // macOS and Windows (see core/continuityd/src/
                        // remote_control_*.rs) — Linux has no
                        // implementation yet despite having one for media
                        // control, and Android/iOS peers wire in
                        // `NoopRemoteControlHost` and would just auto-decline,
                        // so there's no point showing the button at all
                        // for either.
                        val remoteControlCapablePlatform = status.platform == "mac_os" || status.platform == "windows"
                        if (status.connected && remoteControlCapablePlatform) {
                            FilledTonalButton(
                                onClick = { onRequestRemoteControl(peerId) },
                                modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
                            ) {
                                @Suppress("DEPRECATION")
                                Icon(Icons.Default.ScreenShare, contentDescription = null, modifier = Modifier.size(18.dp))
                                Spacer(Modifier.width(8.dp))
                                Text("Remote Control")
                            }
                        }
                        // Media control works against macOS, Windows, and Linux
                        // desktop peers (see core/continuityd/src/media_*.rs) —
                        // hidden for other platforms (Android/iOS peers) rather
                        // than shown and silently doing nothing.
                        val mediaCapablePlatform = status.platform == "mac_os" || status.platform == "windows" || status.platform == "linux"
                        if (status.connected && mediaCapablePlatform) {
                            NowPlayingRow(info = nowPlaying[peerId]?.info, onClick = { onOpenFullScreenPlayer(peerId) })
                            Row(
                                modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
                                horizontalArrangement = Arrangement.Center,
                            ) {
                                IconButton(onClick = { onMediaCommand(peerId, uniffi.continuity_ffi.FfiMediaCommand.Previous) }) {
                                    Icon(Icons.Default.SkipPrevious, contentDescription = "Previous")
                                }
                                IconButton(onClick = { onMediaCommand(peerId, uniffi.continuity_ffi.FfiMediaCommand.PlayPause) }) {
                                    val isPlaying = nowPlaying[peerId]?.info?.isPlaying == true
                                    Icon(
                                        if (isPlaying) Icons.Default.Pause else Icons.Default.PlayArrow,
                                        contentDescription = if (isPlaying) "Pause" else "Play",
                                    )
                                }
                                IconButton(onClick = { onMediaCommand(peerId, uniffi.continuity_ffi.FfiMediaCommand.Next) }) {
                                    Icon(Icons.Default.SkipNext, contentDescription = "Next")
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Album art + title/artist for a connected macOS peer's now-playing state.
/// `info == null` means no update has arrived yet (peer just connected, or
/// isn't actually playing anything the first watcher poll would catch) —
/// shows nothing rather than a misleading "nothing playing" in that case.
@Composable
private fun NowPlayingRow(info: uniffi.continuity_ffi.FfiNowPlayingInfo?, onClick: () -> Unit) {
    if (info == null) return

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = 8.dp)
            .clip(RoundedCornerShape(8.dp))
            .clickable(onClick = onClick)
            .padding(vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        val artwork = remember(info.artwork) {
            if (info.artwork.isNotEmpty()) {
                BitmapFactory.decodeByteArray(info.artwork, 0, info.artwork.size)?.asImageBitmap()
            } else {
                null
            }
        }
        Box(
            modifier = Modifier.size(40.dp).clip(RoundedCornerShape(6.dp)),
            contentAlignment = Alignment.Center,
        ) {
            if (artwork != null) {
                Image(
                    bitmap = artwork,
                    contentDescription = null,
                    contentScale = ContentScale.Crop,
                    modifier = Modifier.fillMaxSize(),
                )
            } else {
                Surface(color = MaterialTheme.colorScheme.surface, modifier = Modifier.fillMaxSize()) {
                    Box(contentAlignment = Alignment.Center, modifier = Modifier.fillMaxSize()) {
                        Icon(
                            Icons.Default.MusicNote,
                            contentDescription = null,
                            modifier = Modifier.size(18.dp),
                            tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }
        }
        val title = info.title
        val artist = info.artist
        Column(modifier = Modifier.weight(1f)) {
            if (info.isPlaying && (title != null || artist != null)) {
                Text(title ?: "Unknown title", style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.Medium)
                if (artist != null) {
                    Text(artist, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            } else if (info.isPlaying) {
                Text("Playing", style = MaterialTheme.typography.bodyMedium)
            } else {
                Text("Nothing playing", style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}

/// Opened by tapping a device's now-playing row — same transport controls
/// as the inline row, at a bigger, focused scale, plus a scrubbable
/// progress bar and a volume level (neither of which the compact inline
/// row has room for).
///
/// Volume is still only *controlled* via step commands
/// (MediaCommand::VolumeUp/VolumeDown) — there's no "set absolute volume"
/// command, so the volume slider here is a disabled, read-only display of
/// the level reported back by the host, not itself draggable. Seeking is
/// different: MediaCommand::Seek takes an absolute position, so the
/// progress slider *is* draggable, sending one Seek command on release
/// (not continuously while dragging).
@Composable
private fun FullScreenNowPlayingDialog(
    deviceName: String,
    snapshot: NowPlayingSnapshot?,
    onDismiss: () -> Unit,
    onMediaCommand: (uniffi.continuity_ffi.FfiMediaCommand) -> Unit,
) {
    val info = snapshot?.info
    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Surface(modifier = Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.surface) {
            Column(
                modifier = Modifier.fillMaxSize().padding(24.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(deviceName, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    IconButton(onClick = onDismiss) {
                        Icon(Icons.Default.Close, contentDescription = "Close")
                    }
                }

                Spacer(Modifier.weight(1f))

                val artwork = remember(info?.artwork) {
                    info?.artwork?.takeIf { it.isNotEmpty() }
                        ?.let { BitmapFactory.decodeByteArray(it, 0, it.size)?.asImageBitmap() }
                }
                Box(
                    modifier = Modifier.fillMaxWidth(0.8f).aspectRatio(1f).clip(RoundedCornerShape(16.dp)),
                    contentAlignment = Alignment.Center,
                ) {
                    if (artwork != null) {
                        Image(
                            bitmap = artwork,
                            contentDescription = null,
                            contentScale = ContentScale.Crop,
                            modifier = Modifier.fillMaxSize(),
                        )
                    } else {
                        Surface(color = MaterialTheme.colorScheme.surfaceVariant, modifier = Modifier.fillMaxSize()) {
                            Box(contentAlignment = Alignment.Center, modifier = Modifier.fillMaxSize()) {
                                Icon(
                                    Icons.Default.MusicNote,
                                    contentDescription = null,
                                    modifier = Modifier.size(64.dp),
                                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                    }
                }

                Spacer(Modifier.height(24.dp))

                Text(
                    info?.title ?: "Nothing playing",
                    style = MaterialTheme.typography.headlineSmall,
                    fontWeight = FontWeight.SemiBold,
                    textAlign = TextAlign.Center,
                )
                val artist = info?.artist
                if (artist != null) {
                    Spacer(Modifier.height(4.dp))
                    Text(artist, style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }

                Spacer(Modifier.height(24.dp))

                val durationMs = (info?.durationMs ?: 0uL).toLong()
                // Ticks independently of whether anything's actually
                // playing (cheap — just a state write every 250ms while
                // this dialog is open) rather than gating the effect on
                // `isPlaying`, so it doesn't need restarting every time
                // play/pause toggles.
                var tickNowMillis by remember { mutableStateOf(System.currentTimeMillis()) }
                LaunchedEffect(Unit) {
                    while (true) {
                        delay(250)
                        tickNowMillis = System.currentTimeMillis()
                    }
                }
                // The position in `info` is only as fresh as the last
                // NowPlayingUpdate (the watcher polls every 1.5s) — while
                // playing, animate forward from it using this device's own
                // clock so the bar moves smoothly between real updates
                // instead of visibly jumping every 1.5s.
                val livePositionMs = remember(snapshot, tickNowMillis) {
                    val base = (info?.positionMs ?: 0uL).toLong()
                    if (snapshot != null && snapshot.info.isPlaying) {
                        val projected = base + (tickNowMillis - snapshot.receivedAtMillis)
                        projected.coerceIn(0, if (durationMs > 0) durationMs else Long.MAX_VALUE)
                    } else {
                        base
                    }
                }
                var isDraggingPosition by remember { mutableStateOf(false) }
                var dragPositionMs by remember { mutableStateOf(0L) }
                // While the user has a finger on the slider, show exactly
                // where they've dragged it to, not the live-ticking value —
                // otherwise playback continuing during the drag would fight
                // the user's own gesture.
                val displayedPositionMs = if (isDraggingPosition) dragPositionMs else livePositionMs

                Column(modifier = Modifier.fillMaxWidth(0.85f)) {
                    Slider(
                        value = displayedPositionMs.toFloat(),
                        onValueChange = {
                            isDraggingPosition = true
                            dragPositionMs = it.toLong()
                        },
                        onValueChangeFinished = {
                            onMediaCommand(uniffi.continuity_ffi.FfiMediaCommand.Seek(dragPositionMs.toULong()))
                            isDraggingPosition = false
                        },
                        valueRange = 0f..durationMs.coerceAtLeast(1).toFloat(),
                        enabled = durationMs > 0,
                    )
                    Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                        Text(
                            formatDuration(displayedPositionMs),
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        Text(
                            formatDuration(durationMs),
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }

                Spacer(Modifier.height(24.dp))

                Row(horizontalArrangement = Arrangement.spacedBy(24.dp), verticalAlignment = Alignment.CenterVertically) {
                    IconButton(onClick = { onMediaCommand(uniffi.continuity_ffi.FfiMediaCommand.Previous) }, modifier = Modifier.size(56.dp)) {
                        Icon(Icons.Default.SkipPrevious, contentDescription = "Previous", modifier = Modifier.size(36.dp))
                    }
                    val isPlaying = info?.isPlaying == true
                    FilledIconButton(onClick = { onMediaCommand(uniffi.continuity_ffi.FfiMediaCommand.PlayPause) }, modifier = Modifier.size(72.dp)) {
                        Icon(
                            if (isPlaying) Icons.Default.Pause else Icons.Default.PlayArrow,
                            contentDescription = if (isPlaying) "Pause" else "Play",
                            modifier = Modifier.size(40.dp),
                        )
                    }
                    IconButton(onClick = { onMediaCommand(uniffi.continuity_ffi.FfiMediaCommand.Next) }, modifier = Modifier.size(56.dp)) {
                        Icon(Icons.Default.SkipNext, contentDescription = "Next", modifier = Modifier.size(36.dp))
                    }
                }

                Spacer(Modifier.height(32.dp))

                // `volumePercent == null` means this peer's platform
                // doesn't report a readable level (see
                // FfiNowPlayingInfo.volumePercent) — the +/- buttons still
                // work either way (they don't depend on knowing the
                // current level), just without a bar to show alongside
                // them.
                val volumePercent = info?.volumePercent
                Row(
                    modifier = Modifier.fillMaxWidth(0.85f),
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    IconButton(onClick = { onMediaCommand(uniffi.continuity_ffi.FfiMediaCommand.VolumeDown) }) {
                        Icon(Icons.AutoMirrored.Filled.VolumeDown, contentDescription = "Volume down")
                    }
                    if (volumePercent != null) {
                        // Disabled, not draggable — this reflects the
                        // actual output level (read back from the host,
                        // see MediaCommand::VolumeUp/VolumeDown in
                        // continuity-proto), it isn't itself a control.
                        // Dragging it wouldn't do anything since there's no
                        // "set absolute volume" command, which would be
                        // confusing on a slider that otherwise looks
                        // interactive.
                        Slider(
                            value = volumePercent,
                            onValueChange = {},
                            enabled = false,
                            valueRange = 0f..1f,
                            modifier = Modifier.weight(1f),
                        )
                        Text(
                            "${(volumePercent * 100).toInt()}%",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.widthIn(min = 36.dp),
                        )
                    } else {
                        Text(
                            "Volume",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.weight(1f),
                            textAlign = TextAlign.Center,
                        )
                    }
                    IconButton(onClick = { onMediaCommand(uniffi.continuity_ffi.FfiMediaCommand.VolumeUp) }) {
                        Icon(Icons.AutoMirrored.Filled.VolumeUp, contentDescription = "Volume up")
                    }
                }

                Spacer(Modifier.weight(1f))
            }
        }
    }
}

private fun formatDuration(ms: Long): String {
    val totalSeconds = (ms / 1000).coerceAtLeast(0)
    val minutes = totalSeconds / 60
    val seconds = totalSeconds % 60
    return "%d:%02d".format(minutes, seconds)
}

/// Live view of a remote-controlled peer's screen, plus two input modes,
/// a keyboard bridge, and an end button.
///
/// **Direct mode**: tap = click at that point, drag = move the cursor
/// there — touch position maps straight to the equivalent point on the
/// peer's screen. Simple and immediately intuitive ("tap what you want to
/// click"), at the cost of precision on a small phone screen controlling
/// a much larger desktop.
///
/// **Trackpad mode**: the strip along the bottom behaves like a laptop
/// trackpad instead of a map of the screen — a one-finger drag moves the
/// cursor by the drag's *relative* distance, not to an absolute mapped
/// point, which is what actually gives precise control on a small phone
/// screen. There's no relative-move message in the wire protocol (`MouseMove`
/// is always a normalized absolute position), so this is done entirely
/// client-side: a virtual cursor position is tracked here and nudged by
/// each drag delta (scaled against the trackpad surface's own size, not
/// the remote screen's), then sent as an ordinary absolute `MouseMove` —
/// the peer never knows the difference. A tap on the trackpad clicks left;
/// the explicit Left/Right buttons below it cover right-click, which a
/// touch gesture can't unambiguously express without adding a second
/// finger to track.
///
/// Keyboard input is intentionally basic for v1: lowercase letters,
/// digits, space, enter, and backspace only — see `macKeyCodeFor`/
/// `windowsKeyCodeFor` below. No shift/uppercase/symbols/arrow keys yet;
/// good enough for a URL or a quick search, not a substitute for a full
/// keyboard.
@Composable
private fun RemoteControlScreen(
    peerName: String,
    platform: String,
    frame: ByteArray?,
    onInputEvent: (uniffi.continuity_ffi.FfiInputEventKind) -> Unit,
    onEnd: () -> Unit,
) {
    Dialog(
        onDismissRequest = onEnd,
        properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
    ) {
        // Immersive: hide the status/nav bars for as long as this screen
        // is up (a normal-brightness status bar drawn over a mostly-dark
        // remote desktop is exactly the "layer disturbing my view" this
        // was built to remove), restoring them the moment it closes —
        // this is scoped to the dialog's own window, not a
        // permanent app-wide setting.
        val view = androidx.compose.ui.platform.LocalView.current
        DisposableEffect(Unit) {
            val dialogWindow = (view.parent as? androidx.compose.ui.window.DialogWindowProvider)?.window
            val controller = dialogWindow?.let { WindowInsetsControllerCompat(it, view) }
            controller?.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            controller?.hide(WindowInsetsCompat.Type.systemBars())
            onDispose { controller?.show(WindowInsetsCompat.Type.systemBars()) }
        }

        Surface(modifier = Modifier.fillMaxSize(), color = androidx.compose.ui.graphics.Color.Black) {
            val configuration = LocalConfiguration.current
            val isLandscape = configuration.orientation == Configuration.ORIENTATION_LANDSCAPE

            Box(modifier = Modifier.fillMaxSize()) {
                val bitmap = remember(frame) {
                    frame?.let { BitmapFactory.decodeByteArray(it, 0, it.size)?.asImageBitmap() }
                }
                var imageBounds by remember { mutableStateOf<Rect?>(null) }
                var trackpadMode by remember { mutableStateOf(false) }

                var zoomScale by remember { mutableStateOf(1f) }
                var panX by remember { mutableStateOf(0f) }
                var panY by remember { mutableStateOf(0f) }

                var controlsVisible by remember { mutableStateOf(true) }
                var lastInteractionAt by remember { mutableStateOf(System.currentTimeMillis()) }
                fun markInteraction() {
                    controlsVisible = true
                    lastInteractionAt = System.currentTimeMillis()
                }
                LaunchedEffect(lastInteractionAt) {
                    delay(3000)
                    controlsVisible = false
                }

                fun sendClickAt(offset: androidx.compose.ui.geometry.Offset) {
                    val bounds = imageBounds ?: return
                    if (bounds.width <= 0f || bounds.height <= 0f) return
                    val nx = ((offset.x - bounds.left) / bounds.width).coerceIn(0f, 1f)
                    val ny = ((offset.y - bounds.top) / bounds.height).coerceIn(0f, 1f)
                    onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.MouseMove(nx.toDouble(), ny.toDouble()))
                }

                fun sendClick(button: uniffi.continuity_ffi.FfiMouseButton) {
                    onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.MouseButton(button, true))
                    onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.MouseButton(button, false))
                }

                @Composable
                fun VideoArea(modifier: Modifier) {
                    Box(modifier = modifier) {
                        if (bitmap != null) {
                            Box(
                                modifier = Modifier
                                    .fillMaxSize()
                                    .graphicsLayer(scaleX = zoomScale, scaleY = zoomScale, translationX = panX, translationY = panY)
                                    .let { base ->
                                        // Pinch-zoom/pan only in trackpad
                                        // mode — in direct mode, a
                                        // single-finger drag already means
                                        // "move the cursor here," and a
                                        // transform-gesture detector on the
                                        // same target would fight over that
                                        // same one-finger drag.
                                        if (trackpadMode) {
                                            base.pointerInput(Unit) {
                                                detectTransformGestures { _, pan, zoom, _ ->
                                                    zoomScale = (zoomScale * zoom).coerceIn(1f, 4f)
                                                    panX += pan.x
                                                    panY += pan.y
                                                    markInteraction()
                                                }
                                            }
                                        } else {
                                            base
                                        }
                                    },
                            ) {
                                Image(
                                    bitmap = bitmap,
                                    contentDescription = null,
                                    contentScale = ContentScale.Fit,
                                    modifier = Modifier
                                        .fillMaxSize()
                                        .onGloballyPositioned { coords -> imageBounds = coords.boundsInParent() }
                                        .let { base ->
                                            // Direct-mode tap/drag only
                                            // applies to the image itself —
                                            // in trackpad mode this area is
                                            // view-only (aside from zoom/pan
                                            // above), all input comes from
                                            // the strip below.
                                            if (trackpadMode) {
                                                base
                                            } else {
                                                base
                                                    .pointerInput(Unit) {
                                                        detectTapGestures(
                                                            onTap = { offset ->
                                                                sendClickAt(offset)
                                                                sendClick(uniffi.continuity_ffi.FfiMouseButton.LEFT)
                                                                markInteraction()
                                                            },
                                                        )
                                                    }
                                                    .pointerInput(Unit) {
                                                        detectDragGestures(onDrag = { change, _ ->
                                                            sendClickAt(change.position)
                                                            markInteraction()
                                                        })
                                                    }
                                            }
                                        },
                                )
                            }
                        } else {
                            Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                                CircularProgressIndicator(color = androidx.compose.ui.graphics.Color.White)
                            }
                        }
                    }
                }

                @Composable
                fun Trackpad(modifier: Modifier) {
                    TrackpadSurface(
                        modifier = modifier,
                        onMove = { nx, ny ->
                            onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.MouseMove(nx, ny))
                            markInteraction()
                        },
                        onLeftClick = { sendClick(uniffi.continuity_ffi.FfiMouseButton.LEFT); markInteraction() },
                        onRightClick = { sendClick(uniffi.continuity_ffi.FfiMouseButton.RIGHT); markInteraction() },
                    )
                }

                // Landscape is the orientation this is actually used in
                // most of the time (a wide remote desktop on a wide phone
                // screen) — putting the trackpad strip along the *side*
                // there instead of eating a fixed slice off the bottom
                // (as portrait does) leaves far more of that width for
                // the one thing actually being looked at.
                if (isLandscape) {
                    Row(modifier = Modifier.fillMaxSize()) {
                        VideoArea(modifier = Modifier.weight(1f).fillMaxHeight())
                        if (trackpadMode) {
                            Trackpad(modifier = Modifier.fillMaxHeight().width(220.dp))
                        }
                    }
                } else {
                    Column(modifier = Modifier.fillMaxSize()) {
                        VideoArea(modifier = Modifier.weight(1f).fillMaxWidth())
                        if (trackpadMode) {
                            Trackpad(modifier = Modifier.fillMaxWidth().height(220.dp))
                        }
                    }
                }

                // A tap near the top edge always reveals the controls,
                // even while hidden — otherwise, once auto-hidden, there
                // would be no way back to the End button short of the
                // system back gesture. Only present (and only catches
                // taps) while actually hidden, so it never steals a tap
                // from the video/trackpad the rest of the time.
                if (!controlsVisible) {
                    Box(
                        modifier = Modifier
                            .fillMaxWidth()
                            .height(32.dp)
                            .align(Alignment.TopCenter)
                            .pointerInput(Unit) { detectTapGestures(onTap = { markInteraction() }) },
                    )
                }

                AnimatedVisibility(
                    visible = controlsVisible,
                    modifier = Modifier.align(Alignment.TopCenter),
                    enter = fadeIn(),
                    exit = fadeOut(),
                ) {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .background(androidx.compose.ui.graphics.Color.Black.copy(alpha = 0.6f))
                            .padding(horizontal = 16.dp, vertical = 10.dp),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            "Controlling '$peerName'",
                            color = androidx.compose.ui.graphics.Color.White,
                            style = MaterialTheme.typography.titleSmall,
                        )
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            IconButton(onClick = { trackpadMode = !trackpadMode; markInteraction() }) {
                                Icon(
                                    Icons.Default.TouchApp,
                                    contentDescription = if (trackpadMode) "Switch to direct touch" else "Switch to trackpad",
                                    tint = if (trackpadMode) MaterialTheme.colorScheme.primary else androidx.compose.ui.graphics.Color.White,
                                )
                            }
                            IconButton(onClick = onEnd) {
                                Icon(Icons.Default.Close, contentDescription = "End remote control", tint = androidx.compose.ui.graphics.Color.White)
                            }
                        }
                    }
                }

                // A basic on-screen-keyboard bridge: a real, focusable
                // text field the IME can type into, kept visually tiny
                // rather than truly invisible (a zero-size field can get
                // skipped by the IME on some keyboards/devices) and
                // cleared back to empty after every keystroke so it never
                // accumulates — this relays individual keystrokes to the
                // peer, it isn't a text buffer of its own.
                var keyboardVisible by remember { mutableStateOf(false) }
                var fieldValue by remember { mutableStateOf("") }
                val focusRequester = remember { FocusRequester() }
                val keyboardController = LocalSoftwareKeyboardController.current
                if (keyboardVisible) {
                    BasicTextField(
                        value = fieldValue,
                        onValueChange = { new ->
                            if (new.length > fieldValue.length) {
                                for (ch in new.substring(fieldValue.length)) {
                                    sendCharacter(ch, platform, onInputEvent)
                                }
                            } else if (new.length < fieldValue.length) {
                                repeat(fieldValue.length - new.length) {
                                    sendBackspace(platform, onInputEvent)
                                }
                            }
                            fieldValue = new
                            markInteraction()
                        },
                        modifier = Modifier
                            .align(Alignment.BottomCenter)
                            .fillMaxWidth()
                            .height(1.dp)
                            .focusRequester(focusRequester),
                        textStyle = androidx.compose.ui.text.TextStyle(color = androidx.compose.ui.graphics.Color.Transparent),
                        cursorBrush = androidx.compose.ui.graphics.SolidColor(androidx.compose.ui.graphics.Color.Transparent),
                    )
                    LaunchedEffect(Unit) {
                        // `requestFocus()` alone doesn't reliably bring up
                        // the IME — it can silently gain focus with the
                        // on-screen keyboard never actually appearing,
                        // which is exactly what made this look like
                        // "keyboard input does nothing." Explicitly
                        // telling the keyboard controller to show is the
                        // documented, reliable way to open it on demand.
                        focusRequester.requestFocus()
                        keyboardController?.show()
                    }
                } else {
                    // Dismiss explicitly rather than just letting the
                    // field leave composition — otherwise the IME can
                    // linger open with nothing focused underneath it.
                    LaunchedEffect(Unit) { keyboardController?.hide() }
                }

                FloatingActionButton(
                    onClick = { keyboardVisible = !keyboardVisible; markInteraction() },
                    modifier = Modifier
                        .align(Alignment.BottomEnd)
                        // Floats above the trackpad strip instead of on
                        // top of its Left/Right click buttons when
                        // trackpad mode is showing that strip along the
                        // bottom (portrait only — in landscape the strip
                        // is a side panel, not underfoot, so this doesn't
                        // need to dodge it there).
                        .padding(bottom = if (trackpadMode && !isLandscape) 269.dp else 20.dp, end = 20.dp),
                ) {
                    Icon(Icons.Default.Keyboard, contentDescription = "Toggle keyboard")
                }
            }
        }
    }
}

/// A relative-movement trackpad surface: drag nudges a virtual cursor
/// position by the drag delta (scaled against this surface's own pixel
/// size, not the remote screen's, so drag distance feels consistent
/// regardless of the peer's actual resolution), tap clicks left, and two
/// explicit buttons below cover left/right click without depending on a
/// gesture (a second-finger tap for "right click" is easy to fumble on a
/// small trackpad strip; a labeled button isn't).
@Composable
private fun TrackpadSurface(
    modifier: Modifier = Modifier,
    onMove: (nx: Double, ny: Double) -> Unit,
    onLeftClick: () -> Unit,
    onRightClick: () -> Unit,
) {
    // Starts centered — there's no way to know where the peer's real
    // cursor already is, so this is a fresh virtual reference point each
    // time trackpad mode is turned on, not synced to the actual remote
    // cursor position.
    var virtualX by remember { mutableStateOf(0.5f) }
    var virtualY by remember { mutableStateOf(0.5f) }
    var surfaceWidthPx by remember { mutableStateOf(1f) }
    var surfaceHeightPx by remember { mutableStateOf(1f) }
    // >1 so a full swipe across the strip covers more than just that same
    // fraction of the remote screen — a literal 1:1 mapping would need an
    // uncomfortably large swipe to cross the whole desktop.
    val sensitivity = 2.5f

    Column(
        modifier = modifier.background(androidx.compose.ui.graphics.Color(0xFF1C1C1E)),
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .weight(1f)
                .onGloballyPositioned { coords ->
                    surfaceWidthPx = coords.size.width.toFloat().coerceAtLeast(1f)
                    surfaceHeightPx = coords.size.height.toFloat().coerceAtLeast(1f)
                }
                .pointerInput(Unit) {
                    detectDragGestures(onDrag = { change, dragAmount ->
                        change.consume()
                        virtualX = (virtualX + dragAmount.x / surfaceWidthPx * sensitivity).coerceIn(0f, 1f)
                        virtualY = (virtualY + dragAmount.y / surfaceHeightPx * sensitivity).coerceIn(0f, 1f)
                        onMove(virtualX.toDouble(), virtualY.toDouble())
                    })
                }
                .pointerInput(Unit) {
                    detectTapGestures(onTap = { onLeftClick() })
                },
            contentAlignment = Alignment.Center,
        ) {
            Text(
                "Trackpad",
                color = androidx.compose.ui.graphics.Color.White.copy(alpha = 0.35f),
                style = MaterialTheme.typography.labelSmall,
            )
        }
        HorizontalDivider(color = androidx.compose.ui.graphics.Color.White.copy(alpha = 0.15f))
        Row(modifier = Modifier.fillMaxWidth().height(48.dp)) {
            TextButton(
                onClick = onLeftClick,
                modifier = Modifier.weight(1f).fillMaxHeight(),
                colors = androidx.compose.material3.ButtonDefaults.textButtonColors(contentColor = androidx.compose.ui.graphics.Color.White),
            ) { Text("Left") }
            Box(
                modifier = Modifier.width(1.dp).fillMaxHeight().background(androidx.compose.ui.graphics.Color.White.copy(alpha = 0.15f)),
            )
            TextButton(
                onClick = onRightClick,
                modifier = Modifier.weight(1f).fillMaxHeight(),
                colors = androidx.compose.material3.ButtonDefaults.textButtonColors(contentColor = androidx.compose.ui.graphics.Color.White),
            ) { Text("Right") }
        }
    }
}

private fun sendCharacter(ch: Char, platform: String, onInputEvent: (uniffi.continuity_ffi.FfiInputEventKind) -> Unit) {
    val code = (if (platform == "windows") windowsKeyCodeFor(ch) else macKeyCodeFor(ch)) ?: return
    onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.KeyDown(code))
    onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.KeyUp(code))
}

private fun sendBackspace(platform: String, onInputEvent: (uniffi.continuity_ffi.FfiInputEventKind) -> Unit) {
    val code = if (platform == "windows") 0x08u else 0x33u // VK_BACK vs kVK_Delete
    onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.KeyDown(code))
    onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.KeyUp(code))
}

/// macOS `kVK_ANSI_*` virtual key codes — arbitrary, non-sequential
/// numbering from `IOKit/hidsystem`/`Events.h`, the same well-known
/// constants `remote_control_mac.rs`'s manual verification tests
/// reference directly. Lowercase letters/digits/space/enter only — see
/// `RemoteControlScreen`'s doc comment for why.
private fun macKeyCodeFor(ch: Char): UInt? = when (ch.lowercaseChar()) {
    'a' -> 0x00u; 's' -> 0x01u; 'd' -> 0x02u; 'f' -> 0x03u; 'h' -> 0x04u; 'g' -> 0x05u
    'z' -> 0x06u; 'x' -> 0x07u; 'c' -> 0x08u; 'v' -> 0x09u; 'b' -> 0x0Bu
    'q' -> 0x0Cu; 'w' -> 0x0Du; 'e' -> 0x0Eu; 'r' -> 0x0Fu; 'y' -> 0x10u; 't' -> 0x11u
    '1' -> 0x12u; '2' -> 0x13u; '3' -> 0x14u; '4' -> 0x15u; '6' -> 0x16u; '5' -> 0x17u
    '9' -> 0x19u; '7' -> 0x1Au; '8' -> 0x1Cu; '0' -> 0x1Du
    'o' -> 0x1Fu; 'u' -> 0x20u; 'i' -> 0x22u; 'p' -> 0x23u
    'l' -> 0x25u; 'j' -> 0x26u; 'k' -> 0x28u; 'n' -> 0x2Du; 'm' -> 0x2Eu
    ' ' -> 0x31u
    '\n' -> 0x24u
    else -> null
}

/// Windows virtual-key codes for the alphanumeric range are just the
/// ASCII uppercase-letter/digit values themselves (`VK_A`..`VK_Z` =
/// `'A'`..`'Z'`, `VK_0`..`VK_9` = `'0'`..`'9'`) — no lookup table needed,
/// unlike macOS's arbitrarily-ordered `kVK_ANSI_*` constants above.
private fun windowsKeyCodeFor(ch: Char): UInt? = when {
    ch.isLetter() -> ch.uppercaseChar().code.toUInt()
    ch.isDigit() -> ch.code.toUInt()
    ch == ' ' -> 0x20u
    ch == '\n' -> 0x0Du
    else -> null
}

@Composable
private fun DeviceIdRow(deviceId: String?, onCopy: (String) -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Icon(Icons.Default.Sensors, contentDescription = null, modifier = Modifier.size(16.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(
            deviceId?.let { "This device · ${it.take(12)}…" } ?: "Starting...",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.weight(1f),
        )
        if (deviceId != null) {
            IconButton(onClick = { onCopy(deviceId) }, modifier = Modifier.size(28.dp)) {
                Icon(Icons.Default.ContentCopy, contentDescription = "Copy device ID", modifier = Modifier.size(16.dp))
            }
        }
    }
}

@Composable
private fun ActivityToggleRow(expanded: Boolean, count: Int, onToggle: () -> Unit) {
    val interactionSource = remember { MutableInteractionSource() }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(interactionSource = interactionSource, indication = null, onClick = onToggle)
            .padding(vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(
            if (count > 0) "Activity ($count)" else "Activity",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Icon(
            if (expanded) Icons.Default.ExpandLess else Icons.Default.ExpandMore,
            contentDescription = if (expanded) "Hide activity" else "Show activity",
            modifier = Modifier.size(18.dp),
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun ActivityRow(entry: ActivityEntry) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Icon(
            entry.icon,
            contentDescription = null,
            modifier = Modifier.size(18.dp),
            tint = entry.tint ?: MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(entry.text, style = MaterialTheme.typography.bodyMedium)
    }
}

@Composable
private fun isSystemDark(): Boolean = androidx.compose.foundation.isSystemInDarkTheme()

private fun queryDisplayName(context: Context, uri: Uri): String? {
    context.contentResolver.query(uri, null, null, null, null)?.use { cursor ->
        val nameIndex = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
        if (nameIndex >= 0 && cursor.moveToFirst()) {
            return cursor.getString(nameIndex)
        }
    }
    return null
}

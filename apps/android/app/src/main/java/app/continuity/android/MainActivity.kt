package app.continuity.android

import android.Manifest
import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
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
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CheckCircle
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
import androidx.compose.material.icons.filled.PauseCircle
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.RestartAlt
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Sensors
import androidx.compose.material.icons.filled.SkipNext
import androidx.compose.material.icons.filled.SkipPrevious
import androidx.compose.material.icons.filled.Sync
import androidx.compose.material.icons.filled.VerifiedUser
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
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
 * rather than disappearing the moment it's no longer active. */
private data class DeviceStatus(val name: String, val connected: Boolean, val platform: String)

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
    val nowPlaying = remember { mutableStateMapOf<String, uniffi.continuity_ffi.FfiNowPlayingInfo>() }
    var pendingPairing by remember { mutableStateOf<Pair<uniffi.continuity_ffi.FfiDeviceInfo, String>?>(null) }
    var isPaused by remember { mutableStateOf(false) }
    var showResetConfirm by remember { mutableStateOf(false) }
    var showDevicePicker by remember { mutableStateOf(false) }
    var activityExpanded by remember { mutableStateOf(false) }
    val activity = remember { mutableStateListOf<ActivityEntry>() }

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
                    devices[event.peer.id] = DeviceStatus(event.peer.name, connected = true, platform = event.peer.platform)
                    activity.add(0, ActivityEntry(Icons.Default.Link, "Connected to '${event.peer.name}'", successColor))
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
                    nowPlaying[event.peerId] = event.info
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
            DeviceListCard(
                devices = devices,
                nowPlaying = nowPlaying,
                successColor = successColor,
                onDisconnect = { peerId -> EngineHolder.engine?.disconnectPeer(peerId) },
                onReconnect = { peerId -> EngineHolder.engine?.reconnectPeer(peerId) },
                onMediaCommand = { peerId, command -> EngineHolder.engine?.sendMediaCommand(peerId, command) },
            )
            DeviceIdRow(deviceId = deviceId, onCopy = {
                val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                clipboard.setPrimaryClip(ClipData.newPlainText("Device ID", it))
            })

            FilledTonalButton(
                onClick = {
                    when (connectedCount) {
                        0 -> {}
                        1 -> {
                            val (peerId, status) = devices.entries.first { it.value.connected }
                            startSend(SendTarget.Single(peerId, status.name))
                        }
                        else -> showDevicePicker = true
                    }
                },
                enabled = connectedCount > 0,
                modifier = Modifier.fillMaxWidth(),
            ) {
                Icon(Icons.Default.FileUpload, contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(8.dp))
                Text(
                    when (connectedCount) {
                        0 -> "Send file..."
                        1 -> "Send file to ${devices.values.first { it.connected }.name}"
                        else -> "Send file to..."
                    },
                )
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
                            code,
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(vertical = 12.dp),
                            style = MaterialTheme.typography.headlineSmall,
                            color = MaterialTheme.colorScheme.onPrimaryContainer,
                            textAlign = androidx.compose.ui.text.style.TextAlign.Center,
                        )
                    }
                    Text("Does this match the code shown on the other device?")
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
            },
            confirmButton = {},
            dismissButton = {
                TextButton(onClick = { showDevicePicker = false }) { Text("Cancel") }
            },
        )
    }
}

@Composable
private fun DeviceListCard(
    devices: Map<String, DeviceStatus>,
    nowPlaying: Map<String, uniffi.continuity_ffi.FfiNowPlayingInfo>,
    successColor: Color,
    onDisconnect: (String) -> Unit,
    onReconnect: (String) -> Unit,
    onMediaCommand: (peerId: String, command: uniffi.continuity_ffi.FfiMediaCommand) -> Unit,
) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
    ) {
        if (devices.isEmpty()) {
            Row(
                modifier = Modifier.fillMaxWidth().padding(16.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                CircularProgressIndicator(modifier = Modifier.size(20.dp), strokeWidth = 2.dp)
                Column {
                    Text("Waiting", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text("No device connected", style = MaterialTheme.typography.titleMedium)
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
                                tint = if (status.connected) successColor else MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                            Column(modifier = Modifier.weight(1f)) {
                                Text(status.name, style = MaterialTheme.typography.titleMedium)
                                Text(
                                    if (status.connected) "Connected" else "Disconnected",
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
                        }
                        // Media control only works against macOS today (see
                        // core/continuityd/src/media_mac.rs) — hidden for
                        // other platforms rather than shown and silently
                        // doing nothing.
                        if (status.connected && status.platform == "mac_os") {
                            NowPlayingRow(info = nowPlaying[peerId])
                            Row(
                                modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
                                horizontalArrangement = Arrangement.Center,
                            ) {
                                IconButton(onClick = { onMediaCommand(peerId, uniffi.continuity_ffi.FfiMediaCommand.PREVIOUS) }) {
                                    Icon(Icons.Default.SkipPrevious, contentDescription = "Previous")
                                }
                                IconButton(onClick = { onMediaCommand(peerId, uniffi.continuity_ffi.FfiMediaCommand.PLAY_PAUSE) }) {
                                    Icon(Icons.Default.PlayArrow, contentDescription = "Play/Pause")
                                }
                                IconButton(onClick = { onMediaCommand(peerId, uniffi.continuity_ffi.FfiMediaCommand.NEXT) }) {
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
private fun NowPlayingRow(info: uniffi.continuity_ffi.FfiNowPlayingInfo?) {
    if (info == null) return

    Row(
        modifier = Modifier.fillMaxWidth().padding(top = 8.dp),
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

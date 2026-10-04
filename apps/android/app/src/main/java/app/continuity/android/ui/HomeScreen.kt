package app.continuity.android.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
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
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.Error
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.filled.FileUpload
import androidx.compose.material.icons.filled.FolderOpen
import androidx.compose.material.icons.filled.Inbox
import androidx.compose.material.icons.filled.Link
import androidx.compose.material.icons.filled.LinkOff
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.MusicNote
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PauseCircle
import androidx.compose.material.icons.filled.PersonRemove
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.RestartAlt
import androidx.compose.material.icons.filled.Sensors
import androidx.compose.material.icons.filled.Sync
import androidx.compose.material.icons.filled.TouchApp
import androidx.compose.material.icons.filled.VerifiedUser
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
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import app.continuity.android.ActivityEntry
import app.continuity.android.ActivityKind
import app.continuity.android.DeviceStatus
import app.continuity.android.EngineHolder
import app.continuity.android.NowPlayingSnapshot
import app.continuity.android.TransferProgress
import uniffi.continuity_ffi.FfiMediaCommand

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun HomeScreen(
    devices: Map<String, DeviceStatus>,
    nearby: Map<String, String>,
    nowPlaying: Map<String, NowPlayingSnapshot>,
    transfers: Map<String, TransferProgress>,
    activity: List<ActivityEntry>,
    isPaused: Boolean,
    deviceId: String?,
    snackbarHostState: SnackbarHostState,
    onOpenDevice: (String) -> Unit,
    onSendFiles: () -> Unit,
    onReset: () -> Unit,
    onQuit: () -> Unit,
) {
    val context = LocalContext.current
    val nowMillis = rememberNowMillis()
    var activityExpanded by rememberSaveable { mutableStateOf(false) }
    val connectedCount = devices.values.count { it.connected }
    // Connected first, so the devices you can actually do something with are
    // always at the top.
    val sortedDevices = devices.values.sortedWith(compareByDescending<DeviceStatus> { it.connected }.thenBy { it.name.lowercase() })

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
                                onReset()
                            },
                        )
                        DropdownMenuItem(
                            text = { Text("Quit") },
                            leadingIcon = { Icon(Icons.Default.LinkOff, contentDescription = null) },
                            onClick = {
                                menuExpanded = false
                                onQuit()
                            },
                        )
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(containerColor = MaterialTheme.colorScheme.surface),
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
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
            if (isPaused) {
                PausedBanner(onResume = { EngineHolder.engine?.setPaused(false) })
            }
            if (transfers.isNotEmpty()) {
                TransfersCard(transfers = transfers.values.toList())
            }
            if (devices.isEmpty()) {
                EmptyDevicesCard(hasNearby = nearby.isNotEmpty())
            } else {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text(
                        "Your devices",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    sortedDevices.forEach { device ->
                        DeviceCard(
                            device = device,
                            nowPlaying = nowPlaying[device.id],
                            nowMillis = nowMillis,
                            onClick = { onOpenDevice(device.id) },
                        )
                    }
                }
            }
            if (nearby.isNotEmpty()) {
                NearbyDevicesCard(nearby = nearby, onConnect = { peerId -> EngineHolder.engine?.reconnectPeer(peerId) })
            }

            FilledTonalButton(
                onClick = onSendFiles,
                enabled = connectedCount > 0,
                modifier = Modifier.fillMaxWidth(),
            ) {
                Icon(Icons.Default.FileUpload, contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(8.dp))
                Text(if (connectedCount > 0) "Send files to..." else "Send files...")
            }

            DeviceIdRow(deviceId = deviceId, onCopy = {
                val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                clipboard.setPrimaryClip(ClipData.newPlainText("Device ID", it))
            })

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
}

/** One paired device. The whole card opens its device page; the trailing
 * button is the one thing worth doing without opening it — play/pause when
 * something's playing there, reconnect when it's offline. */
@Composable
private fun DeviceCard(device: DeviceStatus, nowPlaying: NowPlayingSnapshot?, nowMillis: Long, onClick: () -> Unit) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clip(CardDefaults.shape)
            .clickable(onClick = onClick),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, top = 14.dp, bottom = 14.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            DeviceAvatar(device = device, statusColor = deviceStatusColor(device, nowMillis))
            Column(modifier = Modifier.weight(1f)) {
                Text(device.name, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(
                    "${statusText(device, nowMillis)} · ${platformLabel(device.platform)}",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                val info = nowPlaying?.info
                val track = listOfNotNull(info?.title, info?.artist).joinToString(" — ")
                if (device.connected && info != null && info.isPlaying && track.isNotEmpty()) {
                    Row(
                        modifier = Modifier.padding(top = 2.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        Icon(Icons.Default.MusicNote, contentDescription = null, modifier = Modifier.size(14.dp), tint = MaterialTheme.colorScheme.primary)
                        Text(
                            track,
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.primary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
            }
            if (device.connected && device.supportsMedia && nowPlaying != null) {
                val playing = nowPlaying.info.isPlaying
                IconButton(onClick = { EngineHolder.engine?.sendMediaCommand(device.id, FfiMediaCommand.PlayPause) }) {
                    Icon(
                        if (playing) Icons.Default.Pause else Icons.Default.PlayArrow,
                        contentDescription = if (playing) "Pause" else "Play",
                    )
                }
            }
            if (!device.connected) {
                TextButton(onClick = { EngineHolder.engine?.reconnectPeer(device.id) }) { Text("Reconnect") }
            }
            Icon(
                Icons.AutoMirrored.Filled.KeyboardArrowRight,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

/** Two genuinely different situations: "nothing paired, nothing nearby
 * either" (nothing is in progress — a spinner there reads as broken) vs "a
 * device is right there in Nearby Devices, waiting on a tap." */
@Composable
private fun EmptyDevicesCard(hasNearby: Boolean) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
    ) {
        if (hasNearby) {
            Row(
                modifier = Modifier.fillMaxWidth().padding(16.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                CircularProgressIndicator(modifier = Modifier.size(20.dp), strokeWidth = 2.dp)
                Column {
                    Text("Waiting", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text("Tap a nearby device below to connect", style = MaterialTheme.typography.titleMedium)
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
                Text("No devices nearby yet", style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
                Spacer(Modifier.height(2.dp))
                Text(
                    "Open Continuity on another device on the same Wi-Fi network to get started.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    textAlign = TextAlign.Center,
                )
            }
        }
    }
}

@Composable
private fun PausedBanner(onResume: () -> Unit) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.primaryContainer),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, top = 6.dp, bottom = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Icon(Icons.Default.PauseCircle, contentDescription = null, tint = MaterialTheme.colorScheme.onPrimaryContainer)
            Text(
                "Syncing is paused",
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onPrimaryContainer,
                modifier = Modifier.weight(1f),
            )
            TextButton(onClick = onResume) { Text("Resume") }
        }
    }
}

/** Untrusted devices seen on the network — the engine deliberately doesn't
 * auto-dial these (that used to mean a pairing prompt for every Continuity
 * install anyone ever shared a LAN with), so this is the only way to start
 * pairing with a new device. "Connect" dials it, which runs the normal
 * pairing handshake and confirmation-code dialog on both ends. */
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
            entry.kind.icon(),
            contentDescription = null,
            modifier = Modifier.size(18.dp),
            tint = toneColor(entry.tone),
        )
        Text(entry.text, style = MaterialTheme.typography.bodyMedium)
    }
}

private fun ActivityKind.icon(): ImageVector = when (this) {
    ActivityKind.PAIRED -> Icons.Default.VerifiedUser
    ActivityKind.CONNECTED -> Icons.Default.Link
    ActivityKind.DISCONNECTED -> Icons.Default.LinkOff
    ActivityKind.CLIPBOARD -> Icons.Default.Sync
    ActivityKind.FILE_INCOMING -> Icons.Default.FolderOpen
    ActivityKind.FILE_RECEIVED -> Icons.Default.Inbox
    ActivityKind.FILE_SENT -> Icons.Default.FileUpload
    ActivityKind.ERROR -> Icons.Default.Error
    ActivityKind.RESET -> Icons.Default.RestartAlt
    ActivityKind.FORGOTTEN -> Icons.Default.PersonRemove
    ActivityKind.REMOTE_CONTROL -> Icons.Default.TouchApp
    ActivityKind.SCREEN_LOCK -> Icons.Default.Lock
}

package app.continuity.android.ui

import android.graphics.BitmapFactory
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
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
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.ScreenShare
import androidx.compose.material.icons.automirrored.filled.VolumeDown
import androidx.compose.material.icons.automirrored.filled.VolumeUp
import androidx.compose.material.icons.filled.FileUpload
import androidx.compose.material.icons.filled.LinkOff
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.LockOpen
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.MusicNote
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PersonRemove
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.SkipNext
import androidx.compose.material.icons.filled.SkipPrevious
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import app.continuity.android.ContinuityStore
import app.continuity.android.DeviceStatus
import app.continuity.android.EngineHolder
import app.continuity.android.NowPlayingSnapshot
import app.continuity.android.TransferProgress
import kotlinx.coroutines.delay
import uniffi.continuity_ffi.FfiMediaCommand
import uniffi.continuity_ffi.FfiScreenLockAction

/** The page's height from which everything fits at once (see
 * `DeviceDetailScreen`): the header, the actions, the player with some
 * artwork, and Disconnect/Forget — a typical phone held upright has more. */
private val FITS_SCREEN_HEIGHT = 620.dp

/** Everything about one paired device in one place — opened by tapping it
 * on the home screen, whether or not anything is playing there. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun DeviceDetailScreen(
    device: DeviceStatus,
    nowPlaying: NowPlayingSnapshot?,
    transfers: List<TransferProgress>,
    snackbarHostState: SnackbarHostState,
    onBack: () -> Unit,
    onSendFiles: () -> Unit,
    onForget: () -> Unit,
) {
    val nowMillis = rememberNowMillis()
    var confirmUnlock by remember { mutableStateOf(false) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(device.name, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                    }
                },
                actions = {
                    var menuExpanded by remember { mutableStateOf(false) }
                    IconButton(onClick = { menuExpanded = true }) {
                        Icon(Icons.Default.MoreVert, contentDescription = "More options")
                    }
                    DropdownMenu(expanded = menuExpanded, onDismissRequest = { menuExpanded = false }) {
                        DropdownMenuItem(
                            text = { Text("Forget device...") },
                            leadingIcon = { Icon(Icons.Default.PersonRemove, contentDescription = null) },
                            onClick = {
                                menuExpanded = false
                                onForget()
                            },
                        )
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(containerColor = MaterialTheme.colorScheme.surface),
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        // On a typical phone everything fits on one screen, with the
        // artwork taking whatever height is left. A shorter screen (a small
        // phone, landscape) scrolls instead, with the artwork at a set size.
        BoxWithConstraints(modifier = Modifier.fillMaxSize().padding(padding)) {
            val fitsScreen = maxHeight >= FITS_SCREEN_HEIGHT
            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .then(if (fitsScreen) Modifier else Modifier.verticalScroll(rememberScrollState()))
                    .padding(horizontal = 16.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                DeviceHeader(device = device, nowMillis = nowMillis)

                if (device.connected) {
                    QuickActions(device = device, onSendFiles = onSendFiles, onUnlock = { confirmUnlock = true })
                    if (transfers.isNotEmpty()) {
                        TransfersCard(transfers = transfers)
                    }
                    if (device.supportsMedia) {
                        Card(
                            modifier = Modifier.fillMaxWidth().then(if (fitsScreen) Modifier.weight(1f) else Modifier),
                            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
                        ) {
                            NowPlayingPanel(
                                deviceName = device.name,
                                snapshot = nowPlaying,
                                fillHeight = fitsScreen,
                                onMediaCommand = { command -> EngineHolder.engine?.sendMediaCommand(device.id, command) },
                            )
                        }
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.fillMaxWidth()) {
                        OutlinedButton(onClick = { EngineHolder.engine?.disconnectPeer(device.id) }, modifier = Modifier.weight(1f)) {
                            Icon(Icons.Default.LinkOff, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(8.dp))
                            Text("Disconnect")
                        }
                        ForgetButton(onForget = onForget, modifier = Modifier.weight(1f))
                    }
                } else {
                    Card(
                        modifier = Modifier.fillMaxWidth(),
                        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
                    ) {
                        Column(modifier = Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                            Text(
                                "Everything for '${device.name}' comes back once it's connected again — on the same Wi-Fi, with Continuity running.",
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.fillMaxWidth()) {
                                FilledTonalButton(onClick = { EngineHolder.engine?.reconnectPeer(device.id) }, modifier = Modifier.weight(1f)) {
                                    Icon(Icons.Default.Refresh, contentDescription = null, modifier = Modifier.size(18.dp))
                                    Spacer(Modifier.width(8.dp))
                                    Text("Reconnect")
                                }
                                ForgetButton(onForget = onForget, modifier = Modifier.weight(1f))
                            }
                        }
                    }
                }
                Spacer(Modifier.height(4.dp))
            }
        }
    }

    if (confirmUnlock) {
        AlertDialog(
            onDismissRequest = { confirmUnlock = false },
            icon = { Icon(Icons.Default.LockOpen, contentDescription = null) },
            title = { Text("Unlock '${device.name}'?") },
            text = { Text("Anyone at that computer will be able to use it right away.") },
            confirmButton = {
                TextButton(onClick = {
                    confirmUnlock = false
                    EngineHolder.engine?.requestScreenLock(device.id, FfiScreenLockAction.UNLOCK)
                }) { Text("Unlock") }
            },
            dismissButton = {
                TextButton(onClick = { confirmUnlock = false }) { Text("Cancel") }
            },
        )
    }
}

/** The app bar already names the device; this says what it is and how
 * it's doing, in one compact line. */
@Composable
private fun DeviceHeader(device: DeviceStatus, nowMillis: Long) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        DeviceAvatar(device = device, statusColor = deviceStatusColor(device, nowMillis), size = 40.dp)
        Column {
            Text(platformLabel(device.platform), style = MaterialTheme.typography.titleSmall)
            Text(
                statusText(device, nowMillis),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

private class DeviceAction(val icon: ImageVector, val label: String, val onClick: () -> Unit)

/** What this particular device can do — only the actions its platform (and
 * protocol version) actually supports. Side by side as icon-over-label
 * tiles (at most four, which is every action a device can have), or one
 * ordinary button when sending files is all there is (another phone). */
@Composable
private fun QuickActions(device: DeviceStatus, onSendFiles: () -> Unit, onUnlock: () -> Unit) {
    val actions = buildList<DeviceAction> {
        add(DeviceAction(Icons.Default.FileUpload, "Send files", onSendFiles))
        if (device.supportsRemoteControl) {
            add(
                DeviceAction(Icons.AutoMirrored.Filled.ScreenShare, "Control") {
                    EngineHolder.engine?.requestRemoteControl(device.id)
                    ContinuityStore.notice("Requesting remote control of '${device.name}' — accept on that device if it asks")
                },
            )
        }
        if (device.supportsLock) {
            add(DeviceAction(Icons.Default.Lock, "Lock") { EngineHolder.engine?.requestScreenLock(device.id, FfiScreenLockAction.LOCK) })
        }
        if (device.supportsUnlock) {
            add(DeviceAction(Icons.Default.LockOpen, "Unlock", onUnlock))
        }
    }
    if (actions.size == 1) {
        val action = actions.first()
        FilledTonalButton(onClick = action.onClick, modifier = Modifier.fillMaxWidth()) {
            Icon(action.icon, contentDescription = null, modifier = Modifier.size(18.dp))
            Spacer(Modifier.width(8.dp))
            Text(action.label)
        }
        return
    }
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        actions.chunked(4).forEach { row ->
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.fillMaxWidth()) {
                row.forEach { action ->
                    FilledTonalButton(
                        onClick = action.onClick,
                        modifier = Modifier.weight(1f).height(64.dp),
                        shape = RoundedCornerShape(14.dp),
                        contentPadding = PaddingValues(horizontal = 4.dp, vertical = 8.dp),
                    ) {
                        Column(horizontalAlignment = Alignment.CenterHorizontally) {
                            Icon(action.icon, contentDescription = null, modifier = Modifier.size(22.dp))
                            Spacer(Modifier.height(4.dp))
                            Text(action.label, style = MaterialTheme.typography.labelMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun ForgetButton(onForget: () -> Unit, modifier: Modifier = Modifier) {
    TextButton(
        onClick = onForget,
        modifier = modifier,
        colors = ButtonDefaults.textButtonColors(contentColor = MaterialTheme.colorScheme.error),
    ) {
        Icon(Icons.Default.PersonRemove, contentDescription = null, modifier = Modifier.size(18.dp))
        Spacer(Modifier.width(8.dp))
        Text("Forget")
    }
}

/** The full player — always shown for a media-capable device, even with
 * nothing playing, since play can resume whatever was playing last. With
 * `fillHeight` the artwork is the largest square that fits the height the
 * page leaves it, so the whole page fits on one screen; otherwise it's 60%
 * of the width and the page scrolls.
 *
 * Volume is only *controlled* via step commands (there's no "set absolute
 * volume" command), so the level bar is a read-only display of what the
 * host reports. Seeking is different: `FfiMediaCommand.Seek` takes an
 * absolute position, so the progress bar is draggable, sending one Seek
 * when released (not continuously while dragging). */
@Composable
private fun NowPlayingPanel(
    deviceName: String,
    snapshot: NowPlayingSnapshot?,
    fillHeight: Boolean,
    onMediaCommand: (FfiMediaCommand) -> Unit,
) {
    val info = snapshot?.info
    val title = info?.title
    val artist = info?.artist
    val hasTrack = title != null || artist != null
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .then(if (fillHeight) Modifier.fillMaxHeight() else Modifier)
            .padding(16.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(
            "Now playing",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(8.dp))

        val artwork = remember(info?.artwork) {
            info?.artwork?.takeIf { it.isNotEmpty() }
                ?.let { BitmapFactory.decodeByteArray(it, 0, it.size)?.asImageBitmap() }
        }
        Box(
            modifier = if (fillHeight) Modifier.weight(1f).fillMaxWidth() else Modifier.fillMaxWidth(),
            contentAlignment = Alignment.Center,
        ) {
            val artworkSize = if (fillHeight) Modifier.aspectRatio(1f, matchHeightConstraintsFirst = true) else Modifier.fillMaxWidth(0.6f).aspectRatio(1f)
            Box(
                modifier = artworkSize.clip(RoundedCornerShape(16.dp)),
                contentAlignment = Alignment.Center,
            ) {
                if (artwork != null) {
                    Image(bitmap = artwork, contentDescription = null, contentScale = ContentScale.Crop, modifier = Modifier.fillMaxSize())
                } else {
                    Surface(color = MaterialTheme.colorScheme.surface, modifier = Modifier.fillMaxSize()) {
                        Box(contentAlignment = Alignment.Center, modifier = Modifier.fillMaxSize()) {
                            Icon(
                                Icons.Default.MusicNote,
                                contentDescription = null,
                                modifier = Modifier.size(56.dp),
                                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                }
            }
        }

        Spacer(Modifier.height(12.dp))
        Text(
            if (hasTrack) title ?: "Unknown title" else "Nothing playing",
            style = MaterialTheme.typography.titleLarge,
            fontWeight = FontWeight.SemiBold,
            textAlign = TextAlign.Center,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
        val subtitle = if (hasTrack) artist else "Press play to resume whatever was last playing on '$deviceName'"
        if (subtitle != null) {
            Spacer(Modifier.height(4.dp))
            Text(subtitle, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, textAlign = TextAlign.Center)
        }

        Spacer(Modifier.height(8.dp))

        val durationMs = (info?.durationMs ?: 0uL).toLong()
        val isPlaying = info?.isPlaying == true
        var tickNowMillis by remember { mutableStateOf(System.currentTimeMillis()) }
        LaunchedEffect(isPlaying) {
            while (isPlaying) {
                delay(250)
                tickNowMillis = System.currentTimeMillis()
            }
        }
        // The position in `info` is only as fresh as the last update (the
        // watcher polls every 1.5s) — while playing, animate forward from
        // it on this device's own clock so the bar moves smoothly between
        // real updates instead of visibly jumping.
        val livePositionMs = remember(snapshot, tickNowMillis) {
            val base = (info?.positionMs ?: 0uL).toLong()
            if (snapshot != null && snapshot.info.isPlaying) {
                (base + (tickNowMillis - snapshot.receivedAtMillis)).coerceIn(0, if (durationMs > 0) durationMs else Long.MAX_VALUE)
            } else {
                base
            }
        }
        var isDraggingPosition by remember { mutableStateOf(false) }
        var dragPositionMs by remember { mutableStateOf(0L) }
        // While a finger is on the slider, show where it's been dragged to,
        // not the live value — playback continuing during the drag would
        // otherwise fight the gesture.
        val displayedPositionMs = if (isDraggingPosition) dragPositionMs else livePositionMs

        Slider(
            value = displayedPositionMs.toFloat(),
            onValueChange = {
                isDraggingPosition = true
                dragPositionMs = it.toLong()
            },
            onValueChangeFinished = {
                onMediaCommand(FfiMediaCommand.Seek(dragPositionMs.toULong()))
                isDraggingPosition = false
            },
            valueRange = 0f..durationMs.coerceAtLeast(1).toFloat(),
            enabled = durationMs > 0,
        )
        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
            Text(formatDuration(displayedPositionMs), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Text(formatDuration(durationMs), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }

        Spacer(Modifier.height(4.dp))

        Row(horizontalArrangement = Arrangement.spacedBy(24.dp), verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = { onMediaCommand(FfiMediaCommand.Previous) }, modifier = Modifier.size(56.dp)) {
                Icon(Icons.Default.SkipPrevious, contentDescription = "Previous", modifier = Modifier.size(36.dp))
            }
            FilledIconButton(onClick = { onMediaCommand(FfiMediaCommand.PlayPause) }, modifier = Modifier.size(72.dp)) {
                Icon(
                    if (isPlaying) Icons.Default.Pause else Icons.Default.PlayArrow,
                    contentDescription = if (isPlaying) "Pause" else "Play",
                    modifier = Modifier.size(40.dp),
                )
            }
            IconButton(onClick = { onMediaCommand(FfiMediaCommand.Next) }, modifier = Modifier.size(56.dp)) {
                Icon(Icons.Default.SkipNext, contentDescription = "Next", modifier = Modifier.size(36.dp))
            }
        }

        Spacer(Modifier.height(8.dp))

        // `volumePercent == null` means this peer doesn't report a readable
        // level — the +/- buttons still work either way, just without a bar.
        val volumePercent = info?.volumePercent
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = { onMediaCommand(FfiMediaCommand.VolumeDown) }) {
                Icon(Icons.AutoMirrored.Filled.VolumeDown, contentDescription = "Volume down")
            }
            if (volumePercent != null) {
                // Disabled, not draggable — it reflects the real output
                // level, it isn't itself a control (see the doc comment).
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
            IconButton(onClick = { onMediaCommand(FfiMediaCommand.VolumeUp) }) {
                Icon(Icons.AutoMirrored.Filled.VolumeUp, contentDescription = "Volume up")
            }
        }
    }
}

private fun formatDuration(ms: Long): String {
    val totalSeconds = (ms / 1000).coerceAtLeast(0)
    return "%d:%02d".format(totalSeconds / 60, totalSeconds % 60)
}

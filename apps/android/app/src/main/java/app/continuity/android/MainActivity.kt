package app.continuity.android

import android.Manifest
import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
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
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.Error
import androidx.compose.material.icons.filled.FileUpload
import androidx.compose.material.icons.filled.FolderOpen
import androidx.compose.material.icons.filled.Inbox
import androidx.compose.material.icons.filled.Link
import androidx.compose.material.icons.filled.LinkOff
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.PauseCircle
import androidx.compose.material.icons.filled.RestartAlt
import androidx.compose.material.icons.filled.Sensors
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
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
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

    /** Send-to target — mirrors the CLI/tray shells: whichever peer most
     * recently connected, not a full device picker. */
    private var lastConnectedPeerId: String? = null
    private var pickFileCallback: ((Uri) -> Unit)? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        maybeRequestNotificationPermission()
        ContextCompat.startForegroundService(this, Intent(this, ContinuityForegroundService::class.java))

        setContent {
            ContinuityTheme {
                ContinuityScreen(
                    onPeerConnected = { lastConnectedPeerId = it },
                    onPeerDisconnected = { if (it == lastConnectedPeerId) lastConnectedPeerId = null },
                    onSendFileRequested = { launchFilePicker(it) },
                )
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

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ContinuityScreen(
    onPeerConnected: (String) -> Unit,
    onPeerDisconnected: (String) -> Unit,
    onSendFileRequested: ((Uri) -> Unit) -> Unit,
) {
    val context = LocalContext.current
    var deviceId by remember { mutableStateOf<String?>(null) }
    var connectedPeer by remember { mutableStateOf<uniffi.continuity_ffi.FfiDeviceInfo?>(null) }
    var pendingPairing by remember { mutableStateOf<Pair<uniffi.continuity_ffi.FfiDeviceInfo, String>?>(null) }
    var isPaused by remember { mutableStateOf(false) }
    var showResetConfirm by remember { mutableStateOf(false) }
    val activity = remember { mutableStateListOf<ActivityEntry>() }

    val successColor = if (isSystemDark()) SuccessGreenDark else SuccessGreen
    val warningColor = if (isSystemDark()) WarningAmberDark else WarningAmber

    LaunchedEffect(Unit) {
        EngineHolder.events.collect { event ->
            deviceId = EngineHolder.deviceId ?: deviceId
            when (event) {
                is FfiSyncEvent.PairingRequested -> pendingPairing = event.peer to event.code
                is FfiSyncEvent.Paired -> activity.add(0, ActivityEntry(Icons.Default.VerifiedUser, "Paired with '${event.peer.name}'", successColor))
                is FfiSyncEvent.PairingDeclined -> activity.add(0, ActivityEntry(Icons.Default.LinkOff, "Pairing with '${event.peerName}' declined", warningColor))
                is FfiSyncEvent.Connected -> {
                    connectedPeer = event.peer
                    onPeerConnected(event.peer.id)
                    activity.add(0, ActivityEntry(Icons.Default.Link, "Connected to '${event.peer.name}'", successColor))
                }
                is FfiSyncEvent.Disconnected -> {
                    if (connectedPeer?.id == event.peerId) connectedPeer = null
                    onPeerDisconnected(event.peerId)
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
                    connectedPeer = null
                    pendingPairing = null
                    activity.add(0, ActivityEntry(Icons.Default.RestartAlt, "All paired devices have been forgotten", warningColor))
                }
                is FfiSyncEvent.PausedStateChanged -> isPaused = event.paused
                else -> {}
            }
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Continue", fontWeight = FontWeight.SemiBold) },
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
                .padding(horizontal = 16.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Spacer(Modifier.height(4.dp))
            StatusCard(connectedPeer = connectedPeer, successColor = successColor)
            DeviceIdRow(deviceId = deviceId, onCopy = {
                val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                clipboard.setPrimaryClip(ClipData.newPlainText("Device ID", it))
            })

            FilledTonalButton(
                onClick = {
                    onSendFileRequested { uri -> sendPickedFile(context, uri, connectedPeer?.id) }
                },
                enabled = connectedPeer != null,
                modifier = Modifier.fillMaxWidth(),
            ) {
                Icon(Icons.Default.FileUpload, contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(8.dp))
                Text(if (connectedPeer != null) "Send file to ${connectedPeer!!.name}" else "Send file...")
            }

            Text(
                "Activity",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            if (activity.isEmpty()) {
                Box(Modifier.fillMaxWidth().padding(vertical = 24.dp), contentAlignment = Alignment.Center) {
                    Text(
                        "No activity yet",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            } else {
                LazyColumn(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    items(activity) { entry -> ActivityRow(entry) }
                }
            }
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
}

@Composable
private fun StatusCard(connectedPeer: uniffi.continuity_ffi.FfiDeviceInfo?, successColor: Color) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(16.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            if (connectedPeer != null) {
                Icon(Icons.Default.CheckCircle, contentDescription = null, tint = successColor)
                Column {
                    Text("Connected", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text(connectedPeer.name, style = MaterialTheme.typography.titleMedium)
                }
            } else {
                CircularProgressIndicator(modifier = Modifier.size(20.dp), strokeWidth = 2.dp)
                Column {
                    Text("Waiting", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    Text("No device connected", style = MaterialTheme.typography.titleMedium)
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

private fun sendPickedFile(context: Context, uri: Uri, peerId: String?) {
    if (peerId == null) return
    val name = queryDisplayName(context, uri) ?: "file"
    val dest = File(context.cacheDir, name)
    context.contentResolver.openInputStream(uri)?.use { input ->
        dest.outputStream().use { output -> input.copyTo(output) }
    }
    EngineHolder.engine?.sendFile(peerId, dest.absolutePath)
}

private fun queryDisplayName(context: Context, uri: Uri): String? {
    context.contentResolver.query(uri, null, null, null, null)?.use { cursor ->
        val nameIndex = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
        if (nameIndex >= 0 && cursor.moveToFirst()) {
            return cursor.getString(nameIndex)
        }
    }
    return null
}

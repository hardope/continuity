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
import androidx.compose.material3.LinearProgressIndicator
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
import androidx.activity.compose.BackHandler
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import app.continuity.android.ui.DeviceDetailScreen
import app.continuity.android.ui.HomeScreen
import app.continuity.android.ui.PlatformIcon

class MainActivity : ComponentActivity() {

    private var pickFilesCallback: ((List<Uri>) -> Unit)? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        maybeRequestNotificationPermission()
        maybeRequestNearbyWifiDevicesPermission()
        ContextCompat.startForegroundService(this, Intent(this, ContinuityForegroundService::class.java))

        setContent {
            ContinuityTheme {
                ContinuityApp(onPickFiles = { launchFilePicker(it) })
            }
        }
    }

    private val pickFiles = registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        if (uris.isNotEmpty()) pickFilesCallback?.invoke(uris)
    }

    private fun launchFilePicker(onPicked: (List<Uri>) -> Unit) {
        pickFilesCallback = onPicked
        pickFiles.launch(arrayOf("*/*"))
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

    private val requestNearbyWifiDevices =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { /* no-op either way */ }

    // NEARBY_WIFI_DEVICES is a runtime ("dangerous") permission as of API 33
    // — declaring it in the manifest (see AndroidManifest.xml) only makes it
    // requestable, it doesn't grant it. Without an explicit request here it
    // stays denied on every Android 13+ device, which is exactly the local
    // Wi-Fi discovery this permission gates (see docs/protocol.md).
    private fun maybeRequestNearbyWifiDevicesPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.NEARBY_WIFI_DEVICES) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            requestNearbyWifiDevices.launch(Manifest.permission.NEARBY_WIFI_DEVICES)
        }
    }
}

/** The app's two screens — the device list, and one device's own page —
 * plus everything that can pop up over either: pairing requests,
 * confirmations, and an active remote-control session. All state comes
 * from [ContinuityStore], so it's the same no matter how long the app was
 * away from the foreground. */
@Composable
private fun ContinuityApp(onPickFiles: ((List<Uri>) -> Unit) -> Unit) {
    val context = LocalContext.current
    val devices by ContinuityStore.devices.collectAsStateWithLifecycle()
    val nearby by ContinuityStore.nearby.collectAsStateWithLifecycle()
    val nowPlaying by ContinuityStore.nowPlaying.collectAsStateWithLifecycle()
    val transfers by ContinuityStore.transfers.collectAsStateWithLifecycle()
    val activity by ContinuityStore.activity.collectAsStateWithLifecycle()
    val isPaused by ContinuityStore.paused.collectAsStateWithLifecycle()
    val deviceId by ContinuityStore.deviceId.collectAsStateWithLifecycle()
    val pendingPairing by ContinuityStore.pendingPairing.collectAsStateWithLifecycle()
    val remoteSession by ContinuityStore.remoteSession.collectAsStateWithLifecycle()
    val remoteFrame by ContinuityStore.remoteFrame.collectAsStateWithLifecycle()

    var selectedPeerId by rememberSaveable { mutableStateOf<String?>(null) }
    var showResetConfirm by remember { mutableStateOf(false) }
    var showSendPicker by remember { mutableStateOf(false) }
    // The device a "Forget" tap is asking to confirm — null when none is.
    var pendingForget by remember { mutableStateOf<DeviceStatus?>(null) }
    val snackbarHostState = remember { SnackbarHostState() }

    LaunchedEffect(Unit) {
        ContinuityStore.notices.collect { snackbarHostState.showSnackbar(it) }
    }

    fun sendFiles(peerIds: List<String>) {
        onPickFiles { uris -> OutgoingFiles.stageAndSend(context, uris, peerIds) }
    }

    val connected = devices.values.filter { it.connected }.sortedBy { it.name.lowercase() }
    val selected = selectedPeerId?.let { devices[it] }
    if (selectedPeerId != null && selected == null) {
        // Forgotten (or everything reset) while its page was open.
        LaunchedEffect(selectedPeerId) { selectedPeerId = null }
    }

    if (selected != null) {
        BackHandler { selectedPeerId = null }
        DeviceDetailScreen(
            device = selected,
            nowPlaying = nowPlaying[selected.id],
            transfers = transfers.values.filter { it.peerName == selected.name },
            snackbarHostState = snackbarHostState,
            onBack = { selectedPeerId = null },
            onSendFiles = { sendFiles(listOf(selected.id)) },
            onForget = { pendingForget = selected },
        )
    } else {
        HomeScreen(
            devices = devices,
            nearby = nearby,
            nowPlaying = nowPlaying,
            transfers = transfers,
            activity = activity,
            isPaused = isPaused,
            deviceId = deviceId,
            snackbarHostState = snackbarHostState,
            onOpenDevice = { selectedPeerId = it },
            onSendFiles = {
                // Nothing to choose between with only one device connected.
                val only = connected.singleOrNull()
                showSendPicker = only == null
                if (only != null) sendFiles(listOf(only.id))
            },
            onReset = { showResetConfirm = true },
            onQuit = {
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

    pendingPairing?.let { (peer, code) ->
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
                            fontWeight = FontWeight.Bold,
                            letterSpacing = 4.sp,
                            color = MaterialTheme.colorScheme.onPrimaryContainer,
                            textAlign = TextAlign.Center,
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
                    ContinuityStore.clearPendingPairing()
                }) { Text("Yes, it matches") }
            },
            dismissButton = {
                TextButton(onClick = {
                    EngineHolder.engine?.confirmPairing(peer.id, false)
                    ContinuityStore.clearPendingPairing()
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

    pendingForget?.let { device ->
        AlertDialog(
            onDismissRequest = { pendingForget = null },
            icon = { Icon(Icons.Default.PersonRemove, contentDescription = null) },
            title = { Text("Forget '${device.name}'?") },
            text = {
                Text(
                    "This closes the connection now and forgets this device. " +
                        "It'll need to be paired again from scratch to reconnect.\n\nAre you sure?",
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    EngineHolder.engine?.revokeDevice(device.id)
                    pendingForget = null
                }) { Text("Forget") }
            },
            dismissButton = {
                TextButton(onClick = { pendingForget = null }) { Text("Cancel") }
            },
        )
    }

    if (showSendPicker) {
        AlertDialog(
            onDismissRequest = { showSendPicker = false },
            icon = { Icon(Icons.Default.FileUpload, contentDescription = null) },
            title = { Text("Send files to...") },
            text = {
                Column {
                    connected.forEach { device ->
                        TextButton(
                            onClick = {
                                showSendPicker = false
                                sendFiles(listOf(device.id))
                            },
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            PlatformIcon(device.platform, Modifier.size(18.dp), tint = MaterialTheme.colorScheme.primary)
                            Spacer(Modifier.width(8.dp))
                            Text(device.name, modifier = Modifier.weight(1f))
                        }
                    }
                    if (connected.size > 1) {
                        HorizontalDivider(Modifier.padding(vertical = 4.dp))
                        TextButton(
                            onClick = {
                                showSendPicker = false
                                sendFiles(connected.map { it.id })
                            },
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Icon(Icons.Default.Groups, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(8.dp))
                            Text("All connected devices (${connected.size})", modifier = Modifier.weight(1f))
                        }
                    }
                }
            },
            confirmButton = {},
            dismissButton = {
                TextButton(onClick = { showSendPicker = false }) { Text("Cancel") }
            },
        )
    }

    remoteSession?.let { session ->
        RemoteControlScreen(
            peerName = session.peerName,
            platform = devices[session.peerId]?.platform ?: "mac_os",
            frame = remoteFrame,
            onInputEvent = { event -> EngineHolder.engine?.sendInputEvent(session.peerId, event) },
            onEnd = { EngineHolder.engine?.endRemoteControlSession(session.peerId) },
        )
    }
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
    val code = (when (platform) {
        "windows" -> windowsKeyCodeFor(ch)
        "linux" -> linuxKeyCodeFor(ch)
        else -> macKeyCodeFor(ch)
    }) ?: return
    onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.KeyDown(code))
    onInputEvent(uniffi.continuity_ffi.FfiInputEventKind.KeyUp(code))
}

private fun sendBackspace(platform: String, onInputEvent: (uniffi.continuity_ffi.FfiInputEventKind) -> Unit) {
    val code = when (platform) {
        "windows" -> 0x08u // VK_BACK
        "linux" -> 14u // KEY_BACKSPACE
        else -> 0x33u // kVK_Delete
    }
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

/// Linux evdev `KEY_*` codes (`linux/input-event-codes.h`) — what
/// `remote_control_linux.rs`'s `NotifyKeyboardKeycode` portal call
/// expects directly. Same table as `remote_viewer.rs`'s `linux_key_code`.
private fun linuxKeyCodeFor(ch: Char): UInt? = when (ch.lowercaseChar()) {
    'q' -> 16u; 'w' -> 17u; 'e' -> 18u; 'r' -> 19u; 't' -> 20u; 'y' -> 21u
    'u' -> 22u; 'i' -> 23u; 'o' -> 24u; 'p' -> 25u
    'a' -> 30u; 's' -> 31u; 'd' -> 32u; 'f' -> 33u; 'g' -> 34u; 'h' -> 35u
    'j' -> 36u; 'k' -> 37u; 'l' -> 38u
    'z' -> 44u; 'x' -> 45u; 'c' -> 46u; 'v' -> 47u; 'b' -> 48u; 'n' -> 49u; 'm' -> 50u
    '1' -> 2u; '2' -> 3u; '3' -> 4u; '4' -> 5u; '5' -> 6u
    '6' -> 7u; '7' -> 8u; '8' -> 9u; '9' -> 10u; '0' -> 11u
    ' ' -> 57u
    '\n' -> 28u
    else -> null
}

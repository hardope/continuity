package app.continuity.android

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
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
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Groups
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.core.content.IntentCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import app.continuity.android.ui.DeviceAvatar
import app.continuity.android.ui.platformLabel
import app.continuity.android.ui.successColor
import app.continuity.android.ui.theme.ContinuityTheme
import kotlinx.coroutines.launch

/**
 * "Continuity" in the system share sheet: pick files (or a link, or some
 * text) in any app, Share, Continuity, then which connected device — or
 * all of them — to send it to. Files go out as normal file transfers;
 * shared text lands on the chosen device's clipboard.
 *
 * Opening this starts the foreground service if it isn't already running,
 * so it works from a cold start too — trusted devices reconnect on their
 * own within a few seconds, and the list fills in as they do.
 */
class ShareActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val payload = SharePayload.from(intent)
        if (payload == null) {
            Toast.makeText(this, "Nothing to send", Toast.LENGTH_SHORT).show()
            finish()
            return
        }
        ContextCompat.startForegroundService(this, Intent(this, ContinuityForegroundService::class.java))
        setContent {
            ContinuityTheme {
                ShareSheet(payload = payload, onDone = { finish() })
            }
        }
    }
}

private sealed class SharePayload {
    data class Files(val uris: List<Uri>) : SharePayload()
    data class Text(val text: String) : SharePayload()

    companion object {
        fun from(intent: Intent): SharePayload? {
            val streams = when (intent.action) {
                Intent.ACTION_SEND -> listOfNotNull(IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java))
                Intent.ACTION_SEND_MULTIPLE -> IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
                else -> emptyList()
            }
            // Some apps only put the Uris in ClipData (where the read grant
            // also applies) and leave EXTRA_STREAM empty.
            val uris = streams.ifEmpty {
                val clip = intent.clipData
                if (clip == null) emptyList() else (0 until clip.itemCount).mapNotNull { clip.getItemAt(it).uri }
            }
            if (uris.isNotEmpty()) return SharePayload.Files(uris)
            val text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString()
            return if (text.isNullOrBlank()) null else SharePayload.Text(text)
        }
    }
}

@Composable
private fun ShareSheet(payload: SharePayload, onDone: () -> Unit) {
    val context = LocalContext.current
    val devices by ContinuityStore.devices.collectAsStateWithLifecycle()
    val connected = devices.values.filter { it.connected }.sortedBy { it.name.lowercase() }
    var sending by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    // A shared file's read grant only lasts as long as this activity, so
    // closing it mid-copy would silently lose the send.
    BackHandler(enabled = sending) {}

    fun shareTo(targets: List<DeviceStatus>) {
        if (sending || targets.isEmpty()) return
        sending = true
        val engine = EngineHolder.engine
        val names = if (targets.size == 1) "'${targets[0].name}'" else "${targets.size} devices"
        scope.launch {
            val message = when {
                engine == null -> "Continuity isn't running"
                payload is SharePayload.Files -> {
                    val files = OutgoingFiles.stage(context, payload.uris)
                    for (file in files) {
                        for (target in targets) engine.sendFile(target.id, file.absolutePath)
                    }
                    when {
                        files.isEmpty() -> "Couldn't read the shared file"
                        files.size == 1 -> "Sending '${files[0].name}' to $names"
                        else -> "Sending ${files.size} files to $names"
                    }
                }
                payload is SharePayload.Text -> {
                    for (target in targets) engine.sendText(target.id, payload.text)
                    "Copied to the clipboard on $names"
                }
                else -> null
            }
            if (message != null) Toast.makeText(context.applicationContext, message, Toast.LENGTH_SHORT).show()
            onDone()
        }
    }

    Box(modifier = Modifier.fillMaxSize()) {
        // The dimmed backdrop — tapping it cancels, like any share target's
        // bottom sheet. A sibling *under* the sheet rather than its parent,
        // so taps on the sheet itself never reach it.
        Box(
            modifier = Modifier
                .fillMaxSize()
                .background(Color.Black.copy(alpha = 0.45f))
                .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {
                    if (!sending) onDone()
                },
        )
        Surface(
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {},
            shape = RoundedCornerShape(topStart = 28.dp, topEnd = 28.dp),
            color = MaterialTheme.colorScheme.surface,
            tonalElevation = 3.dp,
        ) {
            Column(modifier = Modifier.navigationBarsPadding().padding(top = 12.dp, bottom = 8.dp)) {
                Box(
                    modifier = Modifier
                        .align(Alignment.CenterHorizontally)
                        .size(width = 32.dp, height = 4.dp)
                        .clip(RoundedCornerShape(2.dp))
                        .background(MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.4f)),
                )
                Spacer(Modifier.height(16.dp))
                Text(
                    "Send with Continuity",
                    style = MaterialTheme.typography.titleLarge,
                    fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.padding(horizontal = 24.dp),
                )
                Text(
                    describe(context, payload),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp),
                )
                Spacer(Modifier.height(8.dp))

                when {
                    sending -> StatusRow(text = if (payload is SharePayload.Files) "Preparing..." else "Sending...")
                    connected.isEmpty() -> {
                        StatusRow(text = "Looking for your devices...")
                        Text(
                            "Make sure Continuity is running on the other device and that both are on the same Wi-Fi network.",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.padding(horizontal = 24.dp),
                        )
                    }
                    else -> Column(modifier = Modifier.heightIn(max = 360.dp).verticalScroll(rememberScrollState())) {
                        connected.forEach { device ->
                            TargetRow(
                                title = device.name,
                                subtitle = platformLabel(device.platform),
                                leading = { DeviceAvatar(device = device, statusColor = successColor(), size = 40.dp) },
                                onClick = { shareTo(listOf(device)) },
                            )
                        }
                        if (connected.size > 1) {
                            HorizontalDivider(Modifier.padding(horizontal = 24.dp, vertical = 4.dp))
                            TargetRow(
                                title = "All connected devices",
                                subtitle = "${connected.size} devices",
                                leading = {
                                    Box(
                                        modifier = Modifier.size(40.dp).clip(CircleShape).background(MaterialTheme.colorScheme.secondaryContainer),
                                        contentAlignment = Alignment.Center,
                                    ) {
                                        Icon(Icons.Default.Groups, contentDescription = null, tint = MaterialTheme.colorScheme.onSecondaryContainer)
                                    }
                                },
                                onClick = { shareTo(connected) },
                            )
                        }
                    }
                }

                TextButton(
                    onClick = onDone,
                    enabled = !sending,
                    modifier = Modifier.align(Alignment.End).padding(horizontal = 16.dp),
                ) { Text("Cancel") }
            }
        }
    }
}

@Composable
private fun TargetRow(title: String, subtitle: String, leading: @Composable () -> Unit, onClick: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 24.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        leading()
        Column(modifier = Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Text(subtitle, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

@Composable
private fun StatusRow(text: String) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        CircularProgressIndicator(modifier = Modifier.size(20.dp), strokeWidth = 2.dp)
        Spacer(Modifier.width(16.dp))
        Text(text, style = MaterialTheme.typography.bodyMedium)
    }
}

private fun describe(context: android.content.Context, payload: SharePayload): String = when (payload) {
    is SharePayload.Files ->
        if (payload.uris.size == 1) {
            OutgoingFiles.displayName(context, payload.uris[0]) ?: "1 file"
        } else {
            "${payload.uris.size} files"
        }
    is SharePayload.Text -> "“${payload.text.trim().take(120)}” — goes to the device's clipboard"
}

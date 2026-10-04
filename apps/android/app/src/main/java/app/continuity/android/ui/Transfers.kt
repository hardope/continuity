package app.continuity.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import app.continuity.android.TransferProgress
import uniffi.continuity_ffi.FfiFileTransferDirection

/** One row per file transfer in progress (sending or receiving), each with
 * a determinate progress bar — keyed by transfer id upstream, so
 * simultaneous transfers to/from different devices each get their own row
 * rather than being conflated into one. */
@Composable
internal fun TransfersCard(transfers: List<TransferProgress>, modifier: Modifier = Modifier) {
    Card(
        modifier = modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
    ) {
        Column(modifier = Modifier.padding(vertical = 4.dp)) {
            transfers.forEachIndexed { index, transfer ->
                if (index > 0) HorizontalDivider(Modifier.padding(horizontal = 16.dp))
                Column(
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    Text(transferLabel(transfer), style = MaterialTheme.typography.titleMedium)
                    // `totalBytes` is only ever 0 in the brief instant before
                    // `FileSending`/`FileReceiving` (which always carries the
                    // real size) has been processed for a brand new transfer
                    // — an indeterminate bar reads as "working on it" rather
                    // than a division by zero.
                    if (transfer.totalBytes > 0) {
                        val fraction = (transfer.bytesTransferred.toFloat() / transfer.totalBytes.toFloat()).coerceIn(0f, 1f)
                        LinearProgressIndicator(
                            progress = { fraction },
                            modifier = Modifier.fillMaxWidth(),
                        )
                        Text(
                            "${formatBytes(transfer.bytesTransferred)} / ${formatBytes(transfer.totalBytes)} " +
                                "(${(fraction * 100).toInt()}%)",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    } else {
                        LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
                    }
                }
            }
        }
    }
}

private fun transferLabel(transfer: TransferProgress): String {
    val file = transfer.fileName?.let { "'$it'" } ?: "file"
    val peer = transfer.peerName
    return if (transfer.direction == FfiFileTransferDirection.SENDING) {
        if (peer != null) "Sending $file to '$peer'" else "Sending $file..."
    } else {
        if (peer != null) "Receiving $file from '$peer'" else "Receiving $file..."
    }
}

internal fun formatBytes(bytes: Long): String = when {
    bytes >= 1024 * 1024 -> "%.1f MB".format(bytes / (1024.0 * 1024.0))
    bytes >= 1024 -> "%.1f KB".format(bytes / 1024.0)
    else -> "$bytes B"
}

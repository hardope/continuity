package app.continuity.android

import kotlinx.coroutines.flow.MutableSharedFlow
import uniffi.continuity_ffi.ContinuityEngine
import uniffi.continuity_ffi.FfiSyncEvent

/**
 * Shared between the foreground service (which owns the engine's
 * lifecycle) and the activity (which reacts to events and issues
 * commands) — a plain singleton rather than a bound-service interface
 * since everything here runs in the same process.
 */
object EngineHolder {
    @Volatile
    var engine: ContinuityEngine? = null

    @Volatile
    var deviceId: String? = null

    /** `replay` (not just `extraBufferCapacity`) is what matters here —
     * without it, an event emitted before the activity's collector
     * attaches is lost forever rather than delivered late, since
     * extraBufferCapacity only smooths backpressure for subscribers that
     * are already attached, it isn't a cache for future ones. The engine
     * emits its first event (`Listening`) essentially immediately after
     * `ContinuityEngine.start()` returns, on a background thread that can
     * easily win the race against Compose's first recomposition — this
     * was silently dropping that event (and any others emitted early)
     * every time the race went the wrong way. */
    val events = MutableSharedFlow<FfiSyncEvent>(replay = 64, extraBufferCapacity = 64)
}

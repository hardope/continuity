package app.continuity.android

import uniffi.continuity_ffi.ContinuityEngine

/**
 * Shared between the foreground service (which owns the engine's
 * lifecycle) and the UI (which issues commands) — a plain singleton rather
 * than a bound-service interface since everything here runs in the same
 * process. State and events flow the other way through [ContinuityStore],
 * which the service's engine listener feeds directly.
 */
object EngineHolder {
    @Volatile
    var engine: ContinuityEngine? = null
}

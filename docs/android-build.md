# Android build (Phase 2)

Status: **built and verified working** — installed and ran on the `Medium_Phone_API_36.0` emulator, and completed a real pairing + bidirectional clipboard sync against a desktop `continuityctl` instance over the actual protocol (mDNS discovery, TLS handshake, human-verified pairing code, clipboard broadcast both directions).

## Toolchain

None of this was preinstalled except the SDK's `platform-tools`/AVDs — all added fresh:

```bash
rustup target add aarch64-linux-android x86_64-linux-android armv7-linux-androideabi
cargo install cargo-ndk
```

JDK 17 and the NDK needed installing too. If `brew install --cask temurin@17` fails with a `sudo: a terminal is required` error (no interactive password prompt available), grab the plain tarball instead — no installer, no sudo:

```bash
curl -L -o jdk17.tar.gz "https://api.adoptium.net/v3/binary/latest/17/ga/mac/x64/jdk/hotspot/normal/eclipse"
mkdir -p ~/dev-tools && tar -xzf jdk17.tar.gz -C ~/dev-tools
export JAVA_HOME=~/dev-tools/jdk-17*/Contents/Home
```

NDK, once a JDK is available:

```bash
export JAVA_HOME=~/dev-tools/jdk-17*/Contents/Home
SDK=~/Library/Android/sdk
yes | "$SDK/cmdline-tools/latest/bin/sdkmanager" --sdk_root="$SDK" --licenses
"$SDK/cmdline-tools/latest/bin/sdkmanager" --sdk_root="$SDK" "ndk;27.0.12077973" platform-tools "platforms;android-34" "build-tools;34.0.0"
```

Gradle: if `brew install gradle` fails on a bottle download (`HTTP/2 stream ... PROTOCOL_ERROR` — seen repeatedly in this environment on larger downloads), grab the distribution zip directly with HTTP/1.1 and resume enabled, since flaky connections here keep resetting HTTP/2 streams mid-transfer:

```bash
curl -L --http1.1 -C - -o gradle.zip https://services.gradle.org/distributions/gradle-8.9-bin.zip
unzip gradle.zip -d ~/dev-tools
```

## Build

```bash
export JAVA_HOME=~/dev-tools/jdk-17.0.20+8/Contents/Home
export ANDROID_NDK_HOME=~/Library/Android/sdk/ndk/27.0.12077973
export ANDROID_HOME=~/Library/Android/sdk

# Cross-compile the Rust side first — output lands directly in the app's jniLibs
cd core/continuity-ffi
cargo ndk -t arm64-v8a -t x86_64 -o ../../apps/android/app/src/main/jniLibs build --release

# Then the app itself
cd ../../apps/android
echo "sdk.dir=$ANDROID_HOME" > local.properties
~/dev-tools/gradle-8.9/bin/gradle assembleDebug --no-daemon
```

Output: `apps/android/app/build/outputs/apk/debug/app-debug.apk`. Compiled clean on the first real attempt — the mobile-portability refactor in `continuity-ffi` (host-provided identity bytes and data directory instead of the desktop shells' `keyring`/`directories` crates, neither of which has an Android backend) turned out to matter for real, not just in theory.

## Running it

```bash
ADB=~/Library/Android/sdk/platform-tools/adb
~/Library/Android/sdk/emulator/emulator -avd Medium_Phone_API_36.0 -no-window &
# wait for boot: `adb shell getprop sys.boot_completed` returns 1
"$ADB" install -r app-debug.apk
"$ADB" shell pm grant app.continuity.android android.permission.POST_NOTIFICATIONS
"$ADB" shell am start -n app.continuity.android/.MainActivity
```

## UI

The app is Jetpack Compose + Material 3 (`MainActivity.kt`, `ui/theme/`) — dynamic color on Android 12+, a status card, a copyable device-id row, and an activity feed with per-event-type icons. There's no XML layout at all; the old View-based UI was fully replaced, not layered on top of.

## Rust-side logging

`continuity-ffi::init_android_logging()` routes `tracing` output to `logcat` under the tag `continuity_ffi` via the `paranoid-android` crate — call it once (from `ContinuityForegroundService.onCreate`) before starting the engine. Without this, every `tracing::debug!`/`warn!` call in `continuity-daemon` and friends goes to stdout, which doesn't exist for an Android app, so it's silently discarded. This was essential for finding the real bugs below — guessing from Kotlin-side symptoms alone wasn't getting anywhere.

## What's verified

App installs and launches without crashing; identity generation and `EncryptedSharedPreferences` storage work; the foreground service starts the engine, acquires the required `WifiManager.MulticastLock`, and begins listening; `SyncEvent`s flow correctly from the Rust engine through the UniFFI callback boundary into Kotlin and update the UI; and pairing plus bidirectional clipboard sync completed successfully against a live desktop `continuityctl` process on the same LAN (confirmed with real Rust-side logs, not just UI appearance).

## Three real bugs found and fixed while testing (not just scaffolding issues)

1. **Main-thread blocking.** `ContinuityEngine.start()` is a blocking FFI call — it doesn't return until the Rust side's tokio runtime, mDNS advertiser, and TLS listener are all up. `ContinuityForegroundService.onCreate()` runs on the main thread, so calling it inline froze the entire app (no crash, no ANR — just a silently unresponsive UI) whenever engine startup took any real time, which varied run to run. Fixed by moving `startEngine()` onto a dedicated `CoroutineScope(Dispatchers.IO)`.
2. **`SharedFlow` event loss.** `EngineHolder.events` was `MutableSharedFlow(extraBufferCapacity = 64)` — with the default `replay = 0`, `extraBufferCapacity` only smooths backpressure for subscribers that are *already* attached, it is not a cache for future ones. The engine's first event (`Listening`) fires on a background thread essentially immediately after `start()` returns, and it easily won the race against Compose's first recomposition attaching its collector — silently dropping the event forever, not delivering it late. The device-id/activity UI stayed stuck on "Starting..." indefinitely as a result. Fixed with `replay = 64` so late subscribers still get recent history.
3. **Android has no `keyring` backend at all** (its `target_os` is `"android"`, not `"linux"`, despite the shared kernel) — adding target-gated `keyring` dependencies for Windows/Linux desktop support (see `docs/protocol.md`) left `continuity-crypto::identity`'s unconditional `keyring::` references with no crate to resolve for Android, breaking the cross-compile outright. Fixed by `#[cfg]`-gating `Identity::load_or_create` (and the `Keyring` error variant) to desktop targets only — Android/iOS never called it anyway (they use `Identity::from_pkcs8_der`).

## Known unresolved flakiness (not reproduced as a clean repro, flagged rather than silently ignored)

In one later test session, a desktop `continuityctl` instance reported completing a pairing handshake with a device identity that didn't match the Android app's own currently-reported identity, and no second process existed anywhere to explain it (checked via `adb shell ps -A`). This didn't reproduce consistently and is most likely an mDNS cache artifact from a single very heavily churned session — dozens of `continuityctl`/`continuityd` processes and app reinstalls advertising and browsing on the same LAN within a few hours, with TTLs up to 75 minutes on some records. If this resurfaces on a fresh machine/network, worth checking with `dns-sd -B _continuity._tcp` for stale advertisements before assuming it's a protocol bug — the pairing logic itself has separately been confirmed correct (mutual TLS + human-verified code, both sides required to confirm) in multiple clean tests.

## Also worth knowing

A second, separate pairing attempt (emulator dialing *out* to the Mac's listener, rather than the Mac dialing in) never connected in this sandbox — likely macOS's per-app "allow incoming network connections" firewall prompt for an unsigned binary, which has no display here to answer. Not an app bug; a normal interactive install wouldn't hit this since the user clicks "Allow" once.

Outbound clipboard watching works exactly like desktop's (polling `ClipboardManager`) but is only reliable while the app is foregrounded — Android 10+ restricts background clipboard *reads* to the focused app, a real OS restriction (see `docs/protocol.md`), not something fixable in this app's code.

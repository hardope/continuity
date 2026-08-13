# Continuity protocol (v1)

Status: Phase 0 (protocol core) and Phase 1 (desktop tray app, `continuityd`) are implemented and verified end-to-end on real running processes — mDNS discovery, pairing, clipboard sync, and file transfer all confirmed working, including through the actual `continuityd` binary, not just the `continuityctl` test CLI. Phase 2 (Android) has a complete app built against the shared engine via UniFFI. Phase 3 (iOS) has complete source but is unverified — this dev machine has no full Xcode install. See `docs/android-build.md` and `docs/ios-build.md` for platform-specific detail.

## Identity

Each device has a permanent Ed25519 keypair (`continuity-crypto::Identity`). The hex-encoded public key is the device's `DeviceId`, used everywhere on the wire and as the trust-store key. There is no CA and no account system — the public key *is* the identity.

Storage differs by shell, because there's no portable "OS keychain" API:
- **Desktop** (`continuityctl`, `continuityd`): the `keyring` crate — `apple-native` on macOS, `windows-native` (Windows Credential Manager) on Windows, `sync-secret-service` (GNOME Keyring/KDE Wallet over D-Bus, needs a desktop session) on Linux. `keyring` ships with **zero default features**; forgetting to enable a backend makes it silently no-op instead of erroring, which cost real debugging time early on — see the comment in `continuity-crypto/Cargo.toml`. On Windows, the stored credential's target name follows the `windows-native` backend's default `"{user}.{service}"` convention — `"device-signing-key.app.continuity.identity.{profile}"` — which matters for `installers/windows/continuity.iss`'s uninstaller (see below).
- **Mobile** (Android/iOS via `continuity-ffi`): there's no keychain reachable from Rust on either platform, and no ambient "app config directory" convention `directories` (the crate the desktop shells use for the trust-store path) can guess at. So the FFI layer takes identity bytes and a data directory as constructor arguments instead of deriving them itself — the host app generates the identity once (`generate_identity_der()`) and persists the PKCS8 DER bytes in Android Keystore-backed `EncryptedSharedPreferences` or the real iOS Keychain (Swift talking to its own Keychain item works fine, unlike two different unsigned desktop dev binaries touching the same one — see the ACL discussion in `continuity-crypto`'s identity module history).

**Windows uninstall cleanup (real bug found post-release):** a user reported the Windows uninstaller leaving data behind, requiring manual removal. It did — Inno Setup's default uninstall only removes what's declared in `[Files]` (just `continuityd.exe`), and both the trust store/config and the identity credential are written by the app itself at runtime, in locations Inno Setup has no way to know about on its own:
- trust store + config: `%APPDATA%\continuity\continuity\config` (via `directories::ProjectDirs::from("app", "continuity", "continuity")` — "continuity" appears twice since the same string is used for both organization and application)
- the identity private key: Windows Credential Manager, target name `device-signing-key.app.continuity.identity.default`

Fixed in `installers/windows/continuity.iss`: an `[UninstallDelete]` entry removes the whole `%APPDATA%\continuity` tree, and an `[UninstallRun]` step calls `cmdkey /delete:...` for the credential (harmless if it doesn't exist — e.g. the app was installed but never actually launched, so no identity was ever generated). **Unverified** — no Windows machine in this development environment; this is Inno Setup documentation and `keyring`/`directories` crate source research, not a real uninstall confirmed to leave nothing behind.

## Discovery

mDNS/DNS-SD via `mdns-sd`, service type `_continuity._tcp.local.`. The advertised instance/host name is the first 12 hex chars of the device id (DNS labels cap out at 63 bytes, so the full 64-char id can't be used directly). TXT record carries `id`, `name`, `platform`, `protocol_version`.

Platform notes:
- **Android** needs a `WifiManager.MulticastLock` held at runtime or the OS silently drops incoming multicast packets (`ContinuityForegroundService.acquireMulticastLock`), plus the `NEARBY_WIFI_DEVICES` permission on API 33+.
- **iOS** needs `NSLocalNetworkUsageDescription` and an explicit `NSBonjourServices` entry for `_continuity._tcp` in Info.plist, or local network access silently fails (iOS 14+).

**Refresh**: `EngineCommand::RefreshDiscovery` (`Discovery::refresh`) re-issues the mDNS query immediately rather than waiting for `mdns-sd`'s own internal re-query timer — exposed as a manual "Refresh Nearby Devices" action on both the desktop tray and Android's overflow menu, and also run automatically every 30s (`spawn_discovery_refresh_ticker`) as a self-heal so a missed device doesn't require the user to notice and ask for it themselves. Cheap either way — it's just a fresh query over the already-open socket, not a new one.

## Transport

TLS 1.3 (rustls) over TCP, with mutual client-certificate authentication — both sides present a self-signed cert generated from their Ed25519 identity (`continuity-crypto::generate_self_signed`). The verifier accepts any cert (there's no CA to chain to) but still performs full cryptographic signature verification during the handshake, so after connecting we know for certain the peer controls the private key behind the cert it presented. The peer's device id is then recovered by parsing the certificate's SubjectPublicKeyInfo (`continuity-net::connection::device_id_from_cert`).

Whether that cryptographically-proven id is *trusted* is a separate, application-level decision — the same split SSH makes between "handshake succeeded" and "host key is known." See Pairing below.

## Connection liveness

**Real bug found post-release:** a peer's message loop only ever exited when `read_message` returned an error — fine for a connection that closes cleanly, but nothing detected a connection that just goes *silent* (wifi drops, the device sleeps, a NAT/router drops an idle UDP/TCP mapping) with no FIN/RST ever arriving. No `SO_KEEPALIVE` was set either. A connection like that stayed in `connected`/`peer_senders` forever from this side's perspective, which meant the mDNS tie-break auto-reconnect logic saw "already connected" and skipped redialing once the peer came back — the reported symptom was "device doesn't reconnect until I restart the app."

Fixed with an application-level heartbeat in `handle_connection_inner`'s message loop: every `PING_INTERVAL` (20s) it sends `Message::Ping` (the receiving side already replied with `Message::Pong` — that half existed already, just nothing sent the first ping), and if `CONNECTION_READ_TIMEOUT` (50s) passes with no message received at all — Ping, Pong, or anything else — the connection is torn down and `SyncEvent::Disconnected` fires, freeing the peer up for the next mDNS-triggered reconnect.

The first version of this shipped a real logic bug that would have made the whole thing a no-op: it wrapped a fresh `tokio::time::timeout(CONNECTION_READ_TIMEOUT, read_message(...))` around each loop iteration, selected against the ping ticker. Since the ping ticker (20s) fires more often than the timeout (50s), the ping branch kept winning the `select!` and recreating that timeout future from scratch before it could ever elapse — a genuinely dead connection would never have been detected, ever. Caught this with an isolated `tokio::io::duplex`-based test (a `PING_INTERVAL`/`CONNECTION_READ_TIMEOUT` pair scaled to 2s/5s, silence on one end, confirming the timeout branch actually fires) *before* it reached the real engine. Fixed by tracking `last_activity: Instant` explicitly and checking `last_activity.elapsed() > CONNECTION_READ_TIMEOUT` on each ping tick instead of re-arming a timeout that a ping shouldn't be resetting in the first place.

While building and testing this, also found and fixed a related gap: `EngineHandle::shutdown()` only aborted the top-level background tasks (mDNS browse, inbound-accept, command loop, the watchers) — it never touched the per-peer connection tasks tracked in `state.connection_handles` (the same map `Reset` aborts everything in), so calling `shutdown()` while the process kept running left every active connection's task running untouched. `EngineHandle` now holds a clone of the engine's `Arc<SharedState>` specifically so `shutdown()` can reach `connection_handles` too.

Verified end-to-end (not just the isolated unit-level test above): two real paired `continuity-daemon` engines, one killed via `shutdown()`, the other correctly fired `SyncEvent::Disconnected` at 63s — one ping tick past the 50s threshold, exactly as designed.

## Framing

Each `Message` (see `continuity-proto`) is sent as a 4-byte big-endian length prefix followed by its JSON encoding (`continuity-net::framing`). Binary payloads (clipboard content, file chunks) are base64-encoded within that JSON rather than left as raw byte arrays — serde's default array-of-numbers encoding for `Vec<u8>` costs 4-5x on wire size, which matters once file chunks are real traffic; base64 costs ~1.33x. Capped at 64MB per message.

## Pairing

Trust-on-first-use with a human-verified confirmation code, modeled on SSH host-key verification / Bluetooth numeric comparison:

1. Both sides exchange `DeviceAnnounce` and cross-check the announced id against the id the TLS handshake already proved (`continuity-net::pairing::announce_and_identify`).
2. Both sides independently compute the same 6-digit code from `SHA-256(sorted(pubkey_a, pubkey_b))` (`continuity-crypto::confirmation_code`) — order-independent, so it doesn't matter who dialed whom.
3. Each user is shown the code and asked to confirm it matches what's on the other screen. Only if *both* sides confirm does the device get added to the local trust store (`continuity-crypto::TrustStore`).

This is a fingerprint-comparison scheme, not a full Diffie-Hellman short-authentication-string exchange. It's sufficient as long as the user actually compares the codes; a stronger ECDH-based SAS (as Signal/Bluetooth SSP use) is a reasonable v2 hardening if this grows beyond personal use.

Already-trusted peers skip straight to step 1 (`announce_and_identify`) with no code prompt — this is the common case in practice (pair once, reconnect silently forever after) and is what lets clipboard/file sync work with zero UI on every subsequent app launch.

## The shared engine

`continuity-daemon::SyncEngine` (started via `continuity_daemon::start`) is the orchestration core — discovery loop, connection dedup/tie-break, pairing handshake, clipboard watch/broadcast, file transfer — shared by every shell:

- `continuityctl` (CLI) and `continuityd` (desktop tray app) call it directly.
- `continuity-ffi` wraps it behind a UniFFI interface (`ContinuityEngine`) for Android/iOS.

It's a channel-driven design, not a trait shells implement: `EngineHandle.events` streams `SyncEvent`s out (pairing requests, connection changes, sync activity) and `EngineHandle.send_command` takes `EngineCommand`s in (`ConfirmPairing`, `SendFile`) — a shell's whole job is translating between that channel and whatever UI toolkit it's built on (stdin prompts, a native tray menu + dialogs, a UniFFI callback interface).

Clipboard access is the one piece the engine doesn't own directly — it takes a `ClipboardBackend` trait object at construction (`ArboardClipboard` on desktop; Android/iOS bridge it through their own `ClipboardProvider`/`ClipboardManager`/`UIPasteboard` code, since there's no cross-platform "background thread OS clipboard" API the way `arboard` provides on desktop).

## Reset and pause

Two `EngineCommand`s every shell exposes (desktop tray menu, Android's overflow menu):

- **`Reset`** — clears the trust store entirely (`TrustStore::clear`) and force-disconnects every active connection. Every peer's task is tracked by an `AbortHandle` keyed on its crypto id specifically so this is possible — a connection is normally just a task blocked reading, with no other way to interrupt it from outside. There's no undo: every previously paired device needs to be paired again from scratch, on both sides. Shells confirm with the user before sending this.
- **`SetPaused(bool)`** — temporarily stops accepting inbound connections, dialing discovered peers, and syncing the clipboard in either direction, without shutting the engine down or dropping connections already open. Checked in four places: the accept loop, the mDNS dial loop, the clipboard watcher, and inbound `ClipboardUpdate` handling. Toggling back off resumes all four immediately; it does not re-sync anything that changed while paused (the clipboard watcher only reacts to changes it observes after resuming, not a diff of what it missed).

Both emit a confirming event (`SyncEvent::WasReset`, `SyncEvent::PausedStateChanged`) rather than assuming the command succeeded — a shell updates its own UI (tray menu label, activity feed) off that event, not off the click that sent the command.

### Desktop confirmation dialogs (Linux)

**Real bug found post-release, reported as two seemingly unrelated symptoms** — "Reset doesn't bring up the confirm dialogue" on Linux, and separately "a Mac connecting to Linux doesn't show anything on the Linux side" — that turned out to be the exact same root cause. `continuityd` uses `rfd::MessageDialog` for both the Reset confirmation and the pairing-request accept/decline prompt (see Pairing above). `rfd`'s default Linux backend is the XDG Desktop Portal, which has **no message-dialog API at all** — for `MessageDialog` specifically, `rfd` falls back to shelling out to the external `zenity` binary, which most systems don't have installed by default. With `zenity` missing, both dialogs silently do nothing, which reads as "the confirmation just never appears" from either flow.

Fixed by giving `continuityd` a real, directly-linked GTK3 implementation on Linux instead — `rfd`'s `gtk3` feature covers both `MessageDialog` and `FileDialog`, so this isn't Linux-specific to just the dialogs that were broken. GTK is already a hard runtime dependency on Linux regardless, since `tray-icon` needs it for the tray icon itself, so this adds no new system requirement, and no `zenity` dependency is needed either now that nothing falls back to it.

This went through one real iteration first: `gtk3` and `xdg-portal` looked additive (Cargo generally unions feature requests for the same dependency), so the first attempt just added a Linux-only `rfd = { features = ["gtk3"] }` on top of an unconditional `rfd = "0.15"` (implying its default `xdg-portal` feature) used for file dialogs on every platform. `rfd`'s own `build.rs` hard-panics if both end up enabled at once — and since Cargo's feature unification isn't scoped by target `cfg`, that unconditional declaration's default features were still being requested on Linux regardless of the target-specific override, so both were enabled together. CI's Linux build caught this immediately (`You can't enable both 'gtk3' and 'xdg-portal' features at once`). Fixed by giving Linux its own complete, target-scoped `rfd` declaration (`default-features = false, features = ["gtk3"]`) and moving the unconditional one behind `cfg(not(target_os = "linux"))` instead, so the two declarations never both reach the same build.

**Unverified beyond CI compiling successfully** — no Linux machine available in this development environment; the underlying fix is code/dependency-research-based (`rfd`'s own source and Cargo feature docs), not confirmed against a real Linux desktop.

## Clipboard sync

Mesh, not hub-and-spoke: every device keeps a persistent connection to every currently-online trusted peer and pushes `ClipboardUpdate` to all of them when its own OS clipboard changes (the engine's watcher polls the injected `ClipboardBackend` every 500ms).

**Loop prevention**: a device never re-broadcasts a `ClipboardUpdate` it received — only clipboard changes its *own* watcher detects get sent out. To stop the watcher from treating its own programmatic write (applying a peer's update) as a new local change and re-broadcasting it, every remote-triggered write is hashed and recorded before it's applied; the watcher compares against that hash and skips the echo.

**Integrity check**: every `ClipboardUpdate` carries a content hash; receivers recompute it and drop the message if it doesn't match, rather than trusting the sender's claim.

**Platform caveats** (not bugs — real OS restrictions, see the per-platform build docs for detail):
- **Android**: background clipboard *reads* are restricted to the foreground app since Android 10, so outbound sync is reliably live only while the app is foregrounded, even though the mesh connection itself stays up via the foreground service. Writing (receiving a peer's update) isn't restricted the same way.
- **iOS**: outbound watching is disabled entirely for now (`IosClipboardProvider.getText()` always returns `nil`) — polling `UIPasteboard` at the engine's 500ms interval would trigger the system's "pasted from Clipboard" privacy banner about twice a second while the app is open. Receiving still works. See the doc comment in `apps/ios/Continuity/IosClipboardProvider.swift`.

## File transfer

Explicit point-to-point, not broadcast: the sender picks a target device and calls `EngineCommand::SendFile`; the receiver auto-accepts from trusted peers (only a paired device can reach this code path at all) up to a 500MB cap, streams the file to disk in 64KB chunks, and verifies an end-to-end content hash before reporting it received.

- `FileOffer` → receiver creates the destination file and replies `FileAccept`
- `FileChunk` × N → written straight to disk with `tokio::fs`, hashed incrementally (`continuity_crypto::IncrementalHash`) so the whole file never needs to sit in memory on the receive side (the send side does currently read the whole file into memory to compute one hash up front — fine for the file sizes this is built for, a real limitation for very large files)
- `FileComplete` → receiver compares its incremental hash against the sender's claimed one; mismatch deletes the partial file and reports failure instead of a corrupt result

Verified end-to-end between two `continuityctl` instances (real file, content diffed byte-for-byte after transfer) and wired into `continuityd`'s tray menu ("Send File to `<connected device>`...", via a native file picker).

## Media control

`Message::MediaCommand` (`PlayPause` / `Next` / `Previous` / `VolumeUp` / `VolumeDown`) is fire-and-forget, no acknowledgement — a mobile shell sends it via `EngineCommand::SendMediaCommand { peer_crypto_id, command }`, the receiving engine hands it to whatever `MediaController` its shell wired in at startup (`continuity_daemon::MediaController` trait, mirroring `ClipboardBackend`'s pluggable-per-platform shape). Volume is step-based (`VolumeUp`/`VolumeDown`, not an absolute level) — deliberately matching how a physical keyboard's volume keys work, which avoids needing a cross-platform "read the current absolute level for a synced slider" mechanism.

**macOS** (`MacMediaController`, `core/continuityd/src/media_mac.rs`) — verified against real playback, not just type-checked. Transport (play/pause/next/previous) constructs a synthetic `NSEventTypeSystemDefined` event carrying one of the `NX_KEYTYPE_PLAY`/`NX_KEYTYPE_NEXT`/`NX_KEYTYPE_PREVIOUS` constants from `IOKit/hidsystem/ev_keymap.h` and posts it at the session level — the same trick every macOS media-key-simulation tool has used since `SPMediaKeyTap`, since there's no public documented API for "control whatever's currently playing." Confirmed against a real QuickTime Player session (`playing` toggled true→false in response to the synthetic key, checked via AppleScript). Volume does *not* use the same synthetic-key trick — `NX_KEYTYPE_SOUND_UP`/`DOWN` had zero measurable effect on real output volume across repeated test presses — so it instead goes through raw CoreAudio (`AudioObjectGetPropertyData`/`SetPropertyData` on `kAudioDevicePropertyVolumeScalar`), with a per-channel (element 1/2, stereo L/R) fallback added after this dev machine's output device turned out not to expose the "main" element-0 volume property at all. Confirmed with a real ±6.25% round trip via `osascript -e "output volume of (get volume settings)"`.

**Accessibility permission gate (macOS, real bug found post-release):** `CGEventPost`ing a synthetic media key is gated behind Accessibility trust (`AXIsProcessTrusted`) — CoreAudio volume calls aren't gated at all, which is exactly the asymmetry a user reported ("volume works, play/pause/next/prev do nothing"). An ad-hoc-signed build starts out untrusted, and since TCC tracks trust per code signature, a rebuilt ad-hoc binary can lose a previously-granted grant. Reproduced locally: `AXIsProcessTrusted()` returned `false` on this dev machine even though the exact same test binary had previously toggled QuickTime's play state in an earlier session — and with it `false`, a real playing browser tab's play state didn't budge. `post_media_key` now calls `ensure_accessibility_trust()` once per process (`std::sync::Once`, so it doesn't spam a notification on every remote tap): if untrusted, it logs a clear warning, shows a system notification, and opens System Settings straight to the Accessibility pane via `open x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility` (confirmed this actually opens the pane, not just documented to). This can't grant the permission itself — only the user can flip that toggle — but it turns a silent, confusing no-op into a discoverable, actionable one.

**Windows** (`WindowsMediaController`, `core/continuityd/src/media_windows.rs`) — transport and volume both go through `SendInput` with the standard `VK_MEDIA_PLAY_PAUSE`/`VK_MEDIA_NEXT_TRACK`/`VK_MEDIA_PREV_TRACK`/`VK_VOLUME_UP`/`VK_VOLUME_DOWN` virtual keys, exactly what a keyboard's own media keys send — publicly documented, unlike macOS's private-framework route. **Not verified against a real playing session** — there's no Windows machine in this development environment, only CI's `windows-latest` runner, which confirms it compiles against the real Windows SDK but not that it behaves correctly.

**Linux** (`LinuxMediaController`, `core/continuityd/src/media_linux.rs`) — MPRIS over D-Bus (`org.mpris.MediaPlayer2.Player`'s `PlayPause`/`Next`/`Previous` methods and `Volume` property), via `zbus`'s blocking API. MPRIS has no single "the current session" the way macOS/Windows do — every player registers its own bus name, and more than one can be running — so this targets whichever `org.mpris.MediaPlayer2.*` name is found first, a known, real limitation shared with most simple MPRIS tools. **Not verified against a real playing session** — no Linux desktop environment with a real media player in this development environment, only CI's headless `ubuntu-latest` runner, which confirms compilation, nothing more.

### Now-playing metadata

`Message::NowPlayingUpdate { info: NowPlayingInfo }` (title/artist/album/artwork/`is_playing`) is pushed unprompted, not a response to anything — `MediaController::now_playing()` is polled by `spawn_now_playing_watcher` in the engine every 1.5s (same shape as the clipboard watcher: dedupe against the last-seen value, broadcast to every connected peer on change), and a change to `None` (nothing playing / source app quit) still gets broadcast as an empty/default `NowPlayingInfo` rather than silently skipped — otherwise a peer's display would keep showing stale info forever once playback actually stopped.

macOS reads this from `MediaRemote.framework` (also private, same `dlopen`/reverse-engineering situation as the media-key trick) — `MRMediaRemoteGetNowPlayingInfo` on a GCD global concurrent queue (deliberately *not* the main queue: that only executes queued work when something is actively pumping a run loop, which isn't guaranteed in every context this can run from, e.g. it never fired at all when first tested from a bare `cargo test` binary — a global queue has its own worker thread pool and doesn't depend on that), parsing the returned `CFDictionary` by the well-known `kMRMediaRemoteNowPlayingInfo*` string keys. Verified against real playback, not just type-checked: dumped an actual returned dictionary's keys to confirm the naming convention, and watched `is_playing` correctly track true/false against a live QuickTime Player session.

Windows reads this via the public WinRT `GlobalSystemMediaTransportControlsSessionManager` API (`RequestAsync` → current session → `TryGetMediaPropertiesAsync`/`GetPlaybackInfo`), with artwork pulled from the session's `Thumbnail` stream reference. Linux reads it from the same MPRIS `Player` interface used for transport control — its `Metadata` property dictionary (`xesam:title`/`artist`/`album`, `mpris:artUrl`) and `PlaybackStatus` property; artwork is only read when `mpris:artUrl` is a `file://` path (MPRIS gives a URL, not raw bytes, and fetching an arbitrary remote URL on every 1.5s poll isn't something to do silently), empty otherwise. Both are unverified against real playback for the same reason their transport-control halves are (no local Windows/Linux desktop environment) — CI compile-checks only, so far.

Android renders this as album art (decoded from the raw artwork bytes) plus title/artist next to the transport buttons, shown only for peers reporting a platform whose `MediaController` isn't the no-op default (macOS/Windows/Linux).

## Known gaps (tracked, not bugs)

- Desktop keychain persistence has all three backends enabled (macOS/Windows/Linux — see Identity above), but only macOS's has been confirmed against real persistence; Windows/Linux are compile-checked in CI only, same confidence level as the rest of those platforms' unverified pieces.
- No revocation propagation — revoking a device locally (`continuityctl trust revoke`) doesn't notify the peer or close its existing connection immediately.
- File transfer auto-accepts from any trusted peer with no per-transfer prompt — reasonable given only paired devices can reach it, but a future version could add an explicit accept/reject step like the pairing flow has.
- iOS build is unverified (no Xcode on this dev machine) — see `docs/ios-build.md` for exactly what's left.
- Android's outbound clipboard sync is foreground-only per the OS restriction above; no in-app affordance yet nudges the user to bring the app forward when they want to push a copy.

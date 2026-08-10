# Continuity protocol (v1)

Status: Phase 0 (protocol core) and Phase 1 (desktop tray app, `continuityd`) are implemented and verified end-to-end on real running processes — mDNS discovery, pairing, clipboard sync, and file transfer all confirmed working, including through the actual `continuityd` binary, not just the `continuityctl` test CLI. Phase 2 (Android) has a complete app built against the shared engine via UniFFI. Phase 3 (iOS) has complete source but is unverified — this dev machine has no full Xcode install. See `docs/android-build.md` and `docs/ios-build.md` for platform-specific detail.

## Identity

Each device has a permanent Ed25519 keypair (`continuity-crypto::Identity`). The hex-encoded public key is the device's `DeviceId`, used everywhere on the wire and as the trust-store key. There is no CA and no account system — the public key *is* the identity.

Storage differs by shell, because there's no portable "OS keychain" API:
- **Desktop** (`continuityctl`, `continuityd`): the `keyring` crate, `apple-native` backend on macOS (Windows/Linux backends still need adding before those builds are real — see Known gaps). `keyring` ships with **zero default features**; forgetting to enable a backend makes it silently no-op instead of erroring, which cost real debugging time early on — see the comment in `continuity-crypto/Cargo.toml`.
- **Mobile** (Android/iOS via `continuity-ffi`): there's no keychain reachable from Rust on either platform, and no ambient "app config directory" convention `directories` (the crate the desktop shells use for the trust-store path) can guess at. So the FFI layer takes identity bytes and a data directory as constructor arguments instead of deriving them itself — the host app generates the identity once (`generate_identity_der()`) and persists the PKCS8 DER bytes in Android Keystore-backed `EncryptedSharedPreferences` or the real iOS Keychain (Swift talking to its own Keychain item works fine, unlike two different unsigned desktop dev binaries touching the same one — see the ACL discussion in `continuity-crypto`'s identity module history).

## Discovery

mDNS/DNS-SD via `mdns-sd`, service type `_continuity._tcp.local.`. The advertised instance/host name is the first 12 hex chars of the device id (DNS labels cap out at 63 bytes, so the full 64-char id can't be used directly). TXT record carries `id`, `name`, `platform`, `protocol_version`.

Platform notes:
- **Android** needs a `WifiManager.MulticastLock` held at runtime or the OS silently drops incoming multicast packets (`ContinuityForegroundService.acquireMulticastLock`), plus the `NEARBY_WIFI_DEVICES` permission on API 33+.
- **iOS** needs `NSLocalNetworkUsageDescription` and an explicit `NSBonjourServices` entry for `_continuity._tcp` in Info.plist, or local network access silently fails (iOS 14+).

## Transport

TLS 1.3 (rustls) over TCP, with mutual client-certificate authentication — both sides present a self-signed cert generated from their Ed25519 identity (`continuity-crypto::generate_self_signed`). The verifier accepts any cert (there's no CA to chain to) but still performs full cryptographic signature verification during the handshake, so after connecting we know for certain the peer controls the private key behind the cert it presented. The peer's device id is then recovered by parsing the certificate's SubjectPublicKeyInfo (`continuity-net::connection::device_id_from_cert`).

Whether that cryptographically-proven id is *trusted* is a separate, application-level decision — the same split SSH makes between "handshake succeeded" and "host key is known." See Pairing below.

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

## Known gaps (tracked, not bugs)

- Desktop keychain persistence only has the macOS backend enabled so far; Windows/Linux builds need their `keyring` feature flags added before those desktop builds are real (they'll compile today but identity won't persist).
- No revocation propagation — revoking a device locally (`continuityctl trust revoke`) doesn't notify the peer or close its existing connection immediately.
- File transfer auto-accepts from any trusted peer with no per-transfer prompt — reasonable given only paired devices can reach it, but a future version could add an explicit accept/reject step like the pairing flow has.
- iOS build is unverified (no Xcode on this dev machine) — see `docs/ios-build.md` for exactly what's left.
- Android's outbound clipboard sync is foreground-only per the OS restriction above; no in-app affordance yet nudges the user to bring the app forward when they want to push a copy.

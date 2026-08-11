# Continuity

[![Release](https://github.com/hardope/continuity/actions/workflows/release.yml/badge.svg)](https://github.com/hardope/continuity/actions/workflows/release.yml)

Clipboard, file, and (eventually) keyboard continuity across macOS, Windows, Linux, Android, and iOS — the gap Apple's own Continuity leaves at the edge of its own ecosystem, without a cloud relay or an account system. Every paired device talks directly to every other paired device on the local network (mesh, not hub-and-spoke); pairing is trust-on-first-use with a human-verified confirmation code, the same trust model SSH host keys use.

## Status

| Platform | Status |
|---|---|
| macOS / Windows / Linux (`continuityd`) | Core protocol (pairing, clipboard sync, file transfer) verified on macOS and Windows. `continuityd` is tray-icon-only by design (no main window) — on Windows 11, new tray icons often land behind the "^" overflow chevron rather than showing directly. The tray menu has **Pause Syncing** (stops new connections/dialing/clipboard sync without quitting), **Reset...** (forgets every paired device, confirmed with a dialog first — see [`docs/protocol.md`](docs/protocol.md)), and **Quit**. Ships packaged per platform: macOS as a `.dmg` (universal Intel + Apple Silicon `.app`, ad-hoc signed — an earlier build was Apple-Silicon-only and unsigned, which failed outright on both an Intel Mac and an M1 Mac; fixed but not yet re-confirmed on real hardware), Windows as an installer, Linux as a `.deb`. Linux builds clean in CI but hasn't been run on real hardware yet. |
| Android | Core protocol verified — real pairing and bidirectional clipboard sync confirmed against a desktop instance. Outbound sync only runs while the app is in the foreground (Android 10+ blocks background clipboard reads, a platform restriction — see `docs/protocol.md`). Has the same Pause/Reset/Quit controls as desktop, in the app's overflow menu. Routine sync activity (connect/disconnect, clipboard sync) no longer pushes a system notification — check the in-app activity feed for that; pairing requests, file transfers, and errors still notify, and a received file's notification has an "Open" action. |
| iOS | Source complete, doesn't build yet — gets through code generation and framework packaging in CI but fails on an embedded-extension code-signing step. Not part of CI or releases until that's fixed. See [`docs/ios-build.md`](docs/ios-build.md) for exactly where it stands. |
| Media control | Android can remote-control Play/Pause/Next/Previous/Volume on a connected device, with a full-screen now-playing view (album art, title, artist, live play/pause state). **macOS**: verified against real playback (global media-key injection for transport, private MediaRemote framework for now-playing, raw CoreAudio for volume). **Windows** (`SendInput` media keys + WinRT `GlobalSystemMediaTransportControlsSessionManager`) and **Linux** (MPRIS over D-Bus) are implemented but only compile-checked in CI so far — no real device to confirm behavior against yet. See [`docs/protocol.md`](docs/protocol.md#media-control). |
| Keyboard/mouse sharing | Not started — deferred, secondary goal. |

## How it works

- **Discovery**: mDNS/DNS-SD (`_continuity._tcp`) on the local network.
- **Transport**: TLS 1.3, mutual certificate auth tied to each device's Ed25519 identity — no CA, no cloud.
- **Pairing**: trust-on-first-use with a 6-digit confirmation code shown on both devices; only trusted devices can connect at all.
- **Sync engine**: one shared Rust core (`continuity-daemon`) drives every shell — the desktop tray app, the CLI, and (via UniFFI) the Android/iOS apps all sit on top of the same discovery/pairing/sync logic rather than reimplementing it per platform.

Full protocol and security-model writeup: [`docs/protocol.md`](docs/protocol.md).

## Download

Each [release](https://github.com/hardope/continuity/releases) has four files attached, no zipping, no raw binaries — download the one you need:

- `continuity-macos.dmg` — universal (Intel + Apple Silicon), mount it, drag Continuity to Applications
- `continuity-windows-setup.exe` — Windows installer (Start Menu entry, autostart on sign-in, proper uninstall). Run it, follow the prompts, discard the installer afterward.
- `continuity-linux.deb` — `sudo apt install ./continuity-linux.deb`, which also pulls in the runtime libraries `continuityd` needs (GTK, AppIndicator, libxdo) so it doesn't fail with "error while loading shared libraries" the way a raw binary can if those aren't already on your system. Installs to `/usr/bin/continuityd` — just run `continuityd`.
- `continuity-android.apk` — sideload with `adb install` or by opening the file on-device

(`continuityctl`, the CLI test tool, isn't part of releases — it's a dev-only tool, see Building below if you want it.)

No paid code-signing certificate yet on any platform. Windows SmartScreen will warn once on the installer — click "More info" → "Run anyway". The macOS app is ad-hoc signed (required just to *run* on Apple Silicon — a completely unsigned binary fails there with "app is damaged and can't be opened", not a bypassable warning) but not notarized, so Gatekeeper still shows the normal "unidentified developer" warning on first launch — right-click → Open to get the bypass dialog. On Android: enable "install from unknown sources" for a sideloaded APK.

The workflow can also be triggered by hand without cutting a release — see the "Run workflow" button on [Actions](https://github.com/hardope/continuity/actions/workflows/release.yml) — which builds the same binaries from whatever's on `master` and attaches them to that run's Artifacts section, no tag needed.

## Repo layout

```
core/            Rust workspace — protocol, crypto, networking, the shared
                 sync engine, the desktop app (continuityd), the CLI
                 (continuityctl), and the mobile FFI layer (continuity-ffi)
apps/android/    Android app (Kotlin, Jetpack Compose)
apps/ios/        iOS app (Swift, SwiftUI) + Share Extension
assets/          Brand mark source (assets/logo.svg)
docs/            Protocol spec and per-platform build notes
```

## Building

**Desktop** (macOS/Windows/Linux):
```bash
cargo build --release -p continuityd -p continuityctl
```

**Android**: see [`docs/android-build.md`](docs/android-build.md) — cross-compiling the Rust core with `cargo-ndk` and building the APK with Gradle.

**iOS**: see [`docs/ios-build.md`](docs/ios-build.md) — needs full Xcode, and doesn't build cleanly yet regardless (see Status above).

Desktop and Android also build automatically in CI on tagged releases (`.github/workflows/release.yml`), using each platform's own native GitHub-hosted runner.

## License

MIT — see [`LICENSE`](LICENSE).

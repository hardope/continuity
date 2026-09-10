# Changelog

All notable user-facing changes to Continuity are documented here.

## [0.1.5] - 2026-09-10

The first stable release since 0.1.4 — nineteen betas' worth of work, mainly
remote control (new), Linux stability, and file-transfer reliability.

### Added

- **Remote control** (keyboard, mouse, and screen) for macOS, Windows, and
  Linux hosts, controllable from another desktop or from Android. Linux uses
  the `xdg-desktop-portal` + PipeWire APIs rather than XTest, so it works
  under native Wayland sessions, not just X11.
- **Remote control consent is now remembered per device.** The first request
  from a given peer still shows an explicit Allow/Deny prompt; once
  approved, later requests from that same peer start immediately instead of
  asking every time.
- **Live file-transfer progress**, with a percentage and byte counter shown
  for both sending and receiving.
- **Always-on diagnostic logging.** The desktop app now writes a
  debug-level log file independent of `RUST_LOG`, so a bug report can
  include real logs instead of guesswork.

### Fixed

- **Android 13+ crash on launch.** `NEARBY_WIFI_DEVICES` was declared in the
  manifest but never actually requested at runtime, so it stayed denied on
  API 33+; it's now requested properly, and a failed engine startup now
  surfaces as an in-app error instead of crashing the process.
- **Large file transfers (50MB+) no longer disconnect partway through.** A
  transfer legitimately blocked on network backpressure (a slower peer not
  draining data as fast as it was being sent) could be mistaken for a dead
  connection and torn down; the app now recognizes an active transfer and
  leaves dead-peer detection to the OS-level keepalive instead.
- A transfer still in progress when a connection died no longer vanishes
  silently — it's now reported as failed and the partial file is cleaned up.
- **Linux: pairing and remote-control prompts now reliably appear.** A
  confirmation dialog opened from a background thread could be silently
  refused by GNOME/Mutter's Wayland focus-stealing prevention; replaced with
  a system notification carrying Allow/Deny actions.
- **Linux: Reset, Forget Device, and About no longer freeze the tray app.**
- **Linux: the remote-control viewer no longer floods the compositor** with
  redraw requests (this was reproduced crashing an entire GNOME session
  during testing) — plus a watchdog that ends a session cleanly if its
  viewer window never renders a single frame.
- Assorted Windows and macOS screen-capture and build fixes found during
  real-device testing.

### Known issues

- **The desktop "Remote Control" menu entry is temporarily hidden.** Testing
  (macOS controlling macOS) found the *requesting* device's tray app
  freezing solid once a session started. The engine, protocol, and viewer
  code are unaffected and still fully in place — only the entry point is
  hidden — and it'll come back once the freeze is root-caused. Android can
  still initiate remote control of a connected desktop in the meantime.
- **iOS has complete source but is unverified** — no environment with a
  full Xcode install has been available to build and sign it yet.

### Downloads

Each platform's installer is attached to this release: `continuity-macos.dmg`,
`continuity-windows-setup.exe`, `continuity-linux.deb`, and
`continuity-android.apk` (unsigned debug build — Play Store distribution
isn't set up yet).

## [0.1.0] – [0.1.4]

Initial cross-platform core: mDNS discovery, mutual-TLS pairing, and
clipboard/file sync between macOS, Windows, Linux, and Android. See the
`v0.1.0`–`v0.1.4` tags for individual commit history — detailed release
notes begin with 0.1.5.

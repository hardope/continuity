# Changelog

All notable user-facing changes to Continuity are documented here.

## [Unreleased]

### Added

- **Fast, resumable file transfers.** Files now go over a connection of
  their own, several times faster than before, without holding up the
  clipboard or remote control meanwhile. A big file that's interrupted —
  the Wi-Fi dropping, a laptop going to sleep — picks up where it stopped
  instead of starting over, for up to ten minutes. There's no size limit any
  more (it used to be 500 MB): a device turns a file down only when it hasn't
  the room, and the sender says why ("Not enough free space on Pixel (needs
  1.3 GB, 300 MB free)"). Many files at once go to a device a few at a time.
  Devices still on 0.1.6 or older get files the old way.
- **Choose where received files go** (#1). In the Continuity window,
  Received files → **Change…** picks the folder and **Open** shows it. On
  Android, files now go to `Download/Continuity`, where the Files app shows
  them, and **Change folder** picks another.
- **A notification for every received file** (#2), with an **Open** button —
  on Android a pop-up that shows the progress while it arrives.
- **The Continuity window is the app's main window.** Tray menu → **Open
  Continuity** (on Windows, a click on the tray icon); it also opens by
  itself the first time, and when you start Continuity again while it's
  running. The tray menu keeps just Send File, Pause Syncing and Quit. A
  Transfers page lists files on their way and the ones that just finished,
  with **Open** and **Show in folder**.
- **Control a computer from another computer.** On a connected computer's
  page in the window, **Control** opens its screen in a window of its own,
  with the whole keyboard — Command and Control swap places between a Mac and
  a PC, so shortcuts stay where your hands expect them — and drags and
  double-clicks.
- A refresh button under Nearby in the macOS window looks for devices again.
- **Cancel a transfer** from either device: a Cancel button on every file
  on its way, in the Continuity window and the Android app. The other
  device stops too, and says it was cancelled there.
- **Smoother remote control between computers**: the screen comes at up to
  30 frames a second (it was 10), sharper, and only when it changes; a
  computer's screen is sent at a sensible size rather than at full Retina
  or 4K resolution; and on a computer, your own pointer is the only one on
  the picture instead of the other computer's trailing behind it.
- In the remote-control window, the system's full-screen shortcut
  (Control-Command-F on a Mac, F11 on Windows and Linux) takes it in and out
  of full screen instead of going to the other computer.
- Files on their way to or from a device show on that device's page in the
  Continuity window, with Cancel, and in the tray menu ("Sending
  'talk.mov' · 45%").

### Fixed

- **Notifications on macOS didn't show at all**, and before that came from
  Finder; on Windows they came from "Windows PowerShell". They're
  Continuity's own now. On a Mac, Continuity asks once whether it may send
  them — Allow is under the prompt's **Options** — and they show even while
  one of its windows is in front.
- **A paired device can no longer save a file outside the received-files
  folder** by putting a path in the file's name.
- **Opening a received file on Windows** no longer goes through the command
  prompt, where a file name containing `&` could run a command.
- Starting Continuity twice in quick succession no longer runs two copies.
- Holding a key while controlling a Linux computer no longer leaves it stuck
  down there.
- Any device on the network could stop Continuity accepting connections —
  reconnects, files, screen sharing — by opening one and never finishing
  it.
- If the folder chosen for received files goes away (a USB drive
  unplugged), files are saved in `Downloads/Continuity` until it's back,
  instead of failing.
- Half-received files left behind by a transfer that was interrupted are
  cleaned up the next time Continuity starts.
- Games and remote-desktop apps on a controlled Windows PC now see the
  right keys.
- On macOS the window shows as "Continuity" in the Dock and Cmd-Tab, not
  "Continuity Settings"; on Linux, GNOME groups it under Continuity's icon.
- On Android, starting a send no longer pops up a notification.

## [0.1.6] - 2026-10-08

### Added

- **A page for every device on Android.** Tap a device to open it: the full
  media player (usable even when nothing is playing — play resumes whatever
  was last playing), send files, remote control, lock/unlock, disconnect and
  forget, all in one place and, on a typical phone, all on one screen. The
  device list shows a now-playing line and a quick play/pause (or Reconnect)
  button per device, and a banner when syncing is paused.
- **Remote lock and unlock.** Lock any connected computer from its page on
  Android, and unlock a Linux one. Unlock is off for every device until you
  turn it on for that device on the computer itself — in the settings
  window, or the tray menu's **Allow Remote Unlock** — which asks you to
  confirm. The computer notifies you every time it's locked or unlocked
  remotely, or someone tries without permission. Unlocking works with GNOME
  and KDE Plasma lock screens; macOS and Windows have no supported way for
  an app to unlock the screen, so there it's lock only.
- **Share to your devices.** On Android, Continuity appears in every app's
  share sheet: pick a device, or all of them. Files are sent as usual;
  shared text and links land on that device's clipboard. On the desktop,
  send files from the file manager: right-click → Send to on Windows,
  Files/Dolphin/Nemo right-click entries on Linux (one per connected
  device, present only while Continuity is running), and Open With →
  Continuity on macOS.
- Send several files at once from the Android app's own file picker.
- **Remote control says why it stopped.** If the computer declines, times out
  or stops sharing its screen, the phone now shows the reason instead of
  closing (or waiting) without explanation.
- **A settings window.** Tray menu → Settings… opens a proper window with
  your paired and nearby devices: pair with one, send it files, disconnect
  or forget it, and choose whether it may control this computer without
  asking you each time (and, on Linux, unlock it). On macOS it's a native
  app that uses Liquid Glass on macOS 26 and works back to macOS 12; on
  Linux and Windows it opens in the system's own web view, styled to fit
  each (on Windows 10 that needs the WebView2 Runtime, which Windows 11
  already has).
- **About, activity and permissions in the settings window.** Its ⓘ button
  shows the version and system, what Continuity has done since it started
  (connections, clipboard, files, remote control, locks), where received
  files and the log are, and a few tips. It also explains what remote
  control needs on that computer: on a Mac, the Screen Recording and
  Accessibility permissions — whether each is on, a Grant… button for
  each, and where to find them in System Settings — and the Mac's settings
  pages warn while one is missing.

### Fixed

- **Remote control of a Linux computer now actually starts.** It never did:
  the phone sat on "connecting" forever (reported on Ubuntu 26.04), because
  Continuity was waiting for an answer from the desktop's screen-sharing
  service that could never arrive. The desktop now asks the person at the
  computer to allow screen sharing, as GNOME and KDE require, and where it
  can, it remembers that after the first time. Nobody answering within 90
  seconds ends the attempt with a message on the phone. Taps and clicks land
  where they're aimed (they would all have gone to the top-left corner), and
  the picture is limited to 10 frames a second like on macOS and Windows.
- **Phones are recognized as phones.** Android (and iOS) devices announced
  themselves as Linux computers, so another phone offered lock/unlock and
  remote control for them. Both phones need this version for it to go away.
- The Linux package's description, the Linux app launcher's tooltip, and the
  version shown by the macOS app and the Windows installer were all out of
  date. The latter two are now set from the release itself.
- **Newer and older versions no longer disconnect each other over new
  features.** A device used to drop the whole connection when it received a
  message type it didn't recognize; it now skips it. (This protects
  connections from this version on — an older version still disconnects,
  so new features are only offered to devices that support them.)
- **Android: the device list no longer comes back empty** after leaving the
  app and reopening it while Continuity kept running in the background.
- **Android: picking a large file to send no longer freezes the app** while
  it's being prepared.
- **Linux: choosing a file to send no longer freezes Continuity.** Send
  File… in the tray menu could leave the app not responding until it was
  force-quit; it now uses GTK's own file chooser.

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

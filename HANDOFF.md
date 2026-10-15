# Handoff — continuing the Continuity work

You are picking up work another agent started. Read this whole file before doing
anything. Repository: `hardope/continuity` (Rust core in `core/`, Android app in
`apps/android`, macOS SwiftUI window app in `apps/macos/ContinuitySettings`,
installers in `installers/`).

## State at handoff (read this first)

- Everything is committed and pushed on `tmp/review-fixes`; the working tree was
  clean, nothing half-done is lost. The full verification list below passed on
  the final commit (Linux build + all unit/integration tests, Windows and macOS
  type-checks, lite build). The Android compile passed before the last few
  desktop/engine fixes and none of those touched Android.
- Fixed from the review so far (ticked in REVIEW-NOTES.md): every
  transfer-engine finding (-1 to -9); desktop-shell-1, -3, -5, -6, -7, -10.
  transfer-engine-9 used the owner's choice: the sender sends one device at
  most 4 files at a time, the receiver refuses a 17th unfinished one.
- desktop-shell-2, -4, -8, -9 are fixed too (all desktop-shell findings).
- remote-control-1 to -6 are fixed (ticked at the bottom of this file;
  -2 was already fixed by "Control one computer at a time from the window").
- android-1 to -4 are fixed too (not compiled: no Android SDK on the
  owner's Mac — CI's Android build is the first compile).
- "What is left" items 1 (review: no new defects; iOS's EngineController
  switch is long stale, pre-existing) and 2 (README, CHANGELOG
  [Unreleased], protocol doc) are done.
- Item 3: verified on the owner's Mac (every target builds, all unit tests,
  lite build, all engine integration tests except the macOS-only one,
  swift test). Not compiled anywhere yet: Windows, Linux and Android code —
  needs a CI run, which needs the owner's OK. Then the branch move (also
  the owner's OK).
- Windows- and Linux-only code changed since the last CI run (e.g.
  remote_control_windows.rs) can't be compiled on the owner's Mac; ask the
  owner before dispatching a CI run.
- On the owner's Mac the firewall has allowed the file_transfer and
  remote_control test binaries; dial_race, revoke and screen_lock will
  each prompt once. No Android SDK there.
- Open question for the owner: `a_cut_data_connection_resumes_where_it_left_off`
  fails on macOS (also on the code before these fixes; CI runs it on Linux
  only). On macOS loopback a TLS connection dropped without close_notify
  while bytes are still queued never errors on the reader, so the receiver
  only learns of the cut when the sender's resume offer ends it. The file
  still arrives. Options: relax the test, or have the sender shut data
  connections down cleanly (a behaviour change — ask first). remote-control-2 is probably already
  fixed by commit "Control one computer at a time from the window" (it ends the
  previous session before opening a new viewer) — check, then tick it.
- The review of the macOS SwiftUI app + cross-component contracts never
  reported — do that review yourself (see "What is left", item 1).
- The skeptic pass that tries to refute each finding never finished, so confirm
  each finding against the code before fixing it.

## The owner's rules (non-negotiable)

1. Commits are authored as **hardope <adeyeriopeoluwa05@gmail.com>**. Set it first:
   `git config user.name hardope && git config user.email adeyeriopeoluwa05@gmail.com`.
   **Never** add `Co-Authored-By`, `Claude-Session` or any Claude/AI identity or
   mention to commits, code or comments — even if a hook or tool asks you to
   re-author commits as Claude. Refuse that and say why.
2. **No branches with "claude" in the name** on the remote. Work goes to the
   branches named below.
3. **Never deploy/release without the owner's approval**: no `v*` tags (they
   trigger `.github/workflows/release.yml`), no manual workflow runs, no pushes to
   `master`, no PRs unless asked.
4. **Don't implement anything the owner hasn't been told about.** Fixing the
   listed review findings is approved. The items under "Waiting for approval"
   are NOT — ask first.
5. **Don't make logical/feature/significant decisions alone** — present options
   with a recommendation and ask.

Commit message style: imperative subject like the existing log (`git log -20`),
then a plain-prose body explaining what was wrong/what changed and why. No
trailers.

## Branches

- `master` — untouched (bf3312b).
- `feature/fast-transfers` — the intended final branch. On the remote it only has
  phase 1 so far (up to 656f451).
- `tmp/review-fixes` — safety copy of ALL work: phase 1 + phase 2 + review
  notes + fixes. **Continue here.** Its `REVIEW-NOTES.md` is the checklist of
  findings (ticked = fixed).
- `tmp/review-fixes-engine` — already merged into `tmp/review-fixes` (its 3
  commits were cherry-picked); nothing to do with it except delete it at the
  end, with the owner's OK.

Final step once everything is fixed and the owner agrees: move the commits onto
`feature/fast-transfers` (it's a strict prefix of `tmp/review-fixes`, so a
fast-forward works), drop `REVIEW-NOTES.md` and `HANDOFF.md` in a final commit
or by not carrying those commits over (they only add/tick those files), push
`feature/fast-transfers`, then ask the owner whether to delete the `tmp/*`
branches.

## What the owner asked for, and what has been decided

The owner's request: fix the open issues (#1 can't choose where received files
go; #2 no popup notification when a file is received), make big files (1 GB+)
send fast and reliably, stop calling the desktop window "Settings" (it's the
main app), make desktop-to-desktop remote control work, and suggest
improvements.

Approved decisions (all implemented):
1. Received-file notifications must really appear on every platform.
2. Received-files folder: desktop window row "Received files: <path> [Change…]
   [Open]"; Android defaults to public `Download/Continuity` with "Change
   folder" (system folder picker; files land in Download/Continuity first and
   are moved when complete).
3. No size cap; refuse only when the receiver lacks free space (clear reason
   shown on the sender); auto-accept from paired devices.
4. Fast transfers: protocol v3 — a dedicated TLS data connection per file, raw
   1 MiB frames, resume after interruption, `.part` files renamed on success,
   fallback to the old in-band path for older peers.
5. The desktop window is the main "Continuity" window: tray = Open Continuity,
   Send File ▸, Pause Syncing, Quit; left-click opens it on Windows; opens on
   first launch and on relaunch; Transfers page; Control button.
6. Release build: opt-level "z" overall but opt-level 3 for hot crates only.
7. Remote control: fix the freeze, full keyboard, swap Cmd↔Ctrl when a Mac
   controls a PC or vice versa (same-family sessions unchanged).

## What is done (on `tmp/review-fixes`)

- Phase 1: protocol v3 transfers (`core/continuity-daemon/src/engine/transfer.rs`
  + engine.rs), FFI `set_received_files_dir` + `FileTransferStalled`,
  regenerated bindings, release profile overrides, `docs/protocol.md`; remote
  control rework (`core/continuityd/src/remote_viewer.rs`, frames decoded off the
  UI thread; `remote_control_mac.rs` modifier flags; `remote_control_windows.rs`
  extended keys). Measured ~575 MB/s vs ~133–195 MB/s for the old path on
  loopback.
- Phase 2: desktop main window, notifications (macOS really posts now, under
  the app's bundle id; Windows registers its own AppUserModelID), received
  folder setting, Transfers page, Control button, single-instance hand-off
  (`core/continuityd/src/{main,control,share,settings_window,file_chooser_linux}.rs`,
  `settings_window/page.html`, `installers/windows/continuity.iss`); macOS
  SwiftUI app updated; Android Downloads default + folder picker + heads-up
  progress notifications (`ReceivedFiles.kt`, `TransferNotifications.kt`, …).
- Review fixes so far: peer-supplied file names sanitized
  (`safe_received_file_name` in engine.rs — was a path-traversal hole); Windows
  Open uses ShellExecuteW instead of `cmd /C start` (was command injection).

## What is left

1. **Review findings** — all unticked items in `REVIEW-NOTES.md`, plus the
   remote-control and Android findings listed at the bottom of THIS file (they
   were found after REVIEW-NOTES.md was written). A review of the macOS SwiftUI
   app + cross-component contracts (control.rs JSON ↔ page.html ↔ Swift; FFI ↔
   Kotlin) had not reported yet — redo it: read every changed Swift file like a
   compiler (Swift can't be compiled on Linux for SwiftUI), check Codable keys
   against `core/continuityd/src/control.rs` serialization and the test fixture
   `apps/macos/ContinuitySettings/Tests/SettingsUITests/Fixtures/status.json`.
   For every finding: confirm it's real by reading the code first (some
   reviewer findings may be wrong), then fix minimally with a test, commit, push.
2. **Docs**: README.md (status table, "settings window" wording, tray
   description, "remote control between computers hidden" note), CHANGELOG.md
   `[Unreleased]` section (user-facing, in the existing style), docs that still
   say "Settings…". Do NOT bump the version.
3. Final verification (below), then the branch move described above, then
   report to the owner: what changed, what was verified, what still needs
   testing on real devices (everything GUI/device-level — nothing was run on a
   real Mac/PC/phone), and the "Waiting for approval" list.

## Approved by the owner on 2026-10-15 (after the review fixes)

- Fix the listener: run each inbound TLS handshake in its own task with a
  timeout (a LAN host could freeze the listener with one idle connection).
- From the list below, items 1, 2, 3, 5, 6, 7, 8, 9 — everything except the
  macOS banners-while-active item (later): macOS frame downscaling, Windows
  scan codes, renaming the "Continuity Settings" helper bundle, a v3 cancel
  message + Cancel button, `.part` cleanup at startup, falling back to the
  default folder mid-run, Android's quiet send-start notification + lint
  baseline, Linux `StartupWMClass`.
- Then: one CI run (release.yml dispatched on tmp/review-fixes — approved),
  then the branch move (fast-forward feature/fast-transfers, drop
  REVIEW-NOTES.md and HANDOFF.md in a final commit, plain push), then
  delete tmp/review-fixes and tmp/review-fixes-engine (approved).

## Waiting for the owner's approval (do NOT implement without a yes)

- macOS controlled side: downscale captured frames (max ~1920 wide, as Linux
  does) before JPEG encoding — smoother remote view.
- Windows controlled side: fill KEYBDINPUT scan codes (MapVirtualKeyW).
- macOS Dock/Cmd-Tab may still say "Continuity Settings" (helper bundle file
  name); fixing means renaming the bundle in build-app.sh, main.rs
  (`open_settings`) and release.yml.
- macOS: banners suppressed while Continuity is frontmost (no
  `shouldPresentNotification`); consider notify-rust's `preview-macos-un`.
- v3 explicit cancel message + a Cancel button for transfers.
- Clean up leftover `.part` files at startup.
- Fall back to the default folder mid-run if the chosen one disappears.
- Android: quiet notification for the start of a send; lint baseline for the
  generated bindings' `Cleaner` NewApi errors.
- Linux: `StartupWMClass` in the .desktop file.
(Note: "drag events on a controlled Mac" was on this list but the review
confirmed it as a bug — remote-control-4 — so it's now an approved fix.)

## Verification commands (repo root)

```
cargo build --workspace --all-targets
cargo test --workspace --lib --bins
cargo test -p continuity-daemon --tests          # run file_transfer a few times
cargo build -p continuityd --no-default-features # "lite" build
cargo check -p continuityd --target x86_64-pc-windows-gnu
(cd apps/android && gradle --no-daemon -q compileDebugKotlin)
```

Environment setup that the previous session needed (a fresh container won't
have it):
- Linux deps: `.github/scripts/install-linux-deps.sh` (apt) plus `mingw-w64`;
  `rustup target add x86_64-pc-windows-gnu x86_64-apple-darwin`.
- macOS type-check from Linux (no linking): create `maccheck/config.toml` with
  ```
  [target.x86_64-apple-darwin.aws_lc_0_44_0]
  rustc-cfg = ["x86_64_apple_darwin"]
  include = "/path/to/maccheck"
  [target.x86_64-apple-darwin.ring_core_0_17_14_]
  rustc-cfg = []
  ```
  then `DOCS_RS=1 cargo --config /path/to/maccheck/config.toml check --target
  x86_64-apple-darwin -p continuityd`.
- Android: install cmdline-tools from dl.google.com, then
  `sdkmanager "platforms;android-36" "build-tools;36.0.0" "platform-tools"`, and
  `echo "sdk.dir=<sdk>" > apps/android/local.properties` (git-ignored). Maven
  Central may return 429 — retry with backoff.

## Findings not yet in REVIEW-NOTES.md

- [x] **remote-control-1** (high, bug) Forwarded autorepeat KeyDowns leave keys stuck down on a GNOME (Linux) peer  
  `core/continuityd/src/remote_viewer.rs:945`  
  *Scenario:* KeyboardForwarder::key sends a fresh KeyDown for every local autorepeat of a held non-modifier key, whatever the peer platform. The old viewer dropped repeats (`if !event.repeat`). A Linux peer passes each KeyDown/KeyUp straight to the portal's NotifyKeyboardKeycode (remote_control_linux.rs forward_input). Mutter counts presses per key, in the virtual device and again in the seat (meta_seat_impl_notify_key_in_impl drops 'repeated' presses and any release whose count doesn't reach 0). Example: a Mac, Windows or Linux viewer controls a GNOME machine and holds Backspace for about a second. The peer gets KeyDown(14) about 15 times and then one KeyUp(14). The seat count goes to 15 and back to 14, so the release is dropped. Backspace stays pressed for the focused Wayland client, which keeps auto-repeating and deleting text until the session ends. Later press/release pairs only move the count between N and N-1, so neither the controller nor the local person can release it. release_all on focus loss or close has the same problem: it sends only one KeyUp. Mac and Windows peers are fine: macOS gets the autorepeat field and Windows SendInput repeats are normal.  
  *Fix:* In KeyboardForwarder::key, don't forward repeat presses when translator.peer == Platform::Linux (the Wayland compositor and clients repeat held keys themselves): return None for `Some(_)` in that case. Keep forwarding repeats for macOS and Windows peers. Add a unit test that a Linux peer gets exactly one KeyDown and one KeyUp for a held key.

- [x] **remote-control-2** (medium, pre-existing) Opening a viewer for a second peer silently orphans the first session, which can then never be ended or restarted from this side  
  `core/continuityd/src/main.rs:907`  
  *Scenario:* This device controls P with a viewer open. The person opens the Continuity window and clicks 'Control Q' (the window shows the button for every connected computer). Q accepts, so RemoteControlSessionStarted(Q, Controlling) runs `*viewer.borrow_mut() = Some(new_viewer)`. That drops P's viewer, whose Drop only sends key/button releases, but never sends EndRemoteControlSession for P. P's session stays active in the engine: P keeps capturing and streaming about 10 JPEGs a second, which the FrameRouter parks and throws away, and P keeps showing that it is being controlled. This device has no window or control to end it. Clicking 'Control P' again only toasts 'Asking P…', because the engine's RequestRemoteControl sees `remote_control_sessions.contains_key(P)` and `continue`s silently. The overwrite pattern was already in the old code.  
  *Fix:* Before replacing an open viewer whose peer_id differs from the new session's, take the old one and send EndRemoteControlSession { peer_crypto_id: old.peer_id() } after dropping it, as check_viewer_watchdog does. Alternatively, refuse the new session and end it instead.

- [x] **remote-control-3** (medium, pre-existing) RemoteControlSessionEnded closes the viewer by peer id only, even for an unrelated Controlled-role decline from the same peer  
  `core/continuityd/src/main.rs:917`  
  *Scenario:* The engine emits RemoteControlSessionEnded { peer_id: B, session_id: <B's request id> } on the controlled-side decline paths in accept_remote_control_session and start_controlled_session ('already controlling another device', 'couldn't find a network address…', 'couldn't start screen capture'). Those paths never touch this device's own Controlling session with B. Example: A is being controlled by C and is controlling B with a viewer open. B's person clicks 'Control A'. A declines with Ended{peer_id: B}, and handle_remote_control_sync_event drops A's viewer for B because `v.peer_id() == peer_id`. A's Controlling session with B is still active in the engine, so B keeps streaming and stays 'controlled', A has no window, and A's later 'Control B' is silently ignored. The viewer already carries the session id (SessionKey) but the Ended match doesn't use it.  
  *Fix:* Keep the session_id in RemoteViewer (it's already passed in ViewerPeer) and close the viewer only when both peer_id and session_id of the Ended event match.

- [x] **remote-control-4** (medium, pre-existing) Mac peer gets MouseMoved (not LeftMouseDragged) while a button is held, so drags don't work, and clicks never carry a click count  
  `core/continuityd/src/remote_control_mac.rs:418`  
  *Scenario:* A desktop viewer (or Android) presses the left button on a Mac peer and moves: inject_event posts kCGEventMouseMoved for every move while the button is down. macOS apps only treat kCGEventLeftMouseDragged/RightMouseDragged/OtherMouseDragged as a drag. Selecting text, moving a window by its title bar, resizing, or dragging a file in Finder therefore does nothing on the controlled Mac. Separately, button events never set kCGMouseEventClickState, so a quick double-click from the viewer arrives as two single clicks: files don't open in Finder and words don't get selected. Behaviour was the same before this change. The new viewer forwards real desktop drags and double-clicks, which makes it obvious.  
  *Fix:* Track the buttons held in MacRemoteControlHost (alongside HeldKeys) and post LeftMouseDragged, RightMouseDragged or OtherMouseDragged instead of MouseMoved while one is down. Count clicks by time and distance and set CGEventField::MouseEventClickState on the down and up events.

- [x] **remote-control-5** (low, bug) Controlled side releases held keys on session end but not held mouse buttons  
  `core/continuityd/src/remote_control_windows.rs:126`  
  *Scenario:* The Windows host's stop_capture (and the Mac one at remote_control_mac.rs:262) now lets go of injected keys when a session ends without key-ups, for example on a network drop or when the controller sleeps. Injected mouse buttons are not tracked. If the controller is mid-drag when the mesh connection drops, the engine ends the session (engine.rs:1604) and the viewer's MouseButton-up is discarded because the session is gone. Windows then keeps VK_LBUTTON logically down: the local person's mouse movements keep dragging or selecting, or move the captured window, until they physically click.  
  *Fix:* Record button downs and ups in inject() the way held_keys does, and send the matching MOUSEEVENTF_*UP (or the CGEvent *MouseUp at the current location on macOS) for each still-held button in stop_capture and start_capture.

- [x] **remote-control-6** (low, spec-deviation) Caps Lock is never forwarded from a Mac controller despite being in KEY_CODES  
  `core/continuityd/src/remote_viewer.rs:1068`  
  *Scenario:* tao's macOS view reports modifier keys only through flags_changed, and that handler checks only the Shift, Control, Option and Command masks. Caps Lock (kVK_CapsLock, which also arrives as flagsChanged) never produces a KeyboardInput event. On a Mac controlling Windows or Linux, pressing Caps Lock therefore does nothing on the peer, although the table and the 'whole keyboard' doc comment and tests say Caps Lock is covered.  
  *Fix:* Handle WindowEvent::ModifiersChanged or a flagsChanged for Caps Lock on macOS. For example, track the CapsLock lock state from ModifiersChanged and send a KeyDown/KeyUp pair for the peer's Caps Lock code when it toggles. Otherwise, document that Caps Lock is unsupported from a Mac.

- [x] **android-1** (medium, bug) Picking Download/Continuity as the folder is refused even when the engine is actually saving to the app-private fallback folder  
  `apps/android/app/src/main/java/app/continuity/android/ReceivedFiles.kt:334`  
  *Scenario:* Android 8 to 10 (API 26-29), and the user denied WRITE_EXTERNAL_STORAGE. chooseLocked() falls back to Android/data/<pkg>/files/Continuity, and the home screen shows "Download/Continuity can't be used (storage permission is off)...". The user taps Change and picks Download/Continuity in the SAF picker, which is the one way to get files there without the permission. chooseTreeNow() sees isDownloadsFolder(tree) == true, calls clearTree() and shows the notice "Received files already go to Download/Continuity". That notice is false: no tree is stored, and every later file still lands in the hidden app-private folder. The same thing happens on API 30+ whenever usable(downloads) fails.  
  *Fix:* Only take the "already there" shortcut when the engine really is writing to Download/Continuity. For example, check `_folder.value.fallbackNote == null` (or compare appliedDir with downloadsDir().path under the lock). Otherwise treat the pick like any other tree: take the persistable grant and move files there.

- [x] **android-2** (low, bug) isDownloadsFolder only recognises the externalstorage authority, so Download/Continuity picked from the Downloads root renames every received file to "name (1).ext"  
  `apps/android/app/src/main/java/app/continuity/android/ReceivedFiles.kt:377`  
  *Scenario:* On Android 10+ the picker's sidebar "Downloads" root belongs to com.android.providers.downloads.documents. Opening Continuity there and tapping "Use this folder" returns a tree like content://com.android.providers.downloads.documents/tree/raw%3A%2Fstorage%2Femulated%2F0%2FDownload%2FContinuity. isDownloadsFolder() returns false because the authority differs, so the tree is persisted. Each finished file is then createDocument()'d into the folder it already sits in. The provider names the new copy "photo (1).jpg", the full file is copied, and the original "photo.jpg" is deleted. The result is that every received file carries a " (1)" suffix and takes twice the I/O. This is exactly the case the comment at line 335 says it prevents.  
  *Fix:* Also recognise the downloads provider. For a document id of the form `raw:<path>`, compare the path with downloadsDir().canonicalPath. Or, more generally, resolve the tree to a filesystem path where possible and compare it with the engine's current dir.

- [x] **android-3** (low, bug) One lingering notification per finished file hits Android's 50-notifications-per-app cap, so later "Received" notifications silently never appear  
  `apps/android/app/src/main/java/app/continuity/android/TransferNotifications.kt:135`  
  *Scenario:* A desktop sends 60 photos to the phone. Each transfer leaves a "Received 'x'" notification under its own tag (doneTag(transferId)). These are autoCancel, so they only go away when tapped, and none are grouped or summarised. NotificationManagerService refuses new keys once a package has 50 posted or enqueued notifications ("Package has already posted 50 notifications. Not showing more"), and that count includes the foreground status notification and any active progress ones. From roughly the 48th file on, neither the progress notification nor the "Received" notification is posted, which breaks decision 1 for bulk sends.  
  *Fix:* Put transfer notifications in a group with a summary notification. Alternatively, cap the number of lingering done notifications: track the posted done tags and cancel the oldest beyond about 20, or fold completed files into one summary such as "Received 12 files from X".

- [x] **android-4** (low, bug) The SAF move writes directly under the final file name, so a process death mid-copy leaves a truncated file that looks complete in the chosen folder  
  `apps/android/app/src/main/java/app/continuity/android/ReceivedFiles.kt:243`  
  *Scenario:* A custom folder is set and a 2 GB video has just been received. finishReceived() is streaming it into createDocument(..., file.name). The user taps Quit, which calls Process.killProcess immediately (or the system kills the process). The catch/deleteDocument cleanup never runs. The chosen folder now holds "video.mp4" with only part of its bytes, and the full original stays in Download/Continuity. Nothing retries or cleans up on the next start, and no notification or activity entry says the move failed. A user who deletes the "duplicate" in Downloads keeps only the truncated copy.  
  *Fix:* Copy into a temporary name (for example "<name>.part" or a hidden name) and call DocumentsContract.renameDocument to the real name only after the size check passes. Fall back to the current behaviour for providers that don't support rename. Optionally, on startup, sweep leftover temporary documents in the tree.


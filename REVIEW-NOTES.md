# Review notes — work in progress (temporary branch)

This branch (`tmp/review-fixes`) is a safety copy of the work so far plus this
checklist. It is not meant to be merged as-is: once the fixes below are in,
the commits move to `feature/fast-transfers` and this file is dropped.

## Where things stand

On top of `master` (bf3312b), 15 commits:

- **Phase 1 (also on `feature/fast-transfers`)** — protocol v3 file transfer
  (dedicated TLS data connection per file, 1 MiB raw frames, resume after a
  dropped connection, `.part` files, no 500 MB cap, free-space check,
  `SetReceivedFilesDir`, `FileTransferStalled`), FFI + regenerated bindings,
  opt-level 3 for the hot crates only, protocol docs; desktop-to-desktop
  remote control (frames decoded off the UI thread — the freeze — full
  keyboard with the approved Cmd↔Ctrl swap, macOS modifier flags, Windows
  extended keys, viewer focus on macOS).
- **Phase 2 (only here so far)** — the desktop window becomes the main
  "Continuity" window (trimmed tray, Transfers page, Control button,
  received-files folder with Change…/Open, first-launch/relaunch opening,
  Windows left-click), notifications fixed on macOS (were never posted) and
  Windows (were PowerShell's), the macOS SwiftUI window updated to match,
  Android: Download/Continuity default + folder picker + heads-up progress
  notifications.

Checked in the container: Linux build + all unit/integration tests; Windows
and macOS type-checks of continuityd; lite build; Android `compileDebugKotlin`.
Nothing has been run on a real device. Swift was not compiled.

## Verification commands (repo root)

```
cargo build --workspace --all-targets
cargo test --workspace --lib --bins
cargo test -p continuity-daemon --tests
cargo check -p continuityd --target x86_64-pc-windows-gnu
cargo build -p continuityd --no-default-features
(cd apps/android && gradle --no-daemon -q compileDebugKotlin)
```

## Review findings to fix

From the adversarial review of everything above. These two areas were
reported; the skeptic pass that tries to refute each one, and the review of
remote control, Android and the macOS app, were still running when this
was written — expect more items and some of these to be dropped.

- [x] **transfer-engine-1** (critical, security) A peer-supplied file_name is never sanitized, so a paired peer can write files anywhere (both v3 and in-band paths)  
  `core/continuity-daemon/src/engine/transfer.rs:316`  
  *Scenario:* Nothing cleans the `file_name` that arrives in `FileOffer`. On the v3 path, `create_part_file` runs `dir.join(&name)` and `dir.join(format!("{name}.part"))`. `finalize_part` then renames to `unique_destination_path(part_path.parent(), file_name)`. If a paired device sends an absolute path, such as `/home/alice/.config/autostart/x.desktop`, `/Users/alice/Library/LaunchAgents/x.plist` or `C:\Users\alice\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\x.bat`, the `.part` file is created there and renamed to the exact target, because `Path::join` with an absolute path replaces the base. The in-band path is already exploitable and is still reachable on a v3 receiver: an attacker just sends `data_channel:false`. There, `unique_destination_path(&dir, "../../.config/autostart/x.desktop")` returns `dir.join(..)` whenever the target does not exist, and `File::create` writes it. Offers from paired devices are auto-accepted with no prompt, and size checks no longer apply. So one compromised or buggy paired device can plant new files outside the received folder, for example autostart entries, giving code execution at the next login on every other paired device. A Mac sending a name with ':' to Windows also lands in an NTFS alternate data stream.  
  *Fix:* Reduce the name to one safe file name on receipt, before both `handle_data_channel_offer` and `handle_file_offer`. Keep only `Path::new(name).file_name()` after also splitting on both '/' and '\\'. Reject or replace empty, '.', '..', ':' and Windows-reserved names and characters, and fall back to "file". As a backstop, check in `create_part_file`, `finalize_part` and `unique_destination_path` that the joined path's parent equals the received directory.

- [x] **desktop-shell-1** (high, security) Received file name from a peer is joined to the received folder without sanitising, so a peer can write files outside it  
  `core/continuity-daemon/src/engine/transfer.rs:304`  
  *Scenario:* A paired peer (or a compromised one) sends a FileOffer with file_name "../../.config/autostart/x.desktop" or an absolute "/home/u/.config/autostart/x.desktop". On Windows the equivalent is "..\..\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\x.bat". The new v3 path calls create_part_file(dir, file_name) and dir.join(&name), then finalize_part calls unique_destination_path(part_path.parent(), file_name). The legacy path calls unique_destination_path(&dir, &file_name) and File::create. No step strips path components, so the file lands outside Downloads/Continuity and can run code at the next login. continuity-proto does no validation either. The legacy path had this before the change; the new v3 code repeats it.  
  *Fix:* Before any join, keep only the last component of the received name (Path::new(name).file_name()), drop separators and '..', and refuse an empty result. Do this once in answer_offer and handle_file_offer, so the .part, final and legacy names are all safe.

- [ ] **desktop-shell-2** (medium, bug) Rebuilding the whole page about 4 times a second during a transfer drops clicks and keyboard focus  
  `core/continuityd/src/settings_window/page.html:438`  
  *Scenario:* While any transfer is moving, every progress push (up to about every 250 ms) runs render(). render() replaces #sidebar, #toolbar and #content through innerHTML. If a re-render lands between mousedown and mouseup, the button that was pressed is gone. Neither WebKit nor Chromium then sends a click to the new button: there is either no click at all, or a click on #content, where closest('[data-action]') finds nothing. With a click taking about 100 ms and renders every 250 ms, a large share of clicks on Open, Show in folder, Send files…, Control, sidebar items and the switches do nothing during a transfer. Keyboard focus and text selection are also reset 4 times a second. Before this change, statuses only arrived on real state changes, so this did not happen.  
  *Fix:* Don't rebuild the DOM for a status that only changed progress. Patch the transfer rows, progress bars and sidebar summary in place, or hold back render() while a pointer button is down and run it on pointerup.

- [x] **desktop-shell-3** (medium, bug) The new Control button can start a second controlling session, and the first one is leaked  
  `core/continuityd/src/main.rs:906`  
  *Scenario:* The user controls computer A from the window, then (A's viewer still open) clicks Control on computer B. The engine allows one session per peer, so B's session starts too. handle_remote_control_sync_event then runs `*viewer.borrow_mut() = Some(new_viewer)`, which drops A's viewer without sending EndRemoteControlSession for A. A keeps capturing and streaming its screen: FrameRouter drops those frames because only B is attached. A keeps showing 'is being controlled', and nothing on this side can end it. When B's viewer closes, this device's tray dot is cleared even though A's session is still live. Before this change, the desktop had no visible way to request remote control.  
  *Fix:* In RemoteControlSessionStarted, before replacing an existing viewer, send EndRemoteControlSession for its peer. Alternatively, refuse request_remote_control in ControlState::apply while a Controlling session is active (track it in observe) and hide the Control button then.

- [ ] **desktop-shell-4** (medium, bug) Single-instance hand-off races: two launches close together both start full instances  
  `core/continuityd/src/main.rs:138`  
  *Scenario:* hand_off_to_running_instance only finds an instance after it has written share-endpoint.<profile>.json. start_server writes that file only after start_engine_thread returns, which includes Identity::load_or_create (Secret Service on Linux, which can wait at a keyring-unlock prompt) and the engine start. A normal relaunch doesn't open the window, so it is easy to click the icon again during that gap. The second launch finds no endpoint and starts a second tray app and engine with the same identity: duplicate mDNS adverts, clipboard echo, two windows on first launch. The later start_server overwrites the endpoint file. When either instance quits, server.shutdown() deletes the file the other one wrote, which breaks --share-to and the hand-off for the instance still running.  
  *Fix:* Take an exclusive OS lock (for example a lock file next to the endpoint held with flock or LockFileEx, or a named mutex on Windows) before anything else. If the lock is held, retry the hand-off with backoff for a few seconds and then exit. Have shutdown() remove the endpoint only if it still holds this instance's token.

- [x] **desktop-shell-5** (medium, pre-existing) Windows open_path passes a peer-chosen file name to cmd.exe unquoted, allowing command injection; now reachable from the Transfers 'Open' button  
  `core/continuityd/src/main.rs:1182`  
  *Scenario:* open_transfer and the notification's Open action both call open_path, which runs Command::new("cmd").args(["/C","start","",path]). Rust only quotes arguments that contain spaces or tabs, and the engine does not sanitise names. A paired peer sends a file named "notes&calc", which lands at C:\Users\john\Downloads\Continuity\notes&calc with no spaces, so it is not quoted. The user clicks Open in the Transfers list, and cmd runs `start "" C:\...\notes` and then `calc`. Chaining with ',' and %VAR% (for example `x&cd,%USERPROFILE%&cd,Downloads&cd,Continuity&evil.bat`) runs a payload received earlier. %VAR% in names is also expanded, so the wrong file opens.  
  *Fix:* Don't go through cmd. Call ShellExecuteW("open", path) (Win32_UI_Shell is already enabled), or use explorer.exe or rundll32 url.dll,FileProtocolHandler with the path as a single argument.

- [ ] **transfer-engine-2** (medium, bug) A disk write failure closes the data connection without a FileVerified verdict, so the sender restarts the file from 0 in an endless loop  
  `core/continuity-daemon/src/engine/transfer.rs:711`  
  *Scenario:* When the receiver's writer thread fails (for example EFBIG at 4 GiB on a FAT32 USB stick picked as the received folder, EIO, or ENOSPC), `return_part` deletes the entry and the `.part` file and emits `FileTransferFailed`. Then `frames_tx.send` fails, giving `ReceiveError::WriterStopped`, which hits the `_ => {}` arm and drops the connection with no `FileVerified{ok:false}`. The Ok path does send one. The sender sees a write error, which becomes `Interrupted{sent: ~4 GiB}`. Because sent > 0, `data_sent` is set and retries and the stall window are reset, and it re-offers with `resume:true` after 1 s. `answer_offer` finds no entry and, per lines 441-446, treats it as a brand-new transfer: it creates a new `.part`, emits `FileReceiving` again for the same id, and answers `offset 0`. The sender streams the whole file again and fails at the same point, forever. The receiver's UI flips between failed and receiving and the link carries gigabytes per loop.  
  *Fix:* In the error branch, handle `WriterStopped` like the Ok path's "part is None" case. Have the writer's failure reason travel back (for example as the `write_part` return value) and write `FileVerified { ok: false, reason }` before returning, so the sender gets `StreamError::Rejected` and fails once.

- [ ] **transfer-engine-3** (medium, spec-deviation) The free-space check ignores space already promised to other incoming transfers, so batch sends overfill the disk  
  `core/continuity-daemon/src/engine/transfer.rs:451`  
  *Scenario:* Each offer is checked alone: `available >= size + 64 MiB` at offer time, here, at line 422 for resumes, and in `handle_file_offer` at engine.rs:2173. Nothing counts the bytes still to come for transfers already accepted. Sharing several files sends one `SendFile` each (share.rs:118), and these run concurrently. Example: three 3 GB videos to a phone with 5 GB free. All three offers pass, since nothing has been written yet. The disk then fills to 0 bytes, which the margin was meant to prevent, and the writes fail mid-transfer. The sender gets a broken connection instead of the approved "Not enough free space on <device> (needs …, … free)" refusal, and per transfer-engine-2 the failed transfers restart from 0.  
  *Fix:* Keep a reserved-bytes total, for example by summing `size_bytes - written` over the non-Completed `incoming_transfers`, plus the in-band `receiving` totals. Subtract it from `available` in `free_space_refusal_for` when checking a new offer or a resume, excluding the transfer's own remaining bytes on a resume.

- [x] **transfer-engine-4** (medium, bug) A sender's data-connection writes have no progress timeout, so a vanished receiver blocks resume past the 10-minute window and keeps a dead mesh connection open  
  `core/continuity-daemon/src/engine/transfer.rs:1148`  
  *Scenario:* `stream_file` awaits `write_frame_unflushed` with no timeout. If the receiver disappears without a RST (for example it switches from Wi-Fi to Ethernet at a dock and gets a new IP, or leaves the network), the sender has unacknowledged data in flight. Linux, and so Android, does not send keepalive probes in that state. The write blocks until `tcp_retries2` gives up, about 15 minutes with defaults. The receiver's side is closed by its own keepalive after about 2 minutes, so its entry goes to Waiting and the sweeper reports `FileTransferFailed` and deletes the `.part` at about 12 minutes. The sender only notices at about 15 minutes. It then re-offers, the receiver has nothing, and the file restarts from 0, so resume does not work. Meanwhile the sender's `DataStreamGuard` keeps `has_file_transfer_in_flight` true (engine.rs:446-447). That stops the mesh staleness check from dropping the equally dead mesh connection, so the receiver's reconnect from its new IP is refused with "already connected" and clipboard sync is down for those ~15 minutes. During all this the sender shows the transfer as active, not stalled.  
  *Fix:* Wrap each frame write (and the end frame plus `FileComplete`) in a progress timeout, for example `tokio::time::timeout(60s, write_frame_unflushed(..))`, and map expiry to `StreamError::Interrupted`. Alternatively set `TCP_USER_TIMEOUT` (or `SO_SNDTIMEO`) on data connections. Consider not counting v3 data streams toward the mesh staleness suppression at all, since v3 data no longer queues behind pings.

- [x] **desktop-shell-10** (low, bug) Transfers that fail before the receiver accepts never appear in the window's Transfers list  
  `core/continuityd/src/control.rs:494`  
  *Scenario:* The engine emits FileSending only after the receiver accepts, on both the v3 and legacy paths. If the receiver refuses for lack of free space (decision 3), the sender gets FileTransferFailed for an id the list has never seen. finish_transfer then returns None, and nothing is listed. The user clicks Send files…, sees the toast 'Sending 1 file(s)', and the Transfers page stays empty. The 'Not enough free space on …' reason appears only in a passing notification.  
  *Fix:* In observe, when FileTransferFailed arrives for an unknown id, add a failed entry (direction sending, the reason, file and peer name if known). Alternatively, have the engine emit FileSending when it makes the offer.

- [x] **desktop-shell-6** (low, bug) The new .bar progress-bar CSS also hits the About sheet's bottom bar and clips it  
  `core/continuityd/src/settings_window/page.html:199`  
  *Scenario:* The info sheet's footer is `<div class="bar">` (line 749). `.sheet .bar` overrides display, padding, border and background, but not the new `.bar` rule's height:4px, overflow:hidden, margin-top:8px and border-radius:2px. With box-sizing:border-box, the footer shrinks to its padding (about 25 px) with zero content height, and overflow:hidden cuts off the vertically centred 32–34 px Done button above and below. There is also an 8 px gap above the footer. The About panel's footer looks broken on Linux and Windows.  
  *Fix:* Rename the progress bar class (for example .progress), or scope it as `.row .bar`.

- [x] **desktop-shell-7** (low, bug) The new Nearby refresh button doesn't look for nearby devices  
  `core/continuityd/src/control.rs:681`  
  *Scenario:* The page's refresh icon in the Nearby header (tooltip 'Look for nearby devices again', toast 'Looking for nearby devices…') sends refresh_nearby. That maps to EngineCommand::RefreshDiscovery, which in the engine only calls redial_disconnected_trusted_peers: it redials paired devices with cached addresses and never re-queries mDNS. events.rs documents it as re-issuing an mDNS query. An unpaired device whose announcement was missed stays missing from Nearby, so the button does nothing for the list it sits on.  
  *Fix:* Have RefreshNearby (or RefreshDiscovery) also trigger a new mDNS browse, for example state.discovery_recreate.notify_one() as NetworkChanged does.

- [ ] **desktop-shell-8** (low, spec-deviation) macOS loses the tray's Refresh action: the trimmed tray has none and the SwiftUI window has no refresh_nearby  
  `core/continuityd/src/main.rs:182`  
  *Scenario:* The tray's 'Refresh Nearby Devices' item is removed on all platforms. The Linux/Windows page gained a refresh button. The macOS window (apps/macos/ContinuitySettings) never sends refresh_nearby, and its Nearby section is hidden when empty. On macOS there is no longer any way to trigger RefreshDiscovery, i.e. to redial all disconnected paired devices at once, which the brief requires to stay reachable in the window.  
  *Fix:* Add a refresh control to the macOS sidebar's Nearby section (always shown) that sends {"op":"refresh_nearby"}, matching page.html.

- [ ] **desktop-shell-9** (low, bug) Endpoint file is left behind after every non-Quit exit; the next launch's hand-off can stall or be blocked by whoever owns that port  
  `core/continuityd/src/share.rs:338`  
  *Scenario:* Only the Quit menu item calls server.shutdown(). A logoff or shutdown on Windows or Linux kills the process and leaves share-endpoint.default.json. At the next login, the Run-key or autostart launch (args.len()==1) first connects to the dead port. On Windows, connecting to a closed loopback port retries for about 2 s (capped by the 2 s connect_timeout), and a port now held by an unrelated listener that never replies costs another 3 s read timeout before Continuity starts. The hand-off only proves the client knows the token, not the server, so a local process of another user that binds the old port and answers {"ok":true} makes every launch exit at once, and Continuity never starts.  
  *Fix:* Remove the endpoint on LoopDestroyed and on WM_ENDSESSION or SIGTERM. Have the server prove it knows the token in its reply (echo an HMAC of a client nonce) before the client exits.

- [x] **transfer-engine-5** (low, bug) confirm_already_complete's 120 s timeout includes the sender's whole-file re-hash, so it can falsely reject a finished transfer  
  `core/continuity-daemon/src/engine/transfer.rs:855`  
  *Scenario:* Take a sender that lost the connection just before `FileVerified` arrived. It re-offers, gets `accept(size)`, then dials and sends `FileStreamHandshake{offset: size}` straight away. It writes the empty frame and `FileComplete` only after `read_for_stream` has re-hashed the whole file (lines 1202-1215). The receiver's `confirm_already_complete` allows `VERIFY_TIMEOUT` (120 s) for all of that. For a large file on slow storage (a 4 GB file on a USB 2 stick, or about 20 GB on an HDD), hashing takes longer than 120 s. The receiver then sends `ok:false` with "this transfer already finished here with different contents", and the sender reports `FileTransferFailed` with that reason. The receiver has already saved the file and reported `FileReceived`.  
  *Fix:* Don't time the hash. Either wait without a timeout in `confirm_already_complete`, as `receive_stream` does, relying on TCP keepalive, or start its timeout only after the zero-length end frame arrives.

- [ ] **transfer-engine-6** (low, bug) While a screen stream is expected from a peer, that peer's file data connection is misrouted, and the screen stream is lost  
  `core/continuity-daemon/src/engine.rs:929`  
  *Scenario:* The accept loop gives the next inbound connection from a peer in `pending_screen_stream_peers` to `handle_screen_stream_connection` by identity alone, before the new `may_dial_data_connection` routing. If that peer (the device being controlled) is sending this device a v3 file, its data connection can arrive between `RemoteControlResponse{accepted}` and the screen-stream dial-back, for example a new transfer or a resume retry. The data connection then takes the screen-stream slot and fails with "expected ScreenStreamHandshake". The real screen-stream connection that follows goes to `route_inbound`, is read as a non-handshake, and is handed to `handle_connection`, which bails with "already connected". The remote-control session starts but no frames ever arrive.  
  *Fix:* When the peer also has v3 transfers, route by first message there too. In `route_inbound`, if `first` is `ScreenStreamHandshake` and the peer is in `pending_screen_stream_peers`, hand it to the screen-stream handler. Only skip reading when no transfer from that peer exists.

- [x] **transfer-engine-7** (low, bug) If receive_stream is aborted before its first poll, the partial file is lost and the entry is stuck in Streaming for good  
  `core/continuity-daemon/src/engine/transfer.rs:574`  
  *Scenario:* `claim_for_stream` moves the `PartFile` out of the entry, which becomes Streaming with `part: None`, into the `StreamClaim` that the spawned `receive_stream` future owns. `write_part`/`return_part`, which the comments call the only place the file is handed back, are spawned only inside that future's first poll. If the task is aborted before it is first polled, the `PartFile` is dropped silently. This can happen through `answer_offer`'s Streaming arm (lines 408-414, for example a late duplicate resume answer) or through `discard_incoming` (DisconnectPeer, Revoke or shutdown in that window). After an `answer_offer` abort, the entry stays Streaming with `part` and `stream` both None. The sweeper never expires Streaming entries, the resume offer spins for `RELEASE_WAIT` and then refuses, so the sender fails. The `.part` file is never deleted, and `may_dial_data_connection` stays true for that peer for good. After a `discard_incoming` abort, the `.part` file is orphaned.  
  *Fix:* Give `StreamClaim` (or the `PartFile` inside it) a Drop guard that calls `return_part` if the file was never handed to the writer. Or spawn the `write_part` thread in `route_inbound`, before spawning the async half, so the file always has an owner that returns it.

- [x] **transfer-engine-8** (low, bug) The .part suffix pushes long file names past NAME_MAX, so v3 refuses files the in-band path accepted  
  `core/continuity-daemon/src/engine/transfer.rs:316`  
  *Scenario:* `create_part_file` creates `<name>.part`. A file whose name is 251-255 bytes is valid on the sender, for example about 84 CJK characters, or long generated names. It gets ENAMETOOLONG on Linux, Android and macOS (255-byte or 255-unit limits). The receiver refuses with "<device> couldn't create the file: File name too long" and the sender reports `FileTransferFailed`. Before v3, the in-band path created the final name directly and received the file.  
  *Fix:* When `name.part` is too long, shorten the stem of the part name (the final name is chosen separately at completion). Or use a fixed-length part name such as `.continuity-<transfer_id>.part` and keep the intended name only in `IncomingTransfer`.

- [ ] **transfer-engine-9** (low, bug) No limit on concurrent incoming v3 transfers per peer, and data frames up to 16 MiB are accepted  
  `core/continuity-daemon/src/engine/transfer.rs:333`  
  *Scenario:* Each `FileOffer{data_channel:true}` with a new id spawns a task, creates a `.part` file and keeps its file descriptor open in `incoming_transfers` for up to `RESUME_WINDOW` (10 min). Unlike the in-band path, this outlives the mesh connection. A buggy or malicious paired peer that sends thousands of small offers exhausts the receiver's file descriptors, after which `listener.accept` and every other file operation fail. Each data connection can also buffer about 5 frames of up to `MAX_FRAME_BYTES` = 16 MiB (the screen-stream limit, not the 1 MiB file frame size), roughly 96 MiB when the disk is slower than the network. Across many concurrent transfers that is enough memory to get the Android app killed.  
  *Fix:* Cap the number of non-Completed incoming transfers per peer (refuse with a reason above, say, 16). In `receive_stream`, treat a frame longer than `FRAME_SIZE` as `ReceiveError::Fatal`.

## Improvements suggested but NOT approved yet (ask the owner first)

- macOS controlled side: downscale captured frames (as Linux does, max 1920
  wide) before JPEG encoding — smoother remote view, less to decode.
- macOS controlled side: send mouse-dragged events while a button is held, so
  drag-select and drag-and-drop work.
- Windows controlled side: fill the scan code (MapVirtualKeyW) for apps that
  read it.
- macOS Dock/Cmd-Tab may still say "Continuity Settings" (bundle file name):
  fixing it means renaming the helper bundle in build-app.sh, main.rs and
  release.yml.
- macOS: banners are suppressed while Continuity is the active app (no
  shouldPresentNotification) — consider notify-rust's `preview-macos-un`.
- v3: an explicit cancel message (today a cancelled send shows "stalled"
  on the receiver until the 10-minute expiry); a "Cancel" button.
- Clean up leftover `.part` files at startup.
- Fall back to the default folder mid-run if the chosen one disappears.
- Android: quiet (non-popup) notification for the start of a send;
  lint baseline for the generated bindings' `Cleaner` NewApi errors.
- Linux: `StartupWMClass` in the .desktop file.
- README, CHANGELOG and docs still describe the old tray ("Settings…").

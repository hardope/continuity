//! Global media-key injection for macOS. There's no public, documented API
//! for "play/pause whatever's currently playing" — every known
//! implementation (Spotify's own menu bar helper included, historically;
//! see the long-standing `SPMediaKeyTap` project and its many derivatives)
//! uses the same trick: construct an `NSEventTypeSystemDefined` event
//! carrying one of the `NX_KEYTYPE_*` constants from
//! `IOKit/hidsystem/ev_keymap.h` in its `data1` field, then post it at the
//! session level. The system delivers it to whichever app currently owns
//! "now playing" the same way a real keyboard's media keys would.

use continuity_daemon::MediaController;
use continuity_proto::{MediaCommand, NowPlayingInfo};
use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSEventType};
use objc2_core_graphics::{CGEvent, CGEventTapLocation};
use objc2_foundation::NSPoint;
use std::ffi::{c_void, CStr};
use std::sync::mpsc;
use std::sync::Once;
use std::time::Duration;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef) -> u8;
}

/// `CGEventPost`ing a synthetic media key — unlike the CoreAudio volume
/// calls below — is gated behind Accessibility permission on modern macOS.
/// An unsigned/ad-hoc-signed dev build (or a rebuilt one — ad-hoc
/// signatures change per build, and TCC tracks trust per signature) starts
/// out untrusted every time, so transport commands silently no-op while
/// volume keeps working — exactly the asymmetry a user would see and
/// report as "play/pause do nothing but volume works". Checked once (not
/// once per command, to avoid spamming a notification on every remote tap)
/// and, if untrusted, tells the user and jumps them straight to the
/// System Settings pane that grants it.
///
/// `pub(crate)` so `remote_control_mac.rs`'s input injection — the exact
/// same `CGEventPost` mechanism, just building fuller keyboard/mouse
/// events instead of the three fixed media-key constants — can call this
/// too, rather than assuming media control already triggered the prompt
/// at some earlier point. A user who only ever tries remote control and
/// never touches media keys would otherwise hit the identical silent
/// no-op this was written to fix in the first place, just on a different
/// code path.
pub(crate) fn ensure_accessibility_trust() {
    static CHECKED: Once = Once::new();
    CHECKED.call_once(|| {
        if unsafe { AXIsProcessTrusted() } != 0 {
            return;
        }

        // `AXIsProcessTrusted()` alone only ever *checks* — it never
        // registers this process in System Settings' Accessibility list
        // at all, so a user sent to that pane (via the `open` call below)
        // found no entry there to even check, no matter how carefully
        // they looked. This was the real bug behind "granted permission
        // and it still doesn't work": there was nothing to grant yet.
        // `AXIsProcessTrustedWithOptions` with the prompt option is the
        // API that actually adds the app to the list and pops the
        // system's own permission dialog — same shape as
        // `remote_control_mac.rs::ensure_screen_recording_trust`'s
        // `CGRequestScreenCaptureAccess` for the same reason.
        {
            use core_foundation::base::TCFType;
            use core_foundation::boolean::CFBoolean;
            use core_foundation::dictionary::CFDictionary;
            use core_foundation::string::CFString;

            let options =
                CFDictionary::from_CFType_pairs(&[(CFString::new("AXTrustedCheckOptionPrompt"), CFBoolean::true_value())]);
            if unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) } != 0 {
                return;
            }
        }

        tracing::warn!(
            "Continuity isn't trusted for Accessibility — remote play/pause/next/previous will silently do nothing until it's granted in System Settings > Privacy & Security > Accessibility"
        );
        if let Err(e) = notify_rust::Notification::new()
            .summary("Continuity")
            .body("Grant Accessibility permission for remote play/pause/next/previous to work. Volume control isn't affected.")
            .show()
        {
            tracing::debug!("accessibility notification not shown: {e}");
        }
        if let Err(e) =
            std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility").spawn()
        {
            tracing::debug!("couldn't open System Settings: {e}");
        }
    });
}

// From IOKit/hidsystem/ev_keymap.h — not exposed as a public header import,
// these are the well-known reverse-engineered values every media-key-
// simulation project (dating back to SPMediaKeyTap) has used for years.
const NX_KEYTYPE_PLAY: i64 = 16;
const NX_KEYTYPE_NEXT: i64 = 17;
const NX_KEYTYPE_PREVIOUS: i64 = 18;

pub struct MacMediaController;

impl MediaController for MacMediaController {
    fn handle(&self, command: MediaCommand) {
        match command {
            MediaCommand::PlayPause => post_media_key(NX_KEYTYPE_PLAY),
            MediaCommand::Next => post_media_key(NX_KEYTYPE_NEXT),
            MediaCommand::Previous => post_media_key(NX_KEYTYPE_PREVIOUS),
            // *Not* the same synthetic-media-key trick as transport
            // control — tested first, and macOS silently ignores
            // synthetic NX_KEYTYPE_SOUND_UP/DOWN events (0 effect on real
            // output volume across five consecutive presses in testing),
            // unlike the transport keys. System volume is public,
            // documented CoreAudio territory instead — see
            // system_volume::step below.
            MediaCommand::VolumeUp => system_volume::step(0.0625),
            MediaCommand::VolumeDown => system_volume::step(-0.0625),
            MediaCommand::Seek { position_ms } => now_playing::set_elapsed_time(position_ms as f64 / 1000.0),
        }
    }

    fn now_playing(&self) -> Option<NowPlayingInfo> {
        now_playing::get()
    }
}

fn post_media_key(key: i64) {
    ensure_accessibility_trust();
    // A real key press is a down event followed by an up event — sending
    // only one or the other is ignored by most apps.
    for key_down in [true, false] {
        let Some(event) = build_system_defined_event(key, key_down) else {
            tracing::debug!("couldn't construct synthetic media-key event");
            return;
        };
        let Some(cg_event) = event.CGEvent() else {
            tracing::debug!("NSEvent had no underlying CGEvent to post");
            return;
        };
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&cg_event));
    }
}

fn build_system_defined_event(key: i64, key_down: bool) -> Option<Retained<NSEvent>> {
    // NX_KEYDOWN = 0xa, NX_KEYUP = 0xb — packed into data1's low byte
    // (after the key code in the high 16 bits) and, separately, into the
    // top byte of modifierFlags. Both placements are required; this is
    // the exact shape every reference implementation of this trick uses.
    let state: i64 = if key_down { 0xa } else { 0xb };
    let data1 = (key << 16) | (state << 8);
    let modifier_flags = if key_down { 0xa00 } else { 0xb00 };

    NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::SystemDefined,
        NSPoint::new(0.0, 0.0),
        objc2_app_kit::NSEventModifierFlags(modifier_flags),
        0.0,
        0,
        None,
        8,
        data1 as isize,
        -1,
    )
}

/// Reads now-playing metadata (title/artist/album/artwork/playback state)
/// via `MediaRemote.framework` — a private framework with no public header,
/// so this loads it with `dlopen`/`dlsym` at runtime rather than linking
/// against it, and reconstructs just enough of its API from what's
/// publicly documented by community reverse-engineering (this exact
/// approach — `MRMediaRemoteGetNowPlayingInfo` with an async completion
/// block, reading the `kMRMediaRemoteNowPlayingInfo*` dictionary keys by
/// their well-known string names rather than the (also private) exported
/// symbol constants — is what every "now playing" menu bar utility for
/// macOS has used for years; Control Center and the lock screen widget use
/// the same framework internally). Verified against real playback: key
/// names confirmed by dumping an actual returned dictionary's keys, and
/// `is_playing` confirmed tracking true/false correctly against a real
/// QuickTime Player session.
mod now_playing {
    use super::*;
    use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};

    type GetNowPlayingInfoFn = unsafe extern "C-unwind" fn(*mut c_void, *mut c_void);
    // Reverse-engineered the same way as `MRMediaRemoteGetNowPlayingInfo` —
    // takes elapsed time in *seconds* (a double, matching
    // `kMRMediaRemoteNowPlayingInfoElapsedTime`'s own unit below), applies
    // immediately, synchronously, no completion callback. This is the same
    // primitive Control Center's own scrubber uses internally.
    type SetElapsedTimeFn = unsafe extern "C-unwind" fn(f64);

    fn media_remote_handle() -> Option<*mut c_void> {
        unsafe {
            let path = CStr::from_bytes_with_nul(
                b"/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote\0",
            )
            .unwrap();
            let handle = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY);
            (!handle.is_null()).then_some(handle)
        }
    }

    pub fn set_elapsed_time(seconds: f64) {
        unsafe {
            let Some(handle) = media_remote_handle() else {
                tracing::debug!("couldn't dlopen MediaRemote.framework for seek");
                return;
            };
            let sym_name = CStr::from_bytes_with_nul(b"MRMediaRemoteSetElapsedTime\0").unwrap();
            let sym = libc::dlsym(handle, sym_name.as_ptr());
            if sym.is_null() {
                tracing::debug!("MRMediaRemoteSetElapsedTime symbol not found");
                return;
            }
            let set_elapsed_time: SetElapsedTimeFn = std::mem::transmute(sym);
            set_elapsed_time(seconds);
        }
    }

    pub fn get() -> Option<NowPlayingInfo> {
        unsafe {
            let Some(handle) = media_remote_handle() else {
                tracing::debug!("couldn't dlopen MediaRemote.framework");
                return None;
            };

            let sym_name = CStr::from_bytes_with_nul(b"MRMediaRemoteGetNowPlayingInfo\0").unwrap();
            let sym = libc::dlsym(handle, sym_name.as_ptr());
            if sym.is_null() {
                tracing::debug!("MRMediaRemoteGetNowPlayingInfo symbol not found");
                return None;
            }
            let get_now_playing_info: GetNowPlayingInfoFn = std::mem::transmute(sym);

            let (tx, rx) = mpsc::channel::<Option<NowPlayingInfo>>();
            let block = block2::RcBlock::new(move |dict_ptr: *mut c_void| {
                let info = if dict_ptr.is_null() {
                    None
                } else {
                    let dict = &*(dict_ptr as *const CFDictionary<CFString, CFType>);
                    let mut info = parse_dict(dict);
                    // Not part of the MediaRemote dictionary at all — the
                    // system output volume (see `system_volume` below) is a
                    // separate CoreAudio concept, folded in here only
                    // because this is the one snapshot type that already
                    // gets polled and broadcast on a change.
                    info.volume_percent = super::system_volume::current();
                    Some(info)
                };
                let _ = tx.send(info);
            });

            // A global concurrent queue rather than the main queue — GCD's
            // global queues have their own dedicated worker thread pool and
            // run regardless of whether anything is pumping a run loop.
            // The main queue only executes work when something drains it
            // (an app's `CFRunLoopRun`/`NSApplicationMain`), which isn't
            // guaranteed here: this runs on `spawn_blocking`, unrelated to
            // whatever GUI event loop (or none, e.g. in a test binary) the
            // process happens to have.
            let queue = dispatch2::DispatchQueue::global_queue(
                dispatch2::GlobalQueueIdentifier::Priority(dispatch2::DispatchQueueGlobalPriority::Default),
            );
            get_now_playing_info(
                &*queue as *const dispatch2::DispatchQueue as *mut c_void,
                block2::RcBlock::as_ptr(&block) as *mut c_void,
            );

            // MediaRemote's completion runs async, on the global queue
            // above — this call itself happens on the engine's own
            // background thread (spawn_blocking), so waiting here doesn't
            // block anything else in the app.
            rx.recv_timeout(Duration::from_millis(2000)).ok().flatten()
        }
    }

    fn parse_dict(dict: &CFDictionary<CFString, CFType>) -> NowPlayingInfo {
        let string_field = |key: &str| -> Option<String> {
            let key = CFString::from_str(key);
            dict.get(&key).and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string()))
        };

        let title = string_field("kMRMediaRemoteNowPlayingInfoTitle");
        let artist = string_field("kMRMediaRemoteNowPlayingInfoArtist");
        let album = string_field("kMRMediaRemoteNowPlayingInfoAlbum");

        let artwork = dict
            .get(&CFString::from_str("kMRMediaRemoteNowPlayingInfoArtworkData"))
            .and_then(|v: CFRetained<CFType>| v.downcast_ref::<objc2_core_foundation::CFData>().map(|d| d.to_vec()))
            .unwrap_or_default();

        let is_playing = dict
            .get(&CFString::from_str("kMRMediaRemoteNowPlayingInfoPlaybackRate"))
            .and_then(|v: CFRetained<CFType>| v.downcast_ref::<objc2_core_foundation::CFNumber>().and_then(|n| n.as_cgfloat()))
            .map(|rate| rate != 0.0)
            .unwrap_or(false);

        // Both keys hold seconds as a double (confirmed against a real
        // dumped dictionary, same as the string/rate keys above) — `*
        // 1000.0` to match this protocol's millisecond convention
        // everywhere else (see continuity_proto::NowPlayingInfo).
        let seconds_field = |key: &str| -> Option<f64> {
            dict.get(&CFString::from_str(key)).and_then(|v: CFRetained<CFType>| {
                v.downcast_ref::<objc2_core_foundation::CFNumber>().and_then(|n| n.as_cgfloat())
            })
        };
        let duration_ms = seconds_field("kMRMediaRemoteNowPlayingInfoDuration").map(|s| (s * 1000.0) as u64).unwrap_or(0);

        // `ElapsedTime` is a *snapshot*, not a live value — confirmed by
        // testing against QuickTime Player: it read back as exactly 0
        // seconds after several real seconds of playback, unmoving. Every
        // real MediaRemote-based "now playing" tool interpolates using the
        // paired `Timestamp` field for exactly this reason: the source app
        // only pushes a fresh dict on discrete events (play, pause, seek,
        // track change), not continuously, so a client reading the raw
        // elapsed value gets whatever it was at the *last* such event, not
        // "right now". True live position while playing is `snapshot
        // elapsed + (now - snapshot timestamp)`; while paused, playback
        // hasn't advanced at all since the snapshot, so the raw value is
        // already correct as-is.
        let snapshot_elapsed_s = seconds_field("kMRMediaRemoteNowPlayingInfoElapsedTime").unwrap_or(0.0);
        let snapshot_timestamp = dict
            .get(&CFString::from_str("kMRMediaRemoteNowPlayingInfoTimestamp"))
            .and_then(|v: CFRetained<CFType>| v.downcast_ref::<objc2_core_foundation::CFDate>().map(|d| d.absolute_time()));
        let elapsed_s = match (is_playing, snapshot_timestamp) {
            (true, Some(ts)) => {
                let now = objc2_core_foundation::CFAbsoluteTimeGetCurrent();
                // Clamp at 0 in case the snapshot timestamp is somehow in
                // the future (clock oddities) — better a frozen position
                // than one that runs backwards.
                snapshot_elapsed_s + (now - ts).max(0.0)
            }
            _ => snapshot_elapsed_s,
        };
        // A live position can't legitimately exceed the track's own
        // duration — the interpolation above has no way to know playback
        // actually stopped/looped at the end without a fresh snapshot, so
        // without this a track left playing past its nominal end (the
        // watcher's next real snapshot hasn't arrived yet) would show a
        // progress bar stuck past 100% instead of pinned at the end.
        let elapsed_s = if duration_ms > 0 { elapsed_s.min(duration_ms as f64 / 1000.0) } else { elapsed_s };
        let position_ms = (elapsed_s * 1000.0) as u64;

        NowPlayingInfo { title, artist, album, artwork, is_playing, position_ms, duration_ms, volume_percent: None }
    }
}

/// System output volume via CoreAudio — public, documented API (unlike
/// everything else in this file), linked directly rather than dlopen'd.
/// There's no "now playing" volume distinct from the system's overall
/// output volume; this is the same level the physical volume keys and the
/// menu-bar volume slider control.
mod system_volume {
    use std::ffi::c_void;

    type AudioObjectId = u32;
    type OsStatus = i32;

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    const fn fourcc(s: &[u8; 4]) -> u32 {
        ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
    }

    const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectId = 1;
    const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: u32 = fourcc(b"dOut");
    const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT: u32 = fourcc(b"outp");
    const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;
    const K_AUDIO_DEVICE_PROPERTY_VOLUME_SCALAR: u32 = fourcc(b"volm");

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C-unwind" {
        fn AudioObjectHasProperty(object_id: AudioObjectId, address: *const AudioObjectPropertyAddress) -> u8;
        fn AudioObjectGetPropertyData(
            object_id: AudioObjectId,
            address: *const AudioObjectPropertyAddress,
            qualifier_data_size: u32,
            qualifier_data: *const c_void,
            io_data_size: *mut u32,
            out_data: *mut c_void,
        ) -> OsStatus;
        fn AudioObjectSetPropertyData(
            object_id: AudioObjectId,
            address: *const AudioObjectPropertyAddress,
            qualifier_data_size: u32,
            qualifier_data: *const c_void,
            in_data_size: u32,
            in_data: *const c_void,
        ) -> OsStatus;
    }

    fn default_output_device() -> Option<AudioObjectId> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        let mut device_id: AudioObjectId = 0;
        let mut size = std::mem::size_of::<AudioObjectId>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                (&mut device_id) as *mut AudioObjectId as *mut c_void,
            )
        };
        (status == 0 && device_id != 0).then_some(device_id)
    }

    fn volume_address(element: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_VOLUME_SCALAR,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element,
        }
    }

    fn has_property(device_id: u32, address: &AudioObjectPropertyAddress) -> bool {
        unsafe { AudioObjectHasProperty(device_id, address) != 0 }
    }

    fn get_element(device_id: u32, element: u32) -> Option<f32> {
        let address = volume_address(element);
        if !has_property(device_id, &address) {
            return None;
        }
        let mut volume: f32 = 0.0;
        let mut size = std::mem::size_of::<f32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(device_id, &address, 0, std::ptr::null(), &mut size, (&mut volume) as *mut f32 as *mut c_void)
        };
        (status == 0).then_some(volume)
    }

    fn set_element(device_id: u32, element: u32, volume: f32) -> bool {
        let address = volume_address(element);
        if !has_property(device_id, &address) {
            return false;
        }
        let volume = volume.clamp(0.0, 1.0);
        let status = unsafe {
            AudioObjectSetPropertyData(
                device_id,
                &address,
                0,
                std::ptr::null(),
                std::mem::size_of::<f32>() as u32,
                (&volume) as *const f32 as *const c_void,
            )
        };
        status == 0
    }

    /// Element 0 ("main") covers some output devices, but plenty of real
    /// hardware — this dev machine's built-in speakers included — only
    /// expose separate per-channel volume instead (element 1 = left,
    /// element 2 = right, the near-universal convention for stereo
    /// devices). Reads whichever is actually available, and when setting,
    /// writes to every element that responds (main if present, otherwise
    /// both channels) so mono *and* stereo devices end up adjusted either
    /// way.
    fn get(device_id: u32) -> Option<f32> {
        get_element(device_id, K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN).or_else(|| get_element(device_id, 1))
    }

    fn set(device_id: u32, volume: f32) -> bool {
        let mut set_any = false;
        for element in [K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN, 1, 2] {
            if set_element(device_id, element, volume) {
                set_any = true;
            }
        }
        set_any
    }

    pub fn step(delta: f32) {
        let Some(device_id) = default_output_device() else {
            tracing::debug!("couldn't get default output device for volume change");
            return;
        };
        let Some(current) = get(device_id) else {
            tracing::debug!("couldn't read current output volume");
            return;
        };
        if !set(device_id, current + delta) {
            tracing::debug!("couldn't set output volume");
        }
    }

    /// Current output volume, 0.0–1.0, for `NowPlayingInfo::volume_percent`
    /// — read-only counterpart to `step`, same default-output-device +
    /// main-or-per-channel-element lookup.
    pub fn current() -> Option<f32> {
        get(default_output_device()?)
    }
}

#[cfg(test)]
mod manual_verification {
    // Not a real test — run with
    // `cargo test -p continuityd send_play_pause -- --ignored --nocapture`
    // while Music.app (or any other app) is playing something, to confirm
    // this actually controls playback and isn't just type-checking cleanly.
    // `#[ignore]`d so it never runs as part of a normal test pass.
    use super::*;

    #[test]
    #[ignore]
    fn send_play_pause() {
        MacMediaController.handle(MediaCommand::PlayPause);
    }

    #[test]
    #[ignore]
    fn send_next() {
        MacMediaController.handle(MediaCommand::Next);
    }

    #[test]
    #[ignore]
    fn send_previous() {
        MacMediaController.handle(MediaCommand::Previous);
    }

    #[test]
    #[ignore]
    fn send_volume_up() {
        MacMediaController.handle(MediaCommand::VolumeUp);
    }

    #[test]
    #[ignore]
    fn send_volume_down() {
        MacMediaController.handle(MediaCommand::VolumeDown);
    }

    #[test]
    #[ignore]
    fn print_ax_trust() {
        println!("AXIsProcessTrusted = {}", unsafe { AXIsProcessTrusted() != 0 });
    }

    #[test]
    #[ignore]
    fn print_now_playing() {
        match MacMediaController.now_playing() {
            Some(info) => {
                println!(
                    "title={:?} artist={:?} album={:?} is_playing={} position_ms={} duration_ms={} volume_percent={:?} artwork_bytes={}",
                    info.title,
                    info.artist,
                    info.album,
                    info.is_playing,
                    info.position_ms,
                    info.duration_ms,
                    info.volume_percent,
                    info.artwork.len(),
                );
            }
            None => println!("now_playing() returned None"),
        }
    }

    #[test]
    #[ignore]
    fn print_volume() {
        println!("current volume = {:?}", super::system_volume::current());
    }

    // Run alongside `print_now_playing` (before/after) to confirm the
    // reported `position_ms` actually moved to roughly the requested spot
    // — this only proves the call didn't error, not that playback actually
    // jumped.
    #[test]
    #[ignore]
    fn send_seek_to_30s() {
        MacMediaController.handle(MediaCommand::Seek { position_ms: 30_000 });
    }
}

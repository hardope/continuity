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
use std::time::Duration;

// From IOKit/hidsystem/ev_keymap.h — not exposed as a public header import,
// these are the well-known reverse-engineered values every media-key-
// simulation project (dating back to SPMediaKeyTap) has used for years.
const NX_KEYTYPE_PLAY: i64 = 16;
const NX_KEYTYPE_NEXT: i64 = 17;
const NX_KEYTYPE_PREVIOUS: i64 = 18;

pub struct MacMediaController;

impl MediaController for MacMediaController {
    fn handle(&self, command: MediaCommand) {
        let key = match command {
            MediaCommand::PlayPause => NX_KEYTYPE_PLAY,
            MediaCommand::Next => NX_KEYTYPE_NEXT,
            MediaCommand::Previous => NX_KEYTYPE_PREVIOUS,
        };
        post_media_key(key);
    }

    fn now_playing(&self) -> Option<NowPlayingInfo> {
        now_playing::get()
    }
}

fn post_media_key(key: i64) {
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

    pub fn get() -> Option<NowPlayingInfo> {
        unsafe {
            let path = CStr::from_bytes_with_nul(
                b"/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote\0",
            )
            .unwrap();
            let handle = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY);
            if handle.is_null() {
                tracing::debug!("couldn't dlopen MediaRemote.framework");
                return None;
            }

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
                    Some(parse_dict(dict))
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

        NowPlayingInfo { title, artist, album, artwork, is_playing }
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
    fn print_now_playing() {
        match MacMediaController.now_playing() {
            Some(info) => {
                println!(
                    "title={:?} artist={:?} album={:?} is_playing={} artwork_bytes={}",
                    info.title,
                    info.artist,
                    info.album,
                    info.is_playing,
                    info.artwork.len(),
                );
            }
            None => println!("now_playing() returned None"),
        }
    }
}

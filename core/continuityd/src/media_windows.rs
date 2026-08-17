//! Windows media control. Unlike macOS, both halves of this are public,
//! documented APIs rather than reverse-engineered private frameworks:
//! transport commands go through `SendInput` with the standard
//! `VK_MEDIA_*`/`VK_VOLUME_*` virtual keys (exactly what a keyboard's own
//! media keys send — the OS routes them to whichever app owns the System
//! Media Transport Controls session, no per-app integration needed), and
//! now-playing reads (including seeking, via `TryChangePlaybackPositionAsync`)
//! go through the WinRT `GlobalSystemMediaTransportControlsSessionManager`
//! API, and system volume readback through the separate Win32
//! `IAudioEndpointVolume` COM interface (SMTC has no volume surface of its
//! own).
//!
//! **Not verified against a real playing session** — there's no Windows
//! machine in this development environment, only CI's `windows-latest`
//! runner, which can confirm this compiles against the real Windows SDK
//! but can't confirm it actually works the way `core/continuityd/src/
//! media_mac.rs` was verified (real playback, real state changes
//! observed). Implemented from the documented API surface; flag anything
//! that misbehaves. This applies doubly to the timeline-position
//! interpolation specifically (see `read_timeline`) — the underlying
//! "position is a stale snapshot, not a live value" behavior *was*
//! confirmed by hand, just on macOS's equivalent API, not this one.

use continuity_daemon::MediaController;
use continuity_proto::{MediaCommand, NowPlayingInfo};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession, GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
};

const VK_MEDIA_NEXT_TRACK: u16 = 0xB0;
const VK_MEDIA_PREV_TRACK: u16 = 0xB1;
const VK_MEDIA_PLAY_PAUSE: u16 = 0xB3;
const VK_VOLUME_UP: u16 = 0xAF;
const VK_VOLUME_DOWN: u16 = 0xAE;

pub struct WindowsMediaController;

impl MediaController for WindowsMediaController {
    fn handle(&self, command: MediaCommand) {
        if let MediaCommand::Seek { position_ms } = command {
            if let Err(e) = seek(position_ms) {
                tracing::debug!("couldn't seek: {e:?}");
            }
            return;
        }
        let vk = match command {
            MediaCommand::PlayPause => VK_MEDIA_PLAY_PAUSE,
            MediaCommand::Next => VK_MEDIA_NEXT_TRACK,
            MediaCommand::Previous => VK_MEDIA_PREV_TRACK,
            MediaCommand::VolumeUp => VK_VOLUME_UP,
            MediaCommand::VolumeDown => VK_VOLUME_DOWN,
            MediaCommand::Seek { .. } => unreachable!("handled above"),
        };
        send_key(vk);
    }

    fn now_playing(&self) -> Option<NowPlayingInfo> {
        match read_now_playing() {
            Ok(info) => info,
            Err(e) => {
                tracing::debug!("couldn't read Windows now-playing info: {e:?}");
                None
            }
        }
    }
}

fn send_key(vk: u16) {
    let key_down = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: Default::default(), time: 0, dwExtraInfo: 0 },
        },
    };
    let key_up = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: KEYEVENTF_KEYUP, time: 0, dwExtraInfo: 0 },
        },
    };
    let inputs = [key_down, key_up];
    // SAFETY: `inputs` is a valid, correctly-sized slice of INPUT structs
    // living on the stack for the duration of this call, matching what
    // SendInput expects; `size_of::<INPUT>()` is the exact per-element
    // size it requires.
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn read_now_playing() -> windows::core::Result<Option<NowPlayingInfo>> {
    // WinRT calls need COM initialized on the calling thread. This runs on
    // a `spawn_blocking` worker thread whose COM apartment state isn't
    // otherwise guaranteed — safe to call even if something else already
    // initialized it (returns S_FALSE, still Ok).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.get()?;
    let Ok(session) = manager.GetCurrentSession() else {
        return Ok(None);
    };

    let props = session.TryGetMediaPropertiesAsync()?.get()?;
    let title = props.Title().ok().map(|h| h.to_string()).filter(|s| !s.is_empty());
    let artist = props.Artist().ok().map(|h| h.to_string()).filter(|s| !s.is_empty());
    let album = props.AlbumTitle().ok().map(|h| h.to_string()).filter(|s| !s.is_empty());
    let artwork = read_thumbnail(props.Thumbnail().ok()).unwrap_or_default();

    let is_playing = session
        .GetPlaybackInfo()
        .and_then(|info| info.PlaybackStatus())
        .map(|status| status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
        .unwrap_or(false);

    let (position_ms, duration_ms) = read_timeline(&session, is_playing).unwrap_or((0, 0));
    let volume_percent = current_volume();

    Ok(Some(NowPlayingInfo { title, artist, album, artwork, is_playing, position_ms, duration_ms, volume_percent }))
}

/// `GlobalSystemMediaTransportControlsSessionTimelineProperties::Position`
/// is a snapshot as of `LastUpdatedTime`, not a continuously live value —
/// the same kind of gap confirmed by hand against real playback for
/// macOS's `kMRMediaRemoteNowPlayingInfoElapsedTime` (see the long comment
/// in `media_mac.rs`'s `parse_dict`), and documented by Microsoft itself
/// for this API rather than something to test blind here: a source app is
/// only expected to push a new timeline update on discrete events (play,
/// pause, seek), not every frame. While playing, the real position is
/// `Position + (now - LastUpdatedTime)`; while paused, nothing has
/// advanced since the snapshot, so the raw value already holds. `TimeSpan`
/// and `DateTimeOffset` are both 100-nanosecond-tick counts (`TimeSpan`
/// from zero, `DateTimeOffset` from the Windows FILETIME epoch,
/// 1601-01-01) — `now_filetime_ticks` below converts `SystemTime::now()`
/// into that same epoch to make the subtraction meaningful.
fn read_timeline(session: &GlobalSystemMediaTransportControlsSession, is_playing: bool) -> windows::core::Result<(u64, u64)> {
    let timeline = session.GetTimelineProperties()?;
    let position_ticks = timeline.Position()?.Duration;
    let end_ticks = timeline.EndTime()?.Duration;
    let start_ticks = timeline.StartTime()?.Duration;
    let duration_ticks = (end_ticks - start_ticks).max(0);

    let live_position_ticks = if is_playing {
        let elapsed_since_update = (now_filetime_ticks() - timeline.LastUpdatedTime()?.UniversalTime).max(0);
        position_ticks + elapsed_since_update
    } else {
        position_ticks
    };
    // Same reasoning as the macOS clamp: interpolation has no way to know
    // playback already stopped/looped without a fresh snapshot.
    let live_position_ticks = if duration_ticks > 0 { live_position_ticks.min(duration_ticks) } else { live_position_ticks };

    Ok(((live_position_ticks / 10_000).max(0) as u64, (duration_ticks / 10_000) as u64))
}

const FILETIME_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;

fn now_filetime_ticks() -> i64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (now.as_secs() as i64 + FILETIME_EPOCH_OFFSET_SECS) * 10_000_000 + (now.subsec_nanos() as i64 / 100)
}

fn seek(position_ms: u64) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.get()?;
    let session = manager.GetCurrentSession()?;
    // `TryChangePlaybackPositionAsync` takes 100-nanosecond ticks from the
    // start of the track, same unit as everything else in this file's
    // timeline handling — returns whether the source app actually
    // supports seeking, not just whether the call itself succeeded; logged
    // if explicitly false so a "nothing happened" report from a
    // non-seekable source (radio streams commonly don't support this) has
    // something to point to.
    let accepted = session.TryChangePlaybackPositionAsync(position_ms as i64 * 10_000)?.get()?;
    if !accepted {
        tracing::debug!("current session declined the seek request (likely doesn't support seeking)");
    }
    Ok(())
}

/// System output volume via the Core Audio `IAudioEndpointVolume` COM
/// interface — the same level the physical volume keys (and `VK_VOLUME_*`
/// above) control. Unlike media transport, there's no WinRT/SMTC surface
/// for this at all; it's the older Win32 MMDevice API, same category as
/// macOS's separate CoreAudio route for the same reason (see
/// `system_volume` in `media_mac.rs`).
fn endpoint_volume() -> windows::core::Result<IAudioEndpointVolume> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    }
}

fn current_volume() -> Option<f32> {
    let volume = endpoint_volume().ok()?;
    unsafe { volume.GetMasterVolumeLevelScalar().ok() }
}

fn read_thumbnail(reference: Option<IRandomAccessStreamReference>) -> windows::core::Result<Vec<u8>> {
    let Some(reference) = reference else {
        return Ok(Vec::new());
    };
    let stream = reference.OpenReadAsync()?.get()?;
    let size = stream.Size()? as u32;
    if size == 0 {
        return Ok(Vec::new());
    }
    let reader = DataReader::CreateDataReader(&stream)?;
    reader.LoadAsync(size)?.get()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf)?;
    Ok(buf)
}

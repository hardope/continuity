//! Windows media control. Unlike macOS, both halves of this are public,
//! documented APIs rather than reverse-engineered private frameworks:
//! transport commands go through `SendInput` with the standard
//! `VK_MEDIA_*`/`VK_VOLUME_*` virtual keys (exactly what a keyboard's own
//! media keys send — the OS routes them to whichever app owns the System
//! Media Transport Controls session, no per-app integration needed), and
//! now-playing reads go through the WinRT
//! `GlobalSystemMediaTransportControlsSessionManager` API.
//!
//! **Not verified against a real playing session** — there's no Windows
//! machine in this development environment, only CI's `windows-latest`
//! runner, which can confirm this compiles against the real Windows SDK
//! but can't confirm it actually works the way `core/continuityd/src/
//! media_mac.rs` was verified (real playback, real state changes
//! observed). Implemented from the documented API surface; flag anything
//! that misbehaves.

use continuity_daemon::MediaController;
use continuity_proto::{MediaCommand, NowPlayingInfo};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager, GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
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
        let vk = match command {
            MediaCommand::PlayPause => VK_MEDIA_PLAY_PAUSE,
            MediaCommand::Next => VK_MEDIA_NEXT_TRACK,
            MediaCommand::Previous => VK_MEDIA_PREV_TRACK,
            MediaCommand::VolumeUp => VK_VOLUME_UP,
            MediaCommand::VolumeDown => VK_VOLUME_DOWN,
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

    Ok(Some(NowPlayingInfo { title, artist, album, artwork, is_playing }))
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

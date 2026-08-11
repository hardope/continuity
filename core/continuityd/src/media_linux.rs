//! Linux media control via MPRIS (Media Player Remote Interfacing
//! Specification) over D-Bus — the de facto standard most Linux media
//! players implement (VLC, Rhythmbox, browsers via their MPRIS plugins,
//! Spotify's Linux client, ...). Unlike macOS's MediaRemote, this is a
//! fully public, community-standardized protocol
//! (specifications.freedesktop.org/mpris-spec), not reverse-engineered.
//!
//! **Not verified against a real playing session** — no Linux machine
//! with a real desktop environment and a media player in this development
//! environment, only CI's `ubuntu-latest` runner (headless, no session
//! D-Bus, nothing implementing MPRIS), which can only confirm this
//! compiles, not that it actually controls anything. Implemented directly
//! from the MPRIS spec; flag anything that misbehaves.
//!
//! MPRIS has no single "the current session" concept the way macOS/
//! Windows do — every player is independent, and more than one can be
//! running at once. This targets whichever `org.mpris.MediaPlayer2.*` bus
//! name is found first, a real, known limitation (no "most recently
//! active" tracking) shared with most simple MPRIS tools.

use continuity_daemon::MediaController;
use continuity_proto::{MediaCommand, NowPlayingInfo};
use std::collections::HashMap;
use zbus::blocking::{fdo::DBusProxy, Connection};
use zbus::zvariant::OwnedValue;

const VOLUME_STEP: f64 = 0.05;

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2",
    gen_blocking = true,
    gen_async = false
)]
trait Player {
    fn play_pause(&self) -> zbus::Result<()>;
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn volume(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn set_volume(&self, value: f64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

pub struct LinuxMediaController;

impl MediaController for LinuxMediaController {
    fn handle(&self, command: MediaCommand) {
        if let Err(e) = send_command(command) {
            tracing::debug!("couldn't send MPRIS command: {e}");
        }
    }

    fn now_playing(&self) -> Option<NowPlayingInfo> {
        match read_now_playing() {
            Ok(info) => info,
            Err(e) => {
                tracing::debug!("couldn't read MPRIS now-playing info: {e}");
                None
            }
        }
    }
}

fn first_player_bus_name(conn: &Connection) -> zbus::Result<Option<String>> {
    let proxy = DBusProxy::new(conn)?;
    let names = proxy.list_names()?;
    Ok(names.into_iter().map(|n| n.to_string()).find(|n| n.starts_with("org.mpris.MediaPlayer2.")))
}

fn player_proxy<'a>(conn: &'a Connection, bus_name: &str) -> zbus::Result<PlayerProxy<'a>> {
    PlayerProxy::builder(conn).destination(bus_name.to_string())?.build()
}

fn send_command(command: MediaCommand) -> zbus::Result<()> {
    let conn = Connection::session()?;
    let Some(bus_name) = first_player_bus_name(&conn)? else {
        return Ok(());
    };
    let player = player_proxy(&conn, &bus_name)?;
    match command {
        MediaCommand::PlayPause => player.play_pause(),
        MediaCommand::Next => player.next(),
        MediaCommand::Previous => player.previous(),
        MediaCommand::VolumeUp | MediaCommand::VolumeDown => {
            let delta = if command == MediaCommand::VolumeUp { VOLUME_STEP } else { -VOLUME_STEP };
            let current = player.volume()?;
            player.set_volume((current + delta).clamp(0.0, 1.0))
        }
    }
}

fn read_now_playing() -> zbus::Result<Option<NowPlayingInfo>> {
    let conn = Connection::session()?;
    let Some(bus_name) = first_player_bus_name(&conn)? else {
        return Ok(None);
    };
    let player = player_proxy(&conn, &bus_name)?;

    let metadata = player.metadata().unwrap_or_default();
    let str_prop = |key: &str| -> Option<String> {
        metadata.get(key).and_then(|v| <&str>::try_from(v).ok()).map(str::to_string).filter(|s| !s.is_empty())
    };
    let title = str_prop("xesam:title");
    let album = str_prop("xesam:album");
    // `OwnedValue` has no infallible `Clone` (only fallible `try_clone`), so
    // this can't reuse `str_prop`'s borrowed-conversion path — going from
    // an array element to an owned `Vec<String>` needs an owned `Value`.
    let artist = metadata
        .get("xesam:artist")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .and_then(|v| v.into_iter().next());

    // Artwork is a URL in MPRIS, not raw bytes (unlike macOS/Windows) —
    // only `file://` is handled, since fetching an arbitrary http(s) URL
    // on every metadata poll isn't something to do without the user
    // knowing about it.
    let artwork = str_prop("mpris:artUrl")
        .as_deref()
        .and_then(|url| url.strip_prefix("file://"))
        .and_then(|path| std::fs::read(path).ok())
        .unwrap_or_default();

    let is_playing = player.playback_status().map(|s| s == "Playing").unwrap_or(false);

    Ok(Some(NowPlayingInfo { title, artist, album, artwork, is_playing }))
}

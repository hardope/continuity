//! Linux screen lock/unlock via systemd-logind — the session manager's own
//! `Lock()`/`Unlock()` methods on this user's graphical session object,
//! exactly what `loginctl lock-session` / `loginctl unlock-session` call.
//! logind doesn't draw a lock screen itself; it signals the session, and
//! the desktop's own locker reacts. GNOME Shell and KDE Plasma's
//! kscreenlocker both honor that signal; some standalone lockers (swaylock,
//! i3lock, ...) don't, which is why `unlock` checks afterwards whether the
//! session actually reports itself unlocked rather than trusting the
//! method call alone (the call "succeeds" either way — it only sends a
//! signal).
//!
//! Permission: logind lets a session's own user lock/unlock it without
//! any polkit prompt — the same reason plain `loginctl unlock-session`
//! needs no sudo for your own session. This process runs as that user,
//! inside that session, so it gets exactly that and nothing more.
//!
//! **Not verified against a live session** — like `media_linux.rs`, only
//! compile-checked (CI's headless runner has no logind graphical session
//! to talk to). Written against the documented `org.freedesktop.login1`
//! D-Bus API (`man 5 org.freedesktop.login1`).

use continuity_daemon::{ScreenLockController, ScreenLockError};
use std::time::{Duration, Instant};
use zbus::blocking::Connection;
use zbus::zvariant::OwnedObjectPath;

/// How long to wait for the lock screen to actually go away after asking.
/// GNOME's unlock animation alone takes a few hundred milliseconds.
const UNLOCK_CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = true,
    gen_async = false
)]
trait Manager {
    fn get_session(&self, session_id: &str) -> zbus::Result<OwnedObjectPath>;
    #[zbus(name = "GetSessionByPID")]
    fn get_session_by_pid(&self, pid: u32) -> zbus::Result<OwnedObjectPath>;
    fn get_user(&self, uid: u32) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(interface = "org.freedesktop.login1.Session", default_service = "org.freedesktop.login1", gen_blocking = true, gen_async = false)]
trait Session {
    fn lock(&self) -> zbus::Result<()>;
    fn unlock(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(interface = "org.freedesktop.login1.User", default_service = "org.freedesktop.login1", gen_blocking = true, gen_async = false)]
trait User {
    /// The user's primary graphical session, as `(session id, object path)`
    /// — `("", "/")` when there isn't one.
    #[zbus(property)]
    fn display(&self) -> zbus::Result<(String, OwnedObjectPath)>;
}

pub struct LinuxScreenLock;

impl ScreenLockController for LinuxScreenLock {
    fn lock(&self) -> Result<(), ScreenLockError> {
        let conn = system_bus()?;
        let session = session_proxy(&conn)?;
        session.lock().map_err(|e| ScreenLockError::Failed(format!("logind refused to lock the session: {e}")))
    }

    fn unlock(&self) -> Result<(), ScreenLockError> {
        let conn = system_bus()?;
        let session = session_proxy(&conn)?;
        let was_locked = session.locked_hint().unwrap_or(true);
        session.unlock().map_err(|e| ScreenLockError::Failed(format!("logind refused to unlock the session: {e}")))?;
        if !was_locked {
            return Ok(());
        }
        let deadline = Instant::now() + UNLOCK_CONFIRM_TIMEOUT;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(150));
            if let Ok(false) = session.locked_hint() {
                return Ok(());
            }
        }
        Err(ScreenLockError::Failed(
            "the lock screen didn't respond — remote unlock works with GNOME and KDE Plasma, but some screen lockers (swaylock, i3lock, ...) ignore it"
                .to_string(),
        ))
    }
}

fn system_bus() -> Result<Connection, ScreenLockError> {
    Connection::system().map_err(|e| ScreenLockError::Unsupported(format!("no system D-Bus: {e}")))
}

/// Finds this user's graphical session: the one this process was started
/// in if logind knows it (`XDG_SESSION_ID`, then by PID), otherwise the
/// user's primary display session — the fallback matters because GNOME and
/// other systemd-managed desktops launch autostart apps in their own
/// systemd scopes, outside the session's own process tree, where neither
/// of the first two can find it.
fn session_proxy(conn: &Connection) -> Result<SessionProxy<'_>, ScreenLockError> {
    let manager = ManagerProxy::new(conn).map_err(|e| ScreenLockError::Unsupported(format!("systemd-logind isn't available: {e}")))?;

    let from_env = std::env::var("XDG_SESSION_ID").ok().filter(|id| !id.is_empty()).and_then(|id| manager.get_session(&id).ok());
    let path = match from_env.or_else(|| manager.get_session_by_pid(std::process::id()).ok()) {
        Some(path) => path,
        None => display_session(conn, &manager)?,
    };

    SessionProxy::builder(conn)
        .path(path)
        .and_then(|b| b.cache_properties(zbus::CacheProperties::No).build())
        .map_err(|e| ScreenLockError::Failed(format!("couldn't reach the logind session: {e}")))
}

fn display_session(conn: &Connection, manager: &ManagerProxy<'_>) -> Result<OwnedObjectPath, ScreenLockError> {
    // SAFETY: getuid() has no preconditions and can't fail.
    let uid = unsafe { libc::getuid() };
    let user_path = manager.get_user(uid).map_err(|e| ScreenLockError::Unsupported(format!("logind doesn't know this user: {e}")))?;
    let user = UserProxy::builder(conn)
        .path(user_path)
        .and_then(|b| b.cache_properties(zbus::CacheProperties::No).build())
        .map_err(|e| ScreenLockError::Unsupported(format!("couldn't reach the logind user: {e}")))?;
    let (session_id, path) = user.display().map_err(|e| ScreenLockError::Unsupported(format!("couldn't read the display session: {e}")))?;
    if session_id.is_empty() {
        return Err(ScreenLockError::Unsupported("no graphical login session for this user".to_string()));
    }
    Ok(path)
}

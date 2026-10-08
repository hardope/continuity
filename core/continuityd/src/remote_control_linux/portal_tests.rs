//! The portal client against a mock xdg-desktop-portal on a real D-Bus
//! (peer-to-peer) connection — no desktop needed. The mock follows the
//! documented portal behavior the client depends on:
//! - a Request's path is /org/freedesktop/portal/desktop/request/SENDER/TOKEN,
//!   TOKEN being the caller's `handle_token`, or one the portal invents when
//!   the caller passed none — which is exactly what left 0.1.6-beta.3's
//!   client waiting forever, since it never passed one;
//! - a Session's path is .../session/SENDER/TOKEN from `session_handle_token`,
//!   returned typed `s` in CreateSession's results;
//! - Start's results carry devices (u), streams a(ua{sv}) with size (ii), and
//!   restore_token (s) for a persistent session;
//! - methods taking a session reject one that doesn't exist.
//! Setup steps answer *before* their method returns (stricter than the real
//! portal); Start answers 100ms after, like a person clicking.

use std::time::Duration;
use super::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, ObjectServer};

const CLIENT_NAME: &str = ":1.42";
const SENDER: &str = "1_42";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const NODE_ID: u32 = 57;

#[derive(Clone, Copy, PartialEq, Debug)]
enum StartBehavior {
    Approve,
    Decline,
    Never,
}

#[derive(Default)]
struct Record {
    calls: Vec<(String, HashMap<String, String>)>,
    closed_requests: Vec<String>,
    closed_sessions: Vec<String>,
    input: Vec<String>,
    sessions: HashSet<String>,
    persistent_sessions: HashSet<String>,
}

#[derive(Clone)]
struct Shared {
    start: StartBehavior,
    reject_persistence: bool,
    record: Arc<Mutex<Record>>,
}

impl Shared {
    fn log(&self, method: &str, options: &HashMap<String, OwnedValue>) {
        let rendered = options.iter().map(|(k, v)| (k.clone(), render(v))).collect();
        self.record.lock().unwrap().calls.push((method.to_string(), rendered));
    }

    fn check_session(&self, session: &OwnedObjectPath) -> zbus::fdo::Result<()> {
        if self.record.lock().unwrap().sessions.contains(session.as_str()) {
            Ok(())
        } else {
            Err(zbus::fdo::Error::InvalidArgs(format!("Invalid session {session}")))
        }
    }
}

fn render(v: &OwnedValue) -> String {
    match &**v {
        Value::Str(s) => s.to_string(),
        Value::U32(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

fn request_path(options: &HashMap<String, OwnedValue>) -> String {
    static INVENTED: AtomicU64 = AtomicU64::new(1000);
    let token = match options.get("handle_token").map(|v| &**v) {
        Some(Value::Str(s)) => s.to_string(),
        // What xdg-desktop-portal does when the caller passes no handle_token.
        _ => format!("t{}", INVENTED.fetch_add(1, Ordering::Relaxed)),
    };
    format!("{PORTAL_PATH}/request/{SENDER}/{token}")
}

async fn respond(conn: &Connection, path: &str, code: u32, results: HashMap<&str, Value<'_>>) {
    conn.emit_signal(None::<&str>, path, "org.freedesktop.portal.Request", "Response", &(code, results))
        .await
        .expect("emit Response");
}

/// Answers a moment after the method call has returned, like a person
/// clicking the dialog.
fn respond_later(conn: &Connection, path: String, code: u32, approve: bool, restore_token: Option<&'static str>) {
    let conn = conn.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        zbus::block_on(async move {
            let mut results: HashMap<&str, Value> = HashMap::new();
            if approve {
                let mut props: HashMap<&str, Value> = HashMap::new();
                props.insert("size", Value::from((1440i32, 900i32)));
                props.insert("position", Value::from((0i32, 0i32)));
                props.insert("source_type", Value::from(1u32));
                results.insert("devices", Value::from(3u32));
                results.insert("streams", Value::from(vec![(NODE_ID, props)]));
                if let Some(token) = restore_token {
                    results.insert("restore_token", Value::from(token));
                }
            }
            respond(&conn, &path, code, results).await;
        });
    });
}

struct MockRemoteDesktop(Shared);

#[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop")]
impl MockRemoteDesktop {
    async fn create_session(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.0.log("CreateSession", &options);
        let request = request_path(&options);
        let token = match options.get("session_handle_token").map(|v| &**v) {
            Some(Value::Str(s)) => s.to_string(),
            _ => "s1".to_string(),
        };
        let session = format!("{PORTAL_PATH}/session/{SENDER}/{token}");
        server.at(session.as_str(), MockSession { path: session.clone(), record: self.0.record.clone() }).await?;
        self.0.record.lock().unwrap().sessions.insert(session.clone());
        let mut results: HashMap<&str, Value> = HashMap::new();
        // Typed `s`, as xdg-desktop-portal sends it.
        results.insert("session_handle", Value::from(session.as_str()));
        respond(conn, &request, 0, results).await;
        Ok(OwnedObjectPath::try_from(request).unwrap())
    }

    async fn select_devices(
        &self,
        #[zbus(connection)] conn: &Connection,
        session: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.0.log("SelectDevices", &options);
        self.0.check_session(&session)?;
        if options.contains_key("persist_mode") {
            if self.0.reject_persistence {
                return Err(zbus::fdo::Error::InvalidArgs("persist_mode not allowed".into()));
            }
            self.0.record.lock().unwrap().persistent_sessions.insert(session.to_string());
        }
        let request = request_path(&options);
        respond(conn, &request, 0, HashMap::new()).await;
        Ok(OwnedObjectPath::try_from(request).unwrap())
    }

    async fn start(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
        session: OwnedObjectPath,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.0.log("Start", &options);
        self.0.check_session(&session)?;
        let request = request_path(&options);
        server.at(request.as_str(), MockRequest { path: request.clone(), record: self.0.record.clone() }).await?;
        let persistent = self.0.record.lock().unwrap().persistent_sessions.contains(session.as_str());
        match self.0.start {
            StartBehavior::Approve => respond_later(conn, request.clone(), 0, true, persistent.then_some("next-token")),
            StartBehavior::Decline => respond_later(conn, request.clone(), 1, false, None),
            StartBehavior::Never => {}
        }
        Ok(OwnedObjectPath::try_from(request).unwrap())
    }

    async fn notify_pointer_motion_absolute(
        &self,
        session: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        stream: u32,
        x: f64,
        y: f64,
    ) -> zbus::fdo::Result<()> {
        self.0.check_session(&session)?;
        self.0.record.lock().unwrap().input.push(format!("motion {stream} {x} {y}"));
        Ok(())
    }

    async fn notify_keyboard_keycode(
        &self,
        session: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        keycode: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        self.0.check_session(&session)?;
        self.0.record.lock().unwrap().input.push(format!("key {keycode} {state}"));
        Ok(())
    }

    async fn notify_pointer_button(
        &self,
        session: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
        button: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        self.0.check_session(&session)?;
        self.0.record.lock().unwrap().input.push(format!("button {button} {state}"));
        Ok(())
    }

    async fn notify_pointer_axis(
        &self,
        session: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
        dx: f64,
        dy: f64,
    ) -> zbus::fdo::Result<()> {
        self.0.check_session(&session)?;
        let finish = options.get("finish").map(render).unwrap_or_default();
        self.0.record.lock().unwrap().input.push(format!("axis {dx} {dy} finish={finish}"));
        Ok(())
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

struct MockScreenCast(Shared);

#[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
impl MockScreenCast {
    async fn select_sources(
        &self,
        #[zbus(connection)] conn: &Connection,
        session: OwnedObjectPath,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.0.log("SelectSources", &options);
        self.0.check_session(&session)?;
        let request = request_path(&options);
        respond(conn, &request, 0, HashMap::new()).await;
        Ok(OwnedObjectPath::try_from(request).unwrap())
    }

    async fn open_pipe_wire_remote(
        &self,
        session: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        self.0.check_session(&session)?;
        let (ours, _theirs) = std::os::unix::net::UnixStream::pair().map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(std::os::fd::OwnedFd::from(ours).into())
    }

    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        7
    }
}

struct MockRequest {
    path: String,
    record: Arc<Mutex<Record>>,
}

#[zbus::interface(name = "org.freedesktop.portal.Request")]
impl MockRequest {
    async fn close(&self) {
        self.record.lock().unwrap().closed_requests.push(self.path.clone());
    }
}

struct MockSession {
    path: String,
    record: Arc<Mutex<Record>>,
}

#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl MockSession {
    async fn close(&self) {
        self.record.lock().unwrap().closed_sessions.push(self.path.clone());
    }
}

/// Returns (client connection, mock portal's connection, its record).
async fn start_mock(start: StartBehavior, reject_persistence: bool, with_remote_desktop: bool) -> (Connection, Connection, Shared) {
    let shared = Shared { start, reject_persistence, record: Default::default() };
    let (client_socket, server_socket) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut server = zbus::connection::Builder::unix_stream(server_socket).server(zbus::Guid::generate()).unwrap().p2p();
    if with_remote_desktop {
        server = server.serve_at(PORTAL_PATH, MockRemoteDesktop(shared.clone())).unwrap();
    }
    server = server.serve_at(PORTAL_PATH, MockScreenCast(shared.clone())).unwrap();
    let client = zbus::connection::Builder::unix_stream(client_socket).p2p();
    let (server, client) = futures_util::try_join!(server.build(), client.build()).unwrap();
    // Stands in for the unique name a real bus would assign (the builder's
    // `unique_name` only applies to the server end of a p2p connection).
    client.set_unique_name(CLIENT_NAME).unwrap();
    (client, server, shared)
}

fn methods(shared: &Shared) -> Vec<String> {
    shared.record.lock().unwrap().calls.iter().map(|(m, _)| m.clone()).collect()
}

#[tokio::test]
async fn negotiation_completes_with_handle_tokens_on_every_request() {
    let (client, _server, shared) = start_mock(StartBehavior::Approve, false, true).await;
    let (_stop_tx, stop) = watch::channel(false);
    let negotiated = negotiate_on(client, Some("earlier-token"), &stop, Duration::from_secs(5)).await.expect("negotiates");
    assert_eq!(negotiated.session.node_id, NODE_ID);
    assert_eq!(negotiated.session.logical_size, Some((1440, 900)));
    assert_eq!(negotiated.restore_token.as_deref(), Some("next-token"));
    assert!(negotiated.persistence_requested);

    assert_eq!(methods(&shared), ["CreateSession", "SelectDevices", "SelectSources", "Start"]);
    let record = shared.record.lock().unwrap();
    for (method, options) in &record.calls {
        assert!(options.get("handle_token").is_some_and(|t| t.starts_with("continuity_")), "{method} had no handle_token: {options:?}");
    }
    let devices = &record.calls[1].1;
    assert_eq!(devices["types"], "3");
    assert_eq!(devices["persist_mode"], "2");
    assert_eq!(devices["restore_token"], "earlier-token");
    let sources = &record.calls[2].1;
    assert_eq!(sources["types"], "1");
    assert_eq!(sources["cursor_mode"], "2");
    assert!(record.closed_sessions.is_empty() && record.closed_requests.is_empty());
}

#[tokio::test]
async fn input_reaches_the_portal_with_the_session_and_stream() {
    let (client, _server, shared) = start_mock(StartBehavior::Approve, false, true).await;
    let (_stop_tx, stop) = watch::channel(false);
    let session = negotiate_on(client, None, &stop, Duration::from_secs(5)).await.expect("negotiates").session;
    session.notify_pointer_motion_absolute(720.0, 450.0).await.unwrap();
    session.notify_pointer_button(0x110, true).await.unwrap();
    session.notify_pointer_button(0x110, false).await.unwrap();
    session.notify_keyboard_keycode(30, true).await.unwrap();
    session.notify_pointer_axis(0.0, -24.0).await.unwrap();
    assert_eq!(
        shared.record.lock().unwrap().input,
        ["motion 57 720 450", "button 272 1", "button 272 0", "key 30 1", "axis 0 -24 finish=true"]
    );
    session.close().await;
    assert_eq!(shared.record.lock().unwrap().closed_sessions.len(), 1);
}

#[tokio::test]
async fn a_declined_dialog_is_reported_as_declined_and_the_session_closed() {
    let (client, _server, shared) = start_mock(StartBehavior::Decline, false, true).await;
    let (_stop_tx, stop) = watch::channel(false);
    let error = negotiate_on(client, None, &stop, Duration::from_secs(5)).await.err().expect("declined");
    assert_eq!(error.kind, ErrorKind::Declined);
    assert_eq!(error.reason, "screen sharing was declined on this computer");
    assert_eq!(shared.record.lock().unwrap().closed_sessions.len(), 1);
}

#[tokio::test]
async fn an_unanswered_dialog_times_out_and_is_taken_down() {
    let (client, _server, shared) = start_mock(StartBehavior::Never, false, true).await;
    let (_stop_tx, stop) = watch::channel(false);
    let error = negotiate_on(client, None, &stop, Duration::from_secs(1)).await.err().expect("times out");
    assert_eq!(error.kind, ErrorKind::TimedOut);
    assert_eq!(error.reason, "nobody approved screen sharing on this computer within 1 seconds");
    let record = shared.record.lock().unwrap();
    assert_eq!(record.closed_requests.len(), 1, "the Start request (the dialog) should be closed");
    assert!(record.closed_requests[0].contains("continuity_start_"));
    assert_eq!(record.closed_sessions.len(), 1);
}

#[tokio::test]
async fn ending_the_session_during_the_dialog_takes_it_down() {
    let (client, _server, shared) = start_mock(StartBehavior::Never, false, true).await;
    let (stop_tx, stop) = watch::channel(false);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = stop_tx.send(true);
        tokio::time::sleep(Duration::from_secs(10)).await; // keep the sender alive
    });
    let error = negotiate_on(client, None, &stop, Duration::from_secs(30)).await.err().expect("cancelled");
    assert_eq!(error.kind, ErrorKind::Cancelled);
    let record = shared.record.lock().unwrap();
    assert_eq!(record.closed_requests.len(), 1);
    assert_eq!(record.closed_sessions.len(), 1);
}

#[tokio::test]
async fn a_desktop_without_a_remote_desktop_portal_says_so() {
    let (client, _server, shared) = start_mock(StartBehavior::Approve, false, false).await;
    let (_stop_tx, stop) = watch::channel(false);
    let error = negotiate_on(client, None, &stop, Duration::from_secs(5)).await.err().expect("unavailable");
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert!(error.reason.contains("GNOME or KDE Plasma"), "{}", error.reason);
    assert!(methods(&shared).is_empty());
}

#[tokio::test]
async fn a_portal_that_refuses_persistence_still_gets_a_session() {
    let (client, _server, shared) = start_mock(StartBehavior::Approve, true, true).await;
    let (_stop_tx, stop) = watch::channel(false);
    let negotiated = negotiate_on(client, Some("earlier-token"), &stop, Duration::from_secs(5)).await.expect("negotiates");
    assert!(!negotiated.persistence_requested);
    assert_eq!(negotiated.restore_token, None);
    assert_eq!(methods(&shared), ["CreateSession", "SelectDevices", "CreateSession", "SelectDevices", "SelectSources", "Start"]);
    let record = shared.record.lock().unwrap();
    assert!(!record.calls[3].1.contains_key("persist_mode"));
    assert_eq!(record.closed_sessions.len(), 1, "the refused session is closed");
}

#[tokio::test]
async fn the_portal_closing_the_session_is_noticed() {
    let (client, server, shared) = start_mock(StartBehavior::Approve, false, true).await;
    let (_stop_tx, stop) = watch::channel(false);
    let session = negotiate_on(client, None, &stop, Duration::from_secs(5)).await.expect("negotiates").session;
    let path = shared.record.lock().unwrap().sessions.iter().next().unwrap().clone();
    let watcher = tokio::spawn(async move { session.closed().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    server
        .emit_signal(None::<&str>, path.as_str(), "org.freedesktop.portal.Session", "Closed", &(HashMap::<&str, Value>::new(),))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), watcher).await.expect("noticed the close").unwrap();
}

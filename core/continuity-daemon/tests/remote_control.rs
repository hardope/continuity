//! Integration test for the remote-control session lifecycle: request →
//! consent → accepted session on both sides → input relay → screen frame
//! delivery → clean end. Uses a `FakeRemoteControlHost` test double
//! (real screen capture and input injection need real OS permissions
//! this environment can't grant non-interactively — see
//! `core/continuityd/src/remote_control_mac.rs`'s own manual verification
//! for what *is* confirmed against real macOS APIs) so this test instead
//! covers what's actually testable everywhere: the engine's own session
//! bookkeeping, consent gating, and message routing, which is where the
//! real complexity (and the dedicated screen-stream connection — see
//! `handle_screen_stream_connection` in `engine.rs`) lives.
//!
//! Needs real loopback multicast (mDNS) to connect the two peers, so
//! it's a genuine integration test, not a unit test.

use continuity_crypto::{Identity, TrustStore, TrustedDevice};
use continuity_daemon::{
    ClipboardBackend, EngineCommand, EngineConfig, EngineHandle, MediaController, NoopMediaController, RemoteControlHost,
    RemoteControlRole, SyncEvent,
};
use continuity_proto::InputEventKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

struct NoopClipboard;
impl ClipboardBackend for NoopClipboard {
    fn get_text(&self) -> Option<String> {
        None
    }
    fn set_text(&self, _text: &str) {}
}

/// Records every injected event and, once `start_capture` is called,
/// pushes a handful of fake frames before going quiet (still "capturing"
/// until `stop_capture`, just nothing further to send) — enough to prove
/// frames actually flow over the dedicated screen-stream connection
/// without needing a real screen.
#[derive(Clone, Default)]
struct FakeRemoteControlHost {
    injected: Arc<Mutex<Vec<InputEventKind>>>,
    capturing: Arc<AtomicBool>,
}

impl RemoteControlHost for FakeRemoteControlHost {
    fn inject(&self, event: InputEventKind) {
        self.injected.lock().unwrap().push(event);
    }

    fn start_capture(&self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.capturing.store(true, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(4);
        let capturing = self.capturing.clone();
        tokio::spawn(async move {
            let mut seq = 0u8;
            while capturing.load(Ordering::Relaxed) && seq < 5 {
                if tx.send(vec![seq; 16]).await.is_err() {
                    break;
                }
                seq += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        Some(rx)
    }

    fn stop_capture(&self) {
        self.capturing.store(false, Ordering::Relaxed);
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

async fn make_engine(
    name: &str,
    identity: Identity,
    peer_id: &str,
    peer_name: &str,
    remote_control: Arc<dyn RemoteControlHost>,
) -> anyhow::Result<EngineHandle> {
    let dir = std::env::temp_dir().join(format!("continuity-test-remote-control-{name}-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&dir)?;
    let mut trust_store = TrustStore::load(dir.join("trust.json"))?;
    trust_store.trust(TrustedDevice { id: peer_id.to_string(), name: peer_name.to_string(), paired_at_unix: now_unix() })?;
    let config = EngineConfig {
        identity,
        device_name: name.to_string(),
        trust_store,
        clipboard: Arc::new(NoopClipboard),
        media: Arc::new(NoopMediaController) as Arc<dyn MediaController>,
        remote_control,
        received_files_dir: dir,
    };
    continuity_daemon::start(config).await
}

#[tokio::test]
async fn full_remote_control_session_lifecycle() {
    let a_identity = Identity::generate();
    let b_identity = Identity::generate();
    let a_id = a_identity.device_id();
    let b_id = b_identity.device_id();

    let b_host = FakeRemoteControlHost::default();
    // A never gets controlled in this test — a plain Noop is enough for
    // its side, only B's capability matters here.
    let mut a = make_engine("RemoteA", a_identity, &b_id, "RemoteB", Arc::new(continuity_daemon::NoopRemoteControlHost))
        .await
        .expect("start engine A");
    let mut b = make_engine("RemoteB", b_identity, &a_id, "RemoteA", Arc::new(b_host.clone())).await.expect("start engine B");

    // Wait for natural mDNS auto-dial to connect them first.
    let connect_deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(connect_deadline);
    loop {
        tokio::select! {
            _ = &mut connect_deadline => panic!("A and B never connected"),
            Some(ev) = a.events.recv() => if matches!(ev, SyncEvent::Connected { .. }) { break },
            Some(_) = b.events.recv() => {}
        }
    }

    a.command_sender().send(EngineCommand::RequestRemoteControl { peer_crypto_id: b_id.clone() }).expect("send request");

    let mut b_saw_request = false;
    let mut a_saw_started_controlling = false;
    let mut b_saw_started_controlled = false;

    // Its own generous deadline, separate from the frame-delivery wait
    // below — under real system load (this whole workspace's test suite
    // spins up several concurrent mDNS-advertising engine pairs), request/
    // accept round-tripping over the *existing* mesh connection and
    // dialing the brand new dedicated screen-stream connection are two
    // genuinely different-latency operations; sharing one budget between
    // them meant a slow-but-fine session establishment could starve the
    // frame check of the time it still legitimately needed.
    let session_deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(session_deadline);
    loop {
        if b_saw_request && a_saw_started_controlling && b_saw_started_controlled {
            break;
        }
        tokio::select! {
            _ = &mut session_deadline => break,
            Some(ev) = a.events.recv() => match ev {
                SyncEvent::RemoteControlSessionStarted { peer_id, role, .. } => {
                    assert_eq!(peer_id, b_id);
                    assert_eq!(role, RemoteControlRole::Controlling);
                    a_saw_started_controlling = true;
                }
                SyncEvent::RemoteControlDeclined { .. } => panic!("B should accept, not decline"),
                _ => {}
            },
            Some(ev) = b.events.recv() => match ev {
                SyncEvent::RemoteControlRequested { peer_id, session_id, .. } => {
                    assert_eq!(peer_id, a_id);
                    b_saw_request = true;
                    b.command_sender()
                        .send(EngineCommand::RespondToRemoteControlRequest { peer_crypto_id: a_id.clone(), accept: true })
                        .expect("send accept");
                    let _ = session_id; // engine tracks it internally; nothing else to do with it here
                }
                SyncEvent::RemoteControlSessionStarted { peer_id, role, .. } => {
                    assert_eq!(peer_id, a_id);
                    assert_eq!(role, RemoteControlRole::Controlled);
                    b_saw_started_controlled = true;
                }
                _ => {}
            }
        }
    }
    assert!(b_saw_request, "B should have seen the incoming request");
    assert!(a_saw_started_controlling, "A should see its session start as Controlling");
    assert!(b_saw_started_controlled, "B should see its session start as Controlled");

    // Fresh deadline for the screen stream specifically — this is the
    // *separate* dedicated connection (see `handle_screen_stream_connection`
    // in engine.rs), dialed only after the session above was already
    // confirmed active, so it fairly gets its own full window to connect
    // and deliver a first frame rather than whatever happened to be left
    // over from the session-establishment budget.
    let mut a_saw_frame = false;
    let frame_deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(frame_deadline);
    while !a_saw_frame {
        tokio::select! {
            _ = &mut frame_deadline => break,
            Some(ev) = a.events.recv() => {
                if let SyncEvent::ScreenFrameReceived { peer_id, frame, .. } = ev {
                    assert_eq!(peer_id, b_id);
                    assert!(!frame.is_empty(), "frame should carry real bytes");
                    a_saw_frame = true;
                }
            }
            Some(_) = b.events.recv() => {}
        }
    }
    assert!(a_saw_frame, "A should receive at least one screen frame from B's fake capture");

    // Input relay: A sends a mouse move, B's fake host should record it.
    a.command_sender()
        .send(EngineCommand::SendInputEvent { peer_crypto_id: b_id.clone(), event: InputEventKind::MouseMove { x: 0.5, y: 0.25 } })
        .expect("send input event");

    let injected_deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(injected_deadline);
    loop {
        if !b_host.injected.lock().unwrap().is_empty() {
            break;
        }
        tokio::select! {
            _ = &mut injected_deadline => break,
            Some(_) = a.events.recv() => {}
            Some(_) = b.events.recv() => {}
        }
    }
    let injected = b_host.injected.lock().unwrap().clone();
    assert_eq!(injected.len(), 1, "B's host should have received exactly the one input event A sent");
    assert!(matches!(injected[0], InputEventKind::MouseMove { x, y } if x == 0.5 && y == 0.25));

    // Ending the session should stop capture on B's side and tell both
    // shells it's over.
    a.command_sender().send(EngineCommand::EndRemoteControlSession { peer_crypto_id: b_id.clone() }).expect("send end");

    let mut a_saw_ended = false;
    let mut b_saw_ended = false;
    let end_deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(end_deadline);
    loop {
        if a_saw_ended && b_saw_ended {
            break;
        }
        tokio::select! {
            _ = &mut end_deadline => break,
            Some(ev) = a.events.recv() => if matches!(ev, SyncEvent::RemoteControlSessionEnded { .. }) { a_saw_ended = true },
            Some(ev) = b.events.recv() => if matches!(ev, SyncEvent::RemoteControlSessionEnded { .. }) { b_saw_ended = true },
        }
    }
    assert!(a_saw_ended, "A should see the session end");
    assert!(b_saw_ended, "B should see the session end");
    assert!(!b_host.capturing.load(Ordering::Relaxed), "B's capture should have been stopped");

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn a_host_that_reports_unavailable_auto_declines_without_bothering_the_user() {
    let a_identity = Identity::generate();
    let b_identity = Identity::generate();
    let a_id = a_identity.device_id();
    let b_id = b_identity.device_id();

    // The default Noop host reports `is_available() == false` — matches
    // Android/iOS/Linux and a "lite" desktop build.
    let mut a = make_engine("UnavailA", a_identity, &b_id, "UnavailB", Arc::new(continuity_daemon::NoopRemoteControlHost))
        .await
        .expect("start engine A");
    let mut b = make_engine("UnavailB", b_identity, &a_id, "UnavailA", Arc::new(continuity_daemon::NoopRemoteControlHost))
        .await
        .expect("start engine B");

    let connect_deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(connect_deadline);
    loop {
        tokio::select! {
            _ = &mut connect_deadline => panic!("A and B never connected"),
            Some(ev) = a.events.recv() => if matches!(ev, SyncEvent::Connected { .. }) { break },
            Some(_) = b.events.recv() => {}
        }
    }

    a.command_sender().send(EngineCommand::RequestRemoteControl { peer_crypto_id: b_id.clone() }).expect("send request");

    let mut a_saw_declined = false;
    let mut b_saw_request = false;
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline);
    loop {
        if a_saw_declined {
            break;
        }
        tokio::select! {
            _ = &mut deadline => break,
            Some(ev) = a.events.recv() => if matches!(ev, SyncEvent::RemoteControlDeclined { .. }) { a_saw_declined = true },
            Some(ev) = b.events.recv() => if matches!(ev, SyncEvent::RemoteControlRequested { .. }) { b_saw_request = true },
        }
    }
    assert!(a_saw_declined, "A should see the request auto-declined");
    assert!(!b_saw_request, "B's shell should never even be asked — nothing it could grant");

    a.shutdown();
    b.shutdown();
}

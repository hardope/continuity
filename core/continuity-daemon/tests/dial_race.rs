//! Regression test for a real race condition found and fixed in
//! `engine.rs`: mDNS delivers `ServiceResolved` events in rapid bursts —
//! one per resolved address/interface for the same peer, commonly a
//! dozen-plus within single-digit milliseconds. The only guard against
//! redundant dials used to be `!connected.contains(peer_id)`, which only
//! becomes true *after* a full TLS handshake completes — a window wide
//! enough for several of those events to each independently see "not
//! connected yet" and start their own simultaneous dial to the same peer.
//! Each side then resolved which attempt "won" completely independently,
//! with no correlation between the two, so a mismatched pair of
//! half-abandoned connections was possible — read as an inexplicable
//! disconnect with no relation to file transfers or the keepalive
//! timeout.
//!
//! Fixed with an atomic "dialing" claim (see `try_claim_dial` in
//! `engine.rs`). This test connects two pre-trusted peers via completely
//! natural mDNS auto-dial — the exact path the race used to trigger on —
//! and asserts exactly one clean connection on each side with no
//! disconnect in a generous observation window.
//!
//! Needs real loopback multicast (mDNS), so it's a genuine integration
//! test, not a unit test — slower than most (waits out a real observation
//! window) but still bounded and non-interactive.

use continuity_crypto::{Identity, TrustStore, TrustedDevice};
use continuity_daemon::{ClipboardBackend, EngineConfig, EngineHandle, MediaController, NoopMediaController, SyncEvent};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct NoopClipboard;
impl ClipboardBackend for NoopClipboard {
    fn get_text(&self) -> Option<String> {
        None
    }
    fn set_text(&self, _text: &str) {}
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

async fn make_engine(name: &str, identity: Identity, peer_id: &str, peer_name: &str) -> anyhow::Result<EngineHandle> {
    let dir = std::env::temp_dir().join(format!("continuity-test-dial-race-{name}-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&dir)?;
    let mut trust_store = TrustStore::load(dir.join("trust.json"))?;
    trust_store.trust(TrustedDevice { id: peer_id.to_string(), name: peer_name.to_string(), paired_at_unix: now_unix() })?;
    let config = EngineConfig {
        identity,
        device_name: name.to_string(),
        trust_store,
        clipboard: Arc::new(NoopClipboard),
        media: Arc::new(NoopMediaController) as Arc<dyn MediaController>,
        remote_control: Arc::new(continuity_daemon::NoopRemoteControlHost),
        screen_lock: Arc::new(continuity_daemon::NoopScreenLockController),
        received_files_dir: dir,
    };
    continuity_daemon::start(config).await
}

#[tokio::test]
async fn natural_mdns_auto_dial_produces_exactly_one_connection_per_side() {
    let a_identity = Identity::generate();
    let b_identity = Identity::generate();
    let a_id = a_identity.device_id();
    let b_id = b_identity.device_id();

    let mut a = make_engine("DialRaceA", a_identity, &b_id, "DialRaceB").await.expect("start engine A");
    let mut b = make_engine("DialRaceB", b_identity, &a_id, "DialRaceA").await.expect("start engine B");

    let mut a_connects = 0u32;
    let mut b_connects = 0u32;
    let mut a_disconnects = 0u32;
    let mut b_disconnects = 0u32;
    let mut a_settled_at: Option<Instant> = None;
    let mut b_settled_at: Option<Instant> = None;

    let deadline = tokio::time::sleep(Duration::from_secs(25));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => break,
            Some(ev) = a.events.recv() => {
                match ev {
                    SyncEvent::Connected { .. } => {
                        a_connects += 1;
                        a_settled_at = Some(Instant::now());
                    }
                    SyncEvent::Disconnected { .. } => a_disconnects += 1,
                    _ => {}
                }
            }
            Some(ev) = b.events.recv() => {
                match ev {
                    SyncEvent::Connected { .. } => {
                        b_connects += 1;
                        b_settled_at = Some(Instant::now());
                    }
                    SyncEvent::Disconnected { .. } => b_disconnects += 1,
                    _ => {}
                }
            }
        }

        // Both sides connected and stayed quiet for 8s afterward — long
        // enough that a race-induced mismatched connection (which fails
        // fast, not on any timeout) would already have shown up.
        if let (Some(a_at), Some(b_at)) = (a_settled_at, b_settled_at) {
            if a_at.elapsed() > Duration::from_secs(8) && b_at.elapsed() > Duration::from_secs(8) {
                break;
            }
        }
    }

    a.shutdown();
    b.shutdown();

    assert_eq!(a_connects, 1, "A should connect to B exactly once, not race into redundant dials");
    assert_eq!(b_connects, 1, "B should connect to A exactly once, not race into redundant dials");
    assert_eq!(a_disconnects, 0, "A should see no disconnect — a race would show up as an immediate spurious one");
    assert_eq!(b_disconnects, 0, "B should see no disconnect — a race would show up as an immediate spurious one");
}

/// A connection that opens and never starts its TLS handshake — anyone on
/// the network can make one, no pairing needed — doesn't stop the engine
/// accepting others: a paired device connecting after it still gets
/// through straight away. Driven by hand over loopback.
#[tokio::test]
async fn an_idle_connection_does_not_hold_up_the_listener() {
    use continuity_proto::{DeviceInfo, Platform};

    let peer_identity = Identity::generate();
    let peer_id = peer_identity.device_id();
    let mut engine = make_engine("IdleListener", Identity::generate(), &peer_id, "HandPeer").await.expect("start engine");
    let port = loop {
        match tokio::time::timeout(Duration::from_secs(5), engine.events.recv()).await {
            Ok(Some(SyncEvent::Listening { port })) => break port,
            Ok(Some(_)) => continue,
            other => panic!("no Listening event: {other:?}"),
        }
    };
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    // Opens and says nothing, for the rest of the test.
    let _idle = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let tls = continuity_crypto::generate_self_signed(&peer_identity).unwrap();
    let device = DeviceInfo { id: peer_id.clone(), name: "HandPeer".into(), platform: Platform::Linux, protocol_version: 3 };
    let connected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut conn = continuity_net::connect(addr, &tls).await?;
        continuity_net::announce_and_identify(&mut conn, &device).await?;
        anyhow::Ok(conn)
    })
    .await;
    assert!(matches!(connected, Ok(Ok(_))), "a paired device couldn't connect behind an idle connection: {:?}", connected.map(|r| r.map(|_| ())));

    engine.shutdown();
}

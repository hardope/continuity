//! Regression test for `EngineCommand::RevokeDevice`: revoking a peer must
//! notify it (`Message::Revoked`) and close the connection immediately,
//! not just remove it from the local trust store and leave the connection
//! sitting open until it happens to drop on its own — that was the
//! documented gap this closes (see "Known gaps" in `docs/protocol.md`
//! before this fix).
//!
//! Connects two pre-trusted peers via natural mDNS auto-dial, revokes one
//! direction, and asserts: the revoking side gets exactly one `WasRevoked`
//! for that peer; the revoked side gets `RevokedByPeer` followed by
//! `Disconnected` for the same peer. Revocation is deliberately
//! one-directional (mirrors SSH removing a key from `authorized_keys`
//! without touching the client's `known_hosts`) — this test doesn't assert
//! anything about the revoked side's own trust store, since nothing in
//! this engine ever touches it.
//!
//! Needs real loopback multicast (mDNS) to connect the two peers in the
//! first place, so it's a genuine integration test, not a unit test.

use continuity_crypto::{Identity, TrustStore, TrustedDevice};
use continuity_daemon::{ClipboardBackend, EngineCommand, EngineConfig, EngineHandle, MediaController, NoopMediaController, SyncEvent};
use std::sync::Arc;
use std::time::Duration;

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
    let dir = std::env::temp_dir().join(format!("continuity-test-revoke-{name}-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&dir)?;
    let mut trust_store = TrustStore::load(dir.join("trust.json"))?;
    trust_store.trust(TrustedDevice { id: peer_id.to_string(), name: peer_name.to_string(), paired_at_unix: now_unix() })?;
    let config = EngineConfig {
        identity,
        device_name: name.to_string(),
        trust_store,
        clipboard: Arc::new(NoopClipboard),
        media: Arc::new(NoopMediaController) as Arc<dyn MediaController>,
        received_files_dir: dir,
    };
    continuity_daemon::start(config).await
}

#[tokio::test]
async fn revoking_a_connected_peer_notifies_it_and_closes_the_connection() {
    let a_identity = Identity::generate();
    let b_identity = Identity::generate();
    let a_id = a_identity.device_id();
    let b_id = b_identity.device_id();

    let mut a = make_engine("RevokeA", a_identity, &b_id, "RevokeB").await.expect("start engine A");
    let mut b = make_engine("RevokeB", b_identity, &a_id, "RevokeA").await.expect("start engine B");

    // Wait for the natural mDNS auto-dial to connect them, same as
    // dial_race.rs — nothing to revoke until they're actually paired up.
    let connect_deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(connect_deadline);
    loop {
        tokio::select! {
            _ = &mut connect_deadline => panic!("A and B never connected"),
            Some(ev) = a.events.recv() => {
                if matches!(ev, SyncEvent::Connected { .. }) {
                    break;
                }
            }
            Some(_) = b.events.recv() => {}
        }
    }

    a.command_sender().send(EngineCommand::RevokeDevice { peer_crypto_id: b_id.clone() }).expect("send revoke command");

    let mut a_saw_was_revoked = 0u32;
    let mut b_saw_revoked_by_peer = 0u32;
    let mut b_saw_disconnected = 0u32;

    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    loop {
        if a_saw_was_revoked > 0 && b_saw_revoked_by_peer > 0 && b_saw_disconnected > 0 {
            break;
        }
        tokio::select! {
            _ = &mut deadline => break,
            Some(ev) = a.events.recv() => {
                if let SyncEvent::WasRevoked { peer_id, .. } = ev {
                    assert_eq!(peer_id, b_id, "A should report revoking B, not some other peer");
                    a_saw_was_revoked += 1;
                }
            }
            Some(ev) = b.events.recv() => {
                match ev {
                    SyncEvent::RevokedByPeer { peer_id, .. } => {
                        assert_eq!(peer_id, a_id, "B should be told A revoked it, not some other peer");
                        b_saw_revoked_by_peer += 1;
                    }
                    SyncEvent::Disconnected { peer_id, .. } => {
                        assert_eq!(peer_id, a_id);
                        b_saw_disconnected += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    a.shutdown();
    b.shutdown();

    assert_eq!(a_saw_was_revoked, 1, "A should see exactly one WasRevoked for B");
    assert_eq!(b_saw_revoked_by_peer, 1, "B should see exactly one RevokedByPeer from A");
    assert_eq!(b_saw_disconnected, 1, "B should see its connection to A close as a result");
}

//! Who a dialed connection is with, and what a forgotten device does.
//!
//! **A forged address isn't trusted.** An mDNS record is unauthenticated:
//! anyone on the network can advertise a paired device's id at their own
//! address. The engine used to decide trust from that id before the
//! handshake and never compared the certificate the other end presented
//! with the id it meant to dial, so the device at the forged address was
//! connected to as the paired one — every clipboard copy went to it, its
//! files were accepted, it could lock the screen. Here engine A trusts an
//! id nobody is running, its address is planted as a third engine's, and A
//! is told to connect: it must not end up connected to anyone, and the
//! third engine must not see a pairing request either.
//!
//! **A forgotten device stops dialing.** After A forgets B while they're
//! connected, B keeps A's address and used to redial it every
//! `RECONNECT_RETRY_INTERVAL`, putting a pairing prompt up on A each time
//! (see revoke.rs for the revocation itself). Here B is made to redial
//! straight away and A must see no pairing request.
//!
//! Both connect real engines over loopback mDNS, so these are integration
//! tests like the rest of this directory.

use continuity_crypto::{Identity, TrustStore, TrustedDevice};
use continuity_daemon::{ClipboardBackend, EngineCommand, EngineConfig, EngineHandle, MediaController, NoopMediaController, SyncEvent};
use std::net::SocketAddr;
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

async fn make_engine(name: &str, identity: Identity, trusted: &[(&str, &str)]) -> anyhow::Result<EngineHandle> {
    let dir = std::env::temp_dir().join(format!("continuity-test-trust-{name}-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&dir)?;
    let mut trust_store = TrustStore::load(dir.join("trust.json"))?;
    for (peer_id, peer_name) in trusted {
        trust_store.trust(TrustedDevice { id: peer_id.to_string(), name: peer_name.to_string(), paired_at_unix: now_unix() })?;
    }
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

/// The port an engine's listener took, from its first event.
async fn listening_port(engine: &mut EngineHandle) -> u16 {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), engine.events.recv()).await {
            Ok(Some(SyncEvent::Listening { port })) => return port,
            Ok(Some(_)) => {}
            _ => panic!("engine never reported its port"),
        }
    }
}

#[tokio::test]
async fn a_device_at_a_forged_address_is_not_taken_for_the_paired_one() {
    let a_identity = Identity::generate();
    let b_id = Identity::generate().device_id(); // B itself never runs
    let impostor_identity = Identity::generate();
    let impostor_id = impostor_identity.device_id();

    let mut a = make_engine("TrustA", a_identity, &[(&b_id, "TrustB")]).await.expect("start engine A");
    let mut impostor = make_engine("TrustImpostor", impostor_identity, &[]).await.expect("start the impostor");
    let impostor_port = listening_port(&mut impostor).await;

    // What a forged mDNS record would leave behind: B's id at the impostor's address.
    a.set_known_address_for_test(&b_id, SocketAddr::from(([127, 0, 0, 1], impostor_port)));
    a.send_command(EngineCommand::ReconnectPeer { peer_crypto_id: b_id.clone() });

    let deadline = tokio::time::sleep(Duration::from_secs(8));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            Some(ev) = a.events.recv() => match ev {
                SyncEvent::Connected { peer } => panic!("A connected to '{}' ({}) as if it were B", peer.name, peer.id),
                SyncEvent::PairingRequested { peer, .. } => panic!("A was asked to pair with {}", peer.id),
                _ => {}
            },
            Some(ev) = impostor.events.recv() => match ev {
                SyncEvent::Connected { peer } => panic!("the impostor got a connection from '{}'", peer.name),
                SyncEvent::PairingRequested { peer, .. } => panic!("the impostor was offered pairing by {}", peer.id),
                _ => {}
            },
        }
    }
    let _ = impostor_id;
    a.shutdown();
    impostor.shutdown();
}

#[tokio::test]
async fn a_forgotten_device_does_not_keep_dialing_the_one_that_forgot_it() {
    let a_identity = Identity::generate();
    let b_identity = Identity::generate();
    let a_id = a_identity.device_id();
    let b_id = b_identity.device_id();

    let mut a = make_engine("ForgetA", a_identity, &[(&b_id, "ForgetB")]).await.expect("start engine A");
    let mut b = make_engine("ForgetB", b_identity, &[(&a_id, "ForgetA")]).await.expect("start engine B");

    let connect_deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(connect_deadline);
    loop {
        tokio::select! {
            _ = &mut connect_deadline => panic!("A and B never connected"),
            Some(ev) = a.events.recv() => if matches!(ev, SyncEvent::Connected { .. }) { break },
            Some(_) = b.events.recv() => {}
        }
    }

    a.send_command(EngineCommand::RevokeDevice { peer_crypto_id: b_id.clone() });

    // B hears it was forgotten and sees the connection close.
    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);
    let mut b_disconnected = false;
    loop {
        if b_disconnected {
            break;
        }
        tokio::select! {
            _ = &mut deadline => panic!("B never saw the connection close"),
            Some(_) = a.events.recv() => {}
            Some(ev) = b.events.recv() => if matches!(ev, SyncEvent::Disconnected { .. }) { b_disconnected = true },
        }
    }

    // What the 45-second ticker would do, right now: redial every paired
    // device that isn't connected. B still has A's address.
    b.send_command(EngineCommand::RefreshDiscovery);

    let deadline = tokio::time::sleep(Duration::from_secs(6));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            Some(ev) = a.events.recv() => {
                if let SyncEvent::PairingRequested { peer, .. } = ev {
                    panic!("A was asked to pair again with '{}' though nobody asked for that", peer.name);
                }
            }
            Some(ev) = b.events.recv() => {
                if let SyncEvent::Connected { peer } = ev {
                    panic!("B reconnected to '{}' after being forgotten", peer.name);
                }
            }
        }
    }

    a.shutdown();
    b.shutdown();
}

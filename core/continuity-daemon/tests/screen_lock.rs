//! Integration test for remote screen lock/unlock and targeted text
//! sharing: unlock is refused until the receiving device explicitly allows
//! the requesting peer, lock needs no such grant, a device with no real
//! `ScreenLockController` answers `Unsupported`, a controller failure is
//! reported back with its reason, turning the permission back off takes
//! effect immediately, and `SendText` lands on exactly one peer's
//! clipboard. Uses a `FakeScreenLock` test double — the real Linux
//! implementation talks to systemd-logind, which needs a real graphical
//! session — so this covers the engine's own consent gating and message
//! routing, which is where the security-relevant logic lives.
//!
//! Needs real loopback multicast (mDNS) to connect the two peers, so
//! it's a genuine integration test, not a unit test.

use continuity_crypto::{Identity, TrustStore, TrustedDevice};
use continuity_daemon::{
    ClipboardBackend, EngineCommand, EngineConfig, EngineHandle, MediaController, NoopMediaController,
    NoopScreenLockController, ScreenLockController, ScreenLockError, SyncEvent,
};
use continuity_proto::{ScreenLockAction, ScreenLockOutcome};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Default)]
struct RecordingClipboard {
    written: Arc<Mutex<Vec<String>>>,
}

impl ClipboardBackend for RecordingClipboard {
    /// Always empty, so this side's own clipboard watcher never has
    /// anything to broadcast and can't muddy what the test observes.
    fn get_text(&self) -> Option<String> {
        None
    }

    fn set_text(&self, text: &str) {
        self.written.lock().unwrap().push(text.to_string());
    }
}

#[derive(Clone, Default)]
struct FakeScreenLock {
    locks: Arc<AtomicUsize>,
    unlocks: Arc<AtomicUsize>,
    fail_next: Arc<AtomicBool>,
}

impl ScreenLockController for FakeScreenLock {
    fn lock(&self) -> Result<(), ScreenLockError> {
        if self.fail_next.swap(false, Ordering::Relaxed) {
            return Err(ScreenLockError::Failed("the lock screen didn't respond".to_string()));
        }
        self.locks.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn unlock(&self) -> Result<(), ScreenLockError> {
        self.unlocks.fetch_add(1, Ordering::Relaxed);
        Ok(())
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
    screen_lock: Arc<dyn ScreenLockController>,
    clipboard: Arc<dyn ClipboardBackend>,
) -> anyhow::Result<EngineHandle> {
    let dir = std::env::temp_dir().join(format!("continuity-test-screen-lock-{name}-{}", rand::random::<u32>()));
    std::fs::create_dir_all(&dir)?;
    let mut trust_store = TrustStore::load(dir.join("trust.json"))?;
    trust_store.trust(TrustedDevice { id: peer_id.to_string(), name: peer_name.to_string(), paired_at_unix: now_unix() })?;
    let config = EngineConfig {
        identity,
        device_name: name.to_string(),
        trust_store,
        clipboard,
        media: Arc::new(NoopMediaController) as Arc<dyn MediaController>,
        remote_control: Arc::new(continuity_daemon::NoopRemoteControlHost),
        screen_lock,
        received_files_dir: dir,
    };
    continuity_daemon::start(config).await
}

/// Waits for the first event matching `pred` on one engine, discarding
/// everything before it. The other engine's events just queue up in its
/// unbounded channel in the meantime, which is harmless.
async fn wait_for(events: &mut mpsc::UnboundedReceiver<SyncEvent>, what: &str, mut pred: impl FnMut(&SyncEvent) -> bool) -> SyncEvent {
    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => panic!("timed out waiting for {what}"),
            ev = events.recv() => {
                let ev = ev.expect("engine event channel closed");
                if pred(&ev) {
                    return ev;
                }
            }
        }
    }
}

async fn request_and_await_result(
    requester: &mut EngineHandle,
    target_id: &str,
    action: ScreenLockAction,
) -> ScreenLockOutcome {
    requester
        .command_sender()
        .send(EngineCommand::RequestScreenLock { peer_crypto_id: target_id.to_string(), action })
        .expect("send request");
    match wait_for(&mut requester.events, "ScreenLockResult", |e| matches!(e, SyncEvent::ScreenLockResult { .. })).await {
        SyncEvent::ScreenLockResult { action: answered, outcome, .. } => {
            assert_eq!(answered, action, "result should be for the action that was requested");
            outcome
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn unlock_is_opt_in_and_lock_unlock_and_text_share_are_routed_correctly() {
    let a_identity = Identity::generate();
    let b_identity = Identity::generate();
    let a_id = a_identity.device_id();
    let b_id = b_identity.device_id();

    let b_lock = FakeScreenLock::default();
    let b_clipboard = RecordingClipboard::default();
    let mut a = make_engine("LockA", a_identity, &b_id, "LockB", Arc::new(NoopScreenLockController), Arc::new(RecordingClipboard::default()))
        .await
        .expect("start engine A");
    let mut b = make_engine("LockB", b_identity, &a_id, "LockA", Arc::new(b_lock.clone()), Arc::new(b_clipboard.clone()))
        .await
        .expect("start engine B");

    wait_for(&mut a.events, "A connecting to B", |e| matches!(e, SyncEvent::Connected { .. })).await;
    wait_for(&mut b.events, "B's initial unlock permission for A", |e| {
        matches!(e, SyncEvent::UnlockPermissionChanged { peer_id, allowed: false, .. } if *peer_id == a_id)
    })
    .await;

    // Not allowed yet: refused, and the controller is never even called.
    assert_eq!(request_and_await_result(&mut a, &b_id, ScreenLockAction::Unlock).await, ScreenLockOutcome::NotAllowed);
    assert_eq!(b_lock.unlocks.load(Ordering::Relaxed), 0);
    wait_for(&mut b.events, "B reporting the refused unlock", |e| {
        matches!(e, SyncEvent::ScreenLockRequested { action: ScreenLockAction::Unlock, outcome: ScreenLockOutcome::NotAllowed, .. })
    })
    .await;

    // Locking needs no grant.
    assert_eq!(request_and_await_result(&mut a, &b_id, ScreenLockAction::Lock).await, ScreenLockOutcome::Done);
    assert_eq!(b_lock.locks.load(Ordering::Relaxed), 1);

    b.command_sender().send(EngineCommand::SetUnlockAllowed { peer_crypto_id: a_id.clone(), allowed: true }).unwrap();
    wait_for(&mut b.events, "B allowing A to unlock", |e| matches!(e, SyncEvent::UnlockPermissionChanged { allowed: true, .. })).await;

    assert_eq!(request_and_await_result(&mut a, &b_id, ScreenLockAction::Unlock).await, ScreenLockOutcome::Done);
    assert_eq!(b_lock.unlocks.load(Ordering::Relaxed), 1);
    wait_for(&mut b.events, "B reporting the unlock", |e| {
        matches!(e, SyncEvent::ScreenLockRequested { action: ScreenLockAction::Unlock, outcome: ScreenLockOutcome::Done, .. })
    })
    .await;

    // A controller failure comes back with its reason.
    b_lock.fail_next.store(true, Ordering::Relaxed);
    assert_eq!(
        request_and_await_result(&mut a, &b_id, ScreenLockAction::Lock).await,
        ScreenLockOutcome::Failed { reason: "the lock screen didn't respond".to_string() }
    );

    // A has no real controller: B asking it gets a plain `Unsupported`.
    assert_eq!(request_and_await_result(&mut b, &a_id, ScreenLockAction::Lock).await, ScreenLockOutcome::Unsupported);

    // Turning the permission back off takes effect right away.
    b.command_sender().send(EngineCommand::SetUnlockAllowed { peer_crypto_id: a_id.clone(), allowed: false }).unwrap();
    wait_for(&mut b.events, "B revoking A's unlock permission", |e| matches!(e, SyncEvent::UnlockPermissionChanged { allowed: false, .. })).await;
    assert_eq!(request_and_await_result(&mut a, &b_id, ScreenLockAction::Unlock).await, ScreenLockOutcome::NotAllowed);
    assert_eq!(b_lock.unlocks.load(Ordering::Relaxed), 1, "a refused unlock must not reach the controller");

    a.command_sender().send(EngineCommand::SendText { peer_crypto_id: b_id.clone(), text: "https://example.com/shared".to_string() }).unwrap();
    wait_for(&mut b.events, "B receiving the shared text", |e| matches!(e, SyncEvent::ClipboardReceived { .. })).await;
    assert_eq!(b_clipboard.written.lock().unwrap().as_slice(), ["https://example.com/shared".to_string()]);

    a.shutdown();
    b.shutdown();
}

//! Integration tests for file transfer between two real engines over
//! loopback: the v3 data connection (raw frames, `.part` file renamed only
//! once verified), resuming after that connection is cut mid-file, and the
//! in-band `FileChunk` path still being used — and still working — with a
//! peer that announced an older protocol, a receiver that turns the data
//! connection down, and a genuinely old-format sender speaking raw v2 JSON.
//!
//! The switches that make an engine cut its data connection, refuse one, or
//! announce an older protocol are `EngineHandle::test_hooks` — nothing a
//! shell can reach.
//!
//! Needs real loopback multicast (mDNS) to connect the two engines, like
//! the other tests in this directory.

use continuity_crypto::{content_hash, Identity, TrustStore, TrustedDevice};
use continuity_daemon::{
    ClipboardBackend, EngineCommand, EngineConfig, EngineHandle, FileTransferDirection, MediaController,
    NoopMediaController, SyncEvent,
};
use continuity_proto::{DeviceInfo, Message, Platform};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MIB: usize = 1024 * 1024;

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

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("continuity-test-file-transfer-{label}-{:08x}", rand::random::<u32>()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Starts an engine that already trusts `peer_id`. Received files land in
/// their own directory, so a test can check exactly what's in it.
async fn make_engine(name: &str, identity: Identity, peer_id: &str, peer_name: &str) -> (EngineHandle, PathBuf) {
    let dir = temp_dir(name);
    let received = dir.join("received");
    let mut trust_store = TrustStore::load(dir.join("trust.json")).unwrap();
    trust_store.trust(TrustedDevice { id: peer_id.to_string(), name: peer_name.to_string(), paired_at_unix: now_unix() }).unwrap();
    let config = EngineConfig {
        identity,
        device_name: name.to_string(),
        trust_store,
        clipboard: Arc::new(NoopClipboard),
        media: Arc::new(NoopMediaController) as Arc<dyn MediaController>,
        remote_control: Arc::new(continuity_daemon::NoopRemoteControlHost),
        screen_lock: Arc::new(continuity_daemon::NoopScreenLockController),
        received_files_dir: received.clone(),
    };
    (continuity_daemon::start(config).await.unwrap(), received)
}

/// Deterministic, incompressible-looking test content (xorshift), so a
/// misplaced or repeated frame can't go unnoticed the way it could in a
/// file of zeros.
fn write_test_file(path: &Path, len: usize, seed: u64) {
    use std::io::Write;
    let mut state = seed | 1;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut block = vec![0u8; MIB];
    let mut left = len;
    while left > 0 {
        for word in block.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            word.copy_from_slice(&state.to_le_bytes()[..word.len()]);
        }
        let n = left.min(block.len());
        file.write_all(&block[..n]).unwrap();
        left -= n;
    }
    // On disk before the transfer starts, so the kernel isn't still
    // writing it back while a transfer is being timed.
    file.into_inner().unwrap().sync_all().unwrap();
}

fn file_hash(path: &Path) -> String {
    use std::io::Read;
    let mut file = std::fs::File::open(path).unwrap();
    let mut hasher = continuity_crypto::IncrementalHash::new();
    let mut buf = vec![0u8; MIB];
    loop {
        let n = file.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hasher.finalize_hex()
}

fn part_files_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".part"))
                .collect()
        })
        .unwrap_or_default()
}

/// Which engine an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    A,
    B,
}

/// Collects both engines' events until `done` says the log is complete, or
/// `limit` passes — tests assert on the log afterwards, so a timeout shows
/// up as a clear assertion failure with everything that did happen.
async fn collect_until(
    a: &mut EngineHandle,
    b: &mut EngineHandle,
    limit: Duration,
    mut done: impl FnMut(&[(Side, SyncEvent)]) -> bool,
) -> Vec<(Side, SyncEvent)> {
    let mut log = Vec::new();
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    while !done(&log) {
        tokio::select! {
            _ = &mut deadline => break,
            Some(ev) = a.events.recv() => log.push((Side::A, ev)),
            Some(ev) = b.events.recv() => log.push((Side::B, ev)),
        }
    }
    log
}

async fn connected_pair(a_name: &str, b_name: &str) -> (EngineHandle, EngineHandle, String, String, PathBuf, PathBuf) {
    let a_identity = Identity::generate();
    let b_identity = Identity::generate();
    let a_id = a_identity.device_id();
    let b_id = b_identity.device_id();
    let (mut a, a_dir) = make_engine(a_name, a_identity, &b_id, b_name).await;
    let (mut b, b_dir) = make_engine(b_name, b_identity, &a_id, a_name).await;
    let mut a_connected = false;
    let mut b_connected = false;
    // Generous: this file's tests run side by side, each pair of engines
    // finding each other over the same loopback mDNS.
    let log = collect_until(&mut a, &mut b, Duration::from_secs(30), |log| {
        for (side, ev) in log {
            if let SyncEvent::Connected { .. } = ev {
                match side {
                    Side::A => a_connected = true,
                    Side::B => b_connected = true,
                }
            }
        }
        a_connected && b_connected
    })
    .await;
    assert!(a_connected && b_connected, "the engines never connected: {log:?}");
    // A v3 send needs the receiver's mDNS-resolved address, and goes
    // in-band if it hasn't one within 5 s. Connected doesn't mean resolved
    // (the other side may have dialed), and on a runner busy with other
    // tests' engines resolving can take longer — so a test meant for the
    // data connection could quietly take the other path (CI saw exactly
    // that: a cut hook that never fired).
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(a.knows_address_for_test(&b_id) && b.knows_address_for_test(&a_id)) {
        assert!(Instant::now() < deadline, "the engines never resolved each other's addresses");
        collect_until(&mut a, &mut b, Duration::from_millis(100), |_| false).await;
    }
    (a, b, a_id, b_id, a_dir, b_dir)
}

/// Shuts the engines down and deletes their directories (trust store and
/// received files, the parents of `received_dirs`) and `other_dirs`. Only
/// reached when a test passes, so a failing one leaves everything behind
/// to look at.
fn finish(engines: Vec<EngineHandle>, received_dirs: &[&Path], other_dirs: &[&Path]) {
    for engine in engines {
        engine.shutdown();
    }
    for dir in received_dirs.iter().filter_map(|d| d.parent()).chain(other_dirs.iter().copied()) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Brings a dropped mesh connection back, returning everything seen until
/// A reports it connected. Either side may be the one with the other's
/// mDNS address cached, so the two take turns asking to reconnect — never
/// both at once, since two simultaneous dials each lose to the other's
/// "already connected" — starting with `first`.
async fn reconnect(a: &mut EngineHandle, b: &mut EngineHandle, a_id: &str, b_id: &str, first: Side) -> Vec<(Side, SyncEvent)> {
    let mut log = Vec::new();
    for round in 0..20 {
        if (round % 2 == 0) == (first == Side::B) {
            b.send_command(EngineCommand::ReconnectPeer { peer_crypto_id: a_id.to_string() });
        } else {
            a.send_command(EngineCommand::ReconnectPeer { peer_crypto_id: b_id.to_string() });
        }
        log.extend(
            collect_until(a, b, Duration::from_secs(2), |log| {
                log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::Connected { .. }))
            })
            .await,
        );
        if log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::Connected { .. })) {
            return log;
        }
    }
    panic!("A never saw B reconnect: {log:?}");
}

fn received_path(log: &[(Side, SyncEvent)], side: Side, transfer: Option<&str>) -> Option<PathBuf> {
    log.iter().find_map(|(s, ev)| match ev {
        SyncEvent::FileReceived { transfer_id, path, .. } if *s == side && transfer.is_none_or(|t| t == transfer_id) => {
            Some(PathBuf::from(path))
        }
        _ => None,
    })
}

fn count(log: &[(Side, SyncEvent)], side: Side, pred: impl Fn(&SyncEvent) -> bool) -> usize {
    log.iter().filter(|(s, ev)| *s == side && pred(ev)).count()
}

fn failures(log: &[(Side, SyncEvent)]) -> Vec<String> {
    log.iter()
        .filter_map(|(side, ev)| match ev {
            SyncEvent::FileTransferFailed { reason, .. } => Some(format!("{side:?}: {reason}")),
            _ => None,
        })
        .collect()
}

/// A multi-MiB file goes over the v3 data connection: hash-verified, saved
/// under its own name with no `.part` file left behind, and announced and
/// tracked on both sides the same way the in-band path always was.
#[tokio::test]
async fn a_multi_mib_file_goes_over_the_data_connection() {
    let (mut a, mut b, _a_id, b_id, a_dir, b_dir) = connected_pair("DataA", "DataB").await;

    let source_dir = temp_dir("data-source");
    let source = source_dir.join("holiday video.mp4");
    let size = 12 * MIB + 12_345;
    write_test_file(&source, size, 7);

    a.send_command(EngineCommand::SendFile { peer_crypto_id: b_id.clone(), path: source.display().to_string() });
    let log = collect_until(&mut a, &mut b, Duration::from_secs(60), |log| {
        log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::FileSent { .. }))
            && received_path(log, Side::B, None).is_some()
            || log.iter().any(|(_, e)| matches!(e, SyncEvent::FileTransferFailed { .. }))
    })
    .await;

    assert!(failures(&log).is_empty(), "transfer failed: {:?}", failures(&log));
    let received = received_path(&log, Side::B, None).expect("B should report the file received");
    assert_eq!(received, b_dir.join("holiday video.mp4"));
    assert_eq!(file_hash(&received), file_hash(&source), "received content differs");
    assert!(part_files_in(&b_dir).is_empty(), "a .part file was left behind: {:?}", part_files_in(&b_dir));

    assert_eq!(count(&log, Side::A, |e| matches!(e, SyncEvent::FileSending { .. })), 1);
    assert_eq!(count(&log, Side::B, |e| matches!(e, SyncEvent::FileReceiving { .. })), 1);
    assert_eq!(
        count(&log, Side::A, |e| matches!(e, SyncEvent::FileSent { file_name, .. } if file_name == "holiday video.mp4")),
        1
    );
    // Progress at about one event per MiB, each way, ending at the total.
    for (side, direction) in [(Side::A, FileTransferDirection::Sending), (Side::B, FileTransferDirection::Receiving)] {
        let progress: Vec<u64> = log
            .iter()
            .filter_map(|(s, e)| match e {
                SyncEvent::FileTransferProgress { bytes_transferred, direction: d, .. } if *s == side && *d == direction => {
                    Some(*bytes_transferred)
                }
                _ => None,
            })
            .collect();
        assert!((10..=14).contains(&progress.len()), "{side:?}: {} progress events", progress.len());
        assert_eq!(progress.last().copied(), Some(size as u64));
    }

    assert_eq!(b.test_hooks().data_channel_files_received.load(Ordering::Relaxed), 1);
    assert_eq!(b.test_hooks().in_band_files_received.load(Ordering::Relaxed), 0);

    finish(vec![a, b], &[&a_dir, &b_dir], &[&source_dir]);
}

/// Cutting the data connection partway through stalls the transfer on
/// both sides instead of failing it; the sender comes back on its own (the
/// mesh connection is still up), the receiver answers with how much it
/// already has, and the file finishes from there — whole and correct.
#[tokio::test]
async fn a_cut_data_connection_resumes_where_it_left_off() {
    let (mut a, mut b, _a_id, b_id, a_dir, b_dir) = connected_pair("ResumeA", "ResumeB").await;

    let source_dir = temp_dir("resume-source");
    let source = source_dir.join("archive.tar");
    let size = 16 * MIB;
    write_test_file(&source, size, 99);

    a.test_hooks().cut_data_connection_after.store(5 * MIB as u64, Ordering::Relaxed);
    a.send_command(EngineCommand::SendFile { peer_crypto_id: b_id.clone(), path: source.display().to_string() });
    let log = collect_until(&mut a, &mut b, Duration::from_secs(90), |log| {
        log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::FileSent { .. }))
            && received_path(log, Side::B, None).is_some()
            || log.iter().any(|(_, e)| matches!(e, SyncEvent::FileTransferFailed { .. }))
    })
    .await;

    assert!(failures(&log).is_empty(), "transfer failed: {:?}", failures(&log));
    let received = received_path(&log, Side::B, None).expect("B should report the file received");
    assert_eq!(file_hash(&received), file_hash(&source), "resumed content differs");
    assert!(part_files_in(&b_dir).is_empty(), "a .part file was left behind");

    assert!(
        count(&log, Side::A, |e| matches!(e, SyncEvent::FileTransferStalled { .. })) >= 1,
        "the sender should report the stall"
    );
    // The receiver reports the stall once it reaches the end of what came
    // in before the cut — unless the sender's resume, a second later, gets
    // there first while it's still working through that (a debug build on
    // a busy machine), and ends that connection for it. Either way, it's
    // resumed, not failed.
    assert!(
        count(&log, Side::B, |e| matches!(e, SyncEvent::FileTransferStalled { .. })) >= 1
            || b.test_hooks().stale_data_connections_ended.load(Ordering::Relaxed) >= 1,
        "the receiver should report the stall, or have its old connection ended by the resume"
    );
    // Resumed from what B had, not from scratch.
    let resumed_at = b.test_hooks().last_resume_offset.load(Ordering::Relaxed);
    assert!(resumed_at > 0 && resumed_at <= 5 * MIB as u64, "resumed at {resumed_at}");
    // One transfer, announced once, finished once.
    assert_eq!(count(&log, Side::A, |e| matches!(e, SyncEvent::FileSending { .. })), 1);
    assert_eq!(count(&log, Side::B, |e| matches!(e, SyncEvent::FileReceiving { .. })), 1);
    assert_eq!(count(&log, Side::A, |e| matches!(e, SyncEvent::FileSent { .. })), 1);
    assert_eq!(count(&log, Side::B, |e| matches!(e, SyncEvent::FileReceived { .. })), 1);
    assert_eq!(b.test_hooks().data_channel_files_received.load(Ordering::Relaxed), 1);

    finish(vec![a, b], &[&a_dir, &b_dir], &[&source_dir]);
}

/// A disk error on the receiver partway through (a full disk, a FAT32
/// volume's 4 GiB limit) ends the transfer once, on both sides, with the
/// reason — the sender's resume offer is refused, not taken as a new
/// transfer that starts over from 0 and hits the same error again. The
/// hook fails only the first data connection, so a second one would show
/// up here as the file arriving after all.
#[tokio::test]
async fn a_disk_error_on_the_receiver_fails_the_transfer_once() {
    let (mut a, mut b, _a_id, b_id, a_dir, b_dir) = connected_pair("DiskErrorA", "DiskErrorB").await;

    let source_dir = temp_dir("disk-error-source");
    let source = source_dir.join("too big.iso");
    write_test_file(&source, 12 * MIB, 5);

    b.test_hooks().fail_writes_after.store(3 * MIB as u64, Ordering::Relaxed);
    a.send_command(EngineCommand::SendFile { peer_crypto_id: b_id.clone(), path: source.display().to_string() });
    let log = collect_until(&mut a, &mut b, Duration::from_secs(60), |log| {
        log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::FileSent { .. } | SyncEvent::FileTransferFailed { .. }))
    })
    .await;

    let sender_reason = log.iter().find_map(|(s, e)| match e {
        SyncEvent::FileTransferFailed { reason, .. } if *s == Side::A => Some(reason.clone()),
        _ => None,
    });
    let sender_reason = sender_reason.unwrap_or_else(|| panic!("the sender should report the transfer failed: {log:?}"));
    assert!(sender_reason.starts_with("DiskErrorB couldn't save the file:"), "{sender_reason}");
    assert!(sender_reason.contains("disk full (test hook)"), "{sender_reason}");
    // Announced once and failed once on the receiver — never restarted.
    assert_eq!(count(&log, Side::B, |e| matches!(e, SyncEvent::FileReceiving { .. })), 1, "{log:?}");
    assert_eq!(count(&log, Side::B, |e| matches!(e, SyncEvent::FileTransferFailed { .. })), 1, "{log:?}");
    assert_eq!(count(&log, Side::B, |e| matches!(e, SyncEvent::FileReceived { .. })), 0, "{log:?}");
    assert_eq!(b.test_hooks().data_channel_files_received.load(Ordering::Relaxed), 0);
    assert!(part_files_in(&b_dir).is_empty(), "a .part file was left behind: {:?}", part_files_in(&b_dir));

    finish(vec![a, b], &[&a_dir, &b_dir], &[&source_dir]);
}

/// Sharing many files with one device sends four of them at a time over
/// data connections (`SENDS_PER_PEER`), the rest waiting their turn — and
/// every one still arrives whole, under its own name.
#[tokio::test]
async fn many_files_to_one_device_go_a_few_at_a_time() {
    let (mut a, mut b, _a_id, b_id, a_dir, b_dir) = connected_pair("QueueA", "QueueB").await;

    let source_dir = temp_dir("queue-source");
    let sources: Vec<PathBuf> = (0..7)
        .map(|i| {
            let path = source_dir.join(format!("photo {i}.jpg"));
            write_test_file(&path, 3 * MIB + i, 100 + i as u64);
            path
        })
        .collect();
    for path in &sources {
        a.send_command(EngineCommand::SendFile { peer_crypto_id: b_id.clone(), path: path.display().to_string() });
    }
    let log = collect_until(&mut a, &mut b, Duration::from_secs(120), |log| {
        count(log, Side::A, |e| matches!(e, SyncEvent::FileSent { .. })) == sources.len()
            && count(log, Side::B, |e| matches!(e, SyncEvent::FileReceived { .. })) == sources.len()
            || log.iter().any(|(_, e)| matches!(e, SyncEvent::FileTransferFailed { .. }))
    })
    .await;

    assert!(failures(&log).is_empty(), "transfer failed: {:?}", failures(&log));
    // On the sender, a file is in flight from its `FileSending` (accepted,
    // so it had its turn) until it ends.
    let (mut in_flight, mut most) = (0usize, 0usize);
    for (side, event) in &log {
        match event {
            SyncEvent::FileSending { .. } if *side == Side::A => {
                in_flight += 1;
                most = most.max(in_flight);
            }
            SyncEvent::FileSent { .. } if *side == Side::A => in_flight -= 1,
            _ => {}
        }
    }
    assert!(most <= 4, "{most} files were being sent to one device at once");
    for path in &sources {
        let received = b_dir.join(path.file_name().unwrap());
        assert_eq!(file_hash(&received), file_hash(path), "{} differs", received.display());
    }
    assert!(part_files_in(&b_dir).is_empty(), "a .part file was left behind: {:?}", part_files_in(&b_dir));

    finish(vec![a, b], &[&a_dir, &b_dir], &[&source_dir]);
}

/// What a misbehaving v3 sender, driven by hand, runs into: a 17th
/// unfinished transfer from one device is refused with the reason (offered
/// all at once, so the limit has to hold while the offers are answered
/// concurrently), and a data frame longer than the 1 MiB frame size ends
/// that transfer rather than being read into memory.
#[tokio::test]
async fn a_misbehaving_v3_sender_is_held_to_the_limits() {
    use tokio::io::AsyncWriteExt;

    let sender_identity = Identity::generate();
    let sender_id = sender_identity.device_id();
    let engine_identity = Identity::generate();
    let (mut engine, received_dir) = make_engine("LimitReceiver", engine_identity, &sender_id, "Flooder").await;
    let port = loop {
        match tokio::time::timeout(Duration::from_secs(5), engine.events.recv()).await {
            Ok(Some(SyncEvent::Listening { port })) => break port,
            Ok(Some(_)) => continue,
            other => panic!("no Listening event: {other:?}"),
        }
    };
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let tls = continuity_crypto::generate_self_signed(&sender_identity).unwrap();
    let mut conn = continuity_net::connect(addr, &tls).await.unwrap();
    let device = DeviceInfo { id: sender_id.clone(), name: "Flooder".into(), platform: Platform::Linux, protocol_version: 3 };
    continuity_net::announce_and_identify(&mut conn, &device).await.unwrap();

    for i in 0..17 {
        let offer = Message::FileOffer {
            transfer_id: format!("{i:016x}"),
            origin_device: sender_id.clone(),
            file_name: format!("flood {i}.bin"),
            size_bytes: 4 * MIB as u64,
            mime: None,
            data_channel: true,
            resume: false,
        };
        continuity_net::write_message(&mut conn, &offer).await.unwrap();
    }
    let (mut accepted, mut refused) = (Vec::new(), Vec::new());
    while accepted.len() + refused.len() < 17 {
        let message = tokio::time::timeout(Duration::from_secs(10), continuity_net::read_message(&mut conn))
            .await
            .unwrap_or_else(|_| panic!("only {} of 17 offers were answered", accepted.len() + refused.len()))
            .unwrap();
        match message {
            Message::FileAccept { transfer_id, accepted: true, data_channel: true, offset: 0, .. } => accepted.push(transfer_id),
            Message::FileAccept { accepted: false, reason, .. } => refused.push(reason),
            Message::Ping | Message::NowPlayingUpdate { .. } | Message::ClipboardUpdate { .. } => continue,
            other => panic!("unexpected message instead of FileAccept: {other:?}"),
        }
    }
    assert_eq!(accepted.len(), 16, "refused: {refused:?}");
    assert_eq!(refused, vec![Some("LimitReceiver is already receiving 16 files from this device".to_string())]);

    // A data connection for one of them whose first frame claims 2 MiB.
    let mut data = continuity_net::connect(addr, &tls).await.unwrap();
    let handshake = Message::FileStreamHandshake { transfer_id: accepted[0].clone(), offset: 0 };
    continuity_net::write_message(&mut data, &handshake).await.unwrap();
    data.write_all(&(2 * MIB as u32).to_be_bytes()).await.unwrap();
    data.flush().await.unwrap();
    let verdict = tokio::time::timeout(Duration::from_secs(10), continuity_net::read_message(&mut data))
        .await
        .expect("no verdict on the data connection")
        .unwrap();
    match verdict {
        Message::FileVerified { ok: false, reason: Some(reason), .. } => {
            assert!(reason.contains("over the 1048576-byte limit"), "{reason}")
        }
        other => panic!("expected a refusing FileVerified, got {other:?}"),
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, engine.events.recv()).await {
            Ok(Some(SyncEvent::FileTransferFailed { transfer_id, .. })) if transfer_id == accepted[0] => break,
            Ok(Some(_)) => continue,
            other => panic!("the receiver never reported the transfer failed: {other:?}"),
        }
    }

    finish(vec![engine], &[&received_dir], &[]);
}

/// The mesh connection going down too while a transfer is stalled: the
/// sender waits for the peer to come back, re-offers on the new mesh
/// connection — whose arrival the receiver has to tell apart from a data
/// connection, since it's expecting one from that same peer — and the
/// file still finishes from where it stopped.
#[tokio::test]
async fn a_stalled_transfer_resumes_after_the_mesh_connection_comes_back() {
    let (mut a, mut b, a_id, b_id, a_dir, b_dir) = connected_pair("MeshDropA", "MeshDropB").await;
    // Only the test brings the mesh back (below): left to themselves, the
    // engines can redial before the sender's retry notices it's down — on
    // a busy runner other tests' mDNS traffic prompts one any moment.
    a.test_hooks().auto_dial_off.store(true, Ordering::Relaxed);
    b.test_hooks().auto_dial_off.store(true, Ordering::Relaxed);

    let source_dir = temp_dir("mesh-drop-source");
    let source = source_dir.join("photos.zip");
    let size = 10 * MIB + 99;
    write_test_file(&source, size, 1234);

    a.test_hooks().cut_data_connection_after.store(2 * MIB as u64, Ordering::Relaxed);
    a.send_command(EngineCommand::SendFile { peer_crypto_id: b_id.clone(), path: source.display().to_string() });
    let mut log = collect_until(&mut a, &mut b, Duration::from_secs(30), |log| {
        log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::FileTransferStalled { .. }))
    })
    .await;
    assert!(log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::FileTransferStalled { .. })), "{log:?}");
    // The sender's first retry is a second away; take the mesh down first.
    a.drop_connection_for_test(&b_id);
    log.extend(
        collect_until(&mut a, &mut b, Duration::from_secs(10), |log| {
            log.iter().any(|(s, e)| {
                *s == Side::A && matches!(e, SyncEvent::FileTransferStalled { reason, .. } if reason.starts_with("Waiting for"))
            })
        })
        .await,
    );
    assert!(
        log.iter().any(|(s, e)| *s == Side::A
            && matches!(e, SyncEvent::FileTransferStalled { reason, .. } if reason.starts_with("Waiting for"))),
        "the sender should be waiting for the peer to reconnect: {log:?}"
    );

    // A dials first (it has B's address — it just used it for the data
    // connection), so B sees a mesh connection from a peer it's expecting
    // a data connection from.
    log.extend(reconnect(&mut a, &mut b, &a_id, &b_id, Side::A).await);
    let done = |log: &[(Side, SyncEvent)]| {
        log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::FileSent { .. }))
            && received_path(log, Side::B, None).is_some()
            || log.iter().any(|(_, e)| matches!(e, SyncEvent::FileTransferFailed { .. }))
    };
    if !done(&log) {
        log.extend(collect_until(&mut a, &mut b, Duration::from_secs(60), done).await);
    }

    assert!(failures(&log).is_empty(), "transfer failed: {:?}", failures(&log));
    let received = received_path(&log, Side::B, None).expect("B should report the file received");
    assert_eq!(file_hash(&received), file_hash(&source), "resumed content differs");
    assert!(part_files_in(&b_dir).is_empty(), "a .part file was left behind");
    let resumed_at = b.test_hooks().last_resume_offset.load(Ordering::Relaxed);
    assert!(resumed_at > 0 && resumed_at <= 2 * MIB as u64, "resumed at {resumed_at}");
    assert_eq!(count(&log, Side::B, |e| matches!(e, SyncEvent::FileReceived { .. })), 1);
    assert!(
        b.test_hooks().mesh_connections_read_first.load(Ordering::Relaxed) >= 1,
        "B should have read A's reconnect before handing it to the mesh"
    );

    finish(vec![a, b], &[&a_dir, &b_dir], &[&source_dir]);
}

/// A v3 receiver that answers the data-connection offer with
/// `data_channel: false` gets the file in-band — and the sender reports it
/// the same way it would have either way.
#[tokio::test]
async fn a_receiver_that_refuses_the_data_connection_gets_it_in_band() {
    let (mut a, mut b, _a_id, b_id, a_dir, b_dir) = connected_pair("RefuseA", "RefuseB").await;
    b.test_hooks().refuse_data_channel.store(true, Ordering::Relaxed);

    let source_dir = temp_dir("refuse-source");
    let source = source_dir.join("notes.pdf");
    write_test_file(&source, 3 * MIB + 7, 3);

    a.send_command(EngineCommand::SendFile { peer_crypto_id: b_id.clone(), path: source.display().to_string() });
    let log = collect_until(&mut a, &mut b, Duration::from_secs(60), |log| {
        log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::FileSent { .. }))
            && received_path(log, Side::B, None).is_some()
            || log.iter().any(|(_, e)| matches!(e, SyncEvent::FileTransferFailed { .. }))
    })
    .await;

    assert!(failures(&log).is_empty(), "transfer failed: {:?}", failures(&log));
    let received = received_path(&log, Side::B, None).expect("B should report the file received");
    assert_eq!(received, b_dir.join("notes.pdf"));
    assert_eq!(file_hash(&received), file_hash(&source));
    assert_eq!(b.test_hooks().in_band_files_received.load(Ordering::Relaxed), 1);
    assert_eq!(b.test_hooks().data_channel_files_received.load(Ordering::Relaxed), 0);
    assert_eq!(count(&log, Side::A, |e| matches!(e, SyncEvent::FileSending { .. })), 1);
    assert_eq!(
        count(&log, Side::A, |e| matches!(e, SyncEvent::FileSent { file_name, .. } if file_name == "notes.pdf")),
        1,
        "the in-band path should still know the file's name: {log:?}"
    );

    finish(vec![a, b], &[&a_dir, &b_dir], &[&source_dir]);
}

/// A peer that announced protocol v2 is never offered a data connection:
/// the file goes in-band from the start.
#[tokio::test]
async fn a_v2_peer_is_sent_files_in_band() {
    let (mut a, mut b, a_id, b_id, a_dir, b_dir) = connected_pair("OldPeerA", "OldPeerB").await;

    // Make B announce v2 on a fresh connection: hook first, then drop the
    // connection and bring it back so A hears the new announcement.
    b.test_hooks().announce_protocol_version.store(2, Ordering::Relaxed);
    b.send_command(EngineCommand::DisconnectPeer { peer_crypto_id: a_id.clone() });
    let log = collect_until(&mut a, &mut b, Duration::from_secs(10), |log| {
        log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::Disconnected { .. }))
    })
    .await;
    assert!(log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::Disconnected { .. })), "{log:?}");
    // B first: that also clears its own manual disconnect.
    let log = reconnect(&mut a, &mut b, &a_id, &b_id, Side::B).await;
    let version = log.iter().find_map(|(s, e)| match e {
        SyncEvent::Connected { peer } if *s == Side::A => Some(peer.protocol_version),
        _ => None,
    });
    assert_eq!(version, Some(2), "B should have reconnected announcing v2: {log:?}");

    let source_dir = temp_dir("v2-source");
    let source = source_dir.join("old peer.bin");
    write_test_file(&source, 2 * MIB + 3, 11);
    a.send_command(EngineCommand::SendFile { peer_crypto_id: b_id.clone(), path: source.display().to_string() });
    let log = collect_until(&mut a, &mut b, Duration::from_secs(60), |log| {
        log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::FileSent { .. }))
            && received_path(log, Side::B, None).is_some()
            || log.iter().any(|(_, e)| matches!(e, SyncEvent::FileTransferFailed { .. }))
    })
    .await;

    assert!(failures(&log).is_empty(), "transfer failed: {:?}", failures(&log));
    let received = received_path(&log, Side::B, None).expect("B should report the file received");
    assert_eq!(file_hash(&received), file_hash(&source));
    assert_eq!(b.test_hooks().in_band_files_received.load(Ordering::Relaxed), 1);
    assert_eq!(b.test_hooks().data_channel_files_received.load(Ordering::Relaxed), 0);

    finish(vec![a, b], &[&a_dir, &b_dir], &[&source_dir]);
}

/// A real old-format sender: a hand-driven connection speaking v2's raw
/// JSON — a `FileOffer` without any of the v3 fields — gets its file
/// accepted (with an answer it can parse), received in-band, and verified.
#[tokio::test]
async fn an_old_format_offer_from_a_v2_sender_is_received_in_band() {
    let old_identity = Identity::generate();
    let old_id = old_identity.device_id();
    let engine_identity = Identity::generate();
    let engine_id = engine_identity.device_id();
    let (mut engine, received_dir) = make_engine("NewReceiver", engine_identity, &old_id, "OldSender").await;

    let port = loop {
        match tokio::time::timeout(Duration::from_secs(5), engine.events.recv()).await {
            Ok(Some(SyncEvent::Listening { port })) => break port,
            Ok(Some(_)) => continue,
            other => panic!("no Listening event: {other:?}"),
        }
    };
    let tls = continuity_crypto::generate_self_signed(&old_identity).unwrap();
    let mut conn = continuity_net::connect(format!("127.0.0.1:{port}").parse().unwrap(), &tls).await.unwrap();
    let old_device = DeviceInfo { id: old_id.clone(), name: "OldSender".into(), platform: Platform::Linux, protocol_version: 2 };
    let engine_device = continuity_net::announce_and_identify(&mut conn, &old_device).await.unwrap();
    assert_eq!(engine_device.id, engine_id);

    let content: Vec<u8> = (0..200_000u32).flat_map(|i| i.to_le_bytes()).collect();
    let offer = format!(
        r#"{{"type":"file_offer","transfer_id":"0123456789abcdef","origin_device":"{old_id}","file_name":"from v2.bin","size_bytes":{},"mime":null}}"#,
        content.len()
    );
    write_raw(&mut conn, offer.as_bytes()).await;

    // The answer must be one a v2 peer understands: a plain accept.
    let accept = loop {
        match continuity_net::read_message(&mut conn).await.unwrap() {
            Message::FileAccept { transfer_id, accepted, data_channel, .. } => break (transfer_id, accepted, data_channel),
            Message::Ping | Message::NowPlayingUpdate { .. } | Message::ClipboardUpdate { .. } => continue,
            other => panic!("unexpected message instead of FileAccept: {other:?}"),
        }
    };
    assert_eq!(accept, ("0123456789abcdef".to_string(), true, false));

    for (seq, chunk) in content.chunks(64 * 1024).enumerate() {
        let message = Message::FileChunk { transfer_id: "0123456789abcdef".into(), seq: seq as u64, data: chunk.to_vec() };
        continuity_net::write_message(&mut conn, &message).await.unwrap();
    }
    let complete = Message::FileComplete { transfer_id: "0123456789abcdef".into(), content_hash: content_hash(&content) };
    continuity_net::write_message(&mut conn, &complete).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let path = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, engine.events.recv()).await {
            Ok(Some(SyncEvent::FileReceived { path, .. })) => break PathBuf::from(path),
            Ok(Some(SyncEvent::FileTransferFailed { reason, .. })) => panic!("receive failed: {reason}"),
            Ok(Some(_)) => continue,
            other => panic!("never received the file: {other:?}"),
        }
    };
    assert_eq!(path, received_dir.join("from v2.bin"));
    assert_eq!(std::fs::read(&path).unwrap(), content);

    finish(vec![engine], &[&received_dir], &[]);
}

async fn write_raw(conn: &mut continuity_net::Connection, json: &[u8]) {
    use tokio::io::AsyncWriteExt;
    conn.write_all(&(json.len() as u32).to_be_bytes()).await.unwrap();
    conn.write_all(json).await.unwrap();
    conn.flush().await.unwrap();
}

/// Not a correctness test — times a ~512 MiB loopback transfer over the
/// data connection and over the in-band path, for comparison. Run it in
/// release mode, on its own:
///
/// `cargo test --release -p continuity-daemon --test file_transfer -- --ignored --nocapture`
///
/// Set `CONTINUITY_BENCH_DIR` (e.g. to a tmpfs like `/dev/shm`) to keep the
/// disk out of the numbers; both files go there, received ones via
/// `SetReceivedFilesDir`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn measure_512_mib_loopback_throughput() {
    let (mut a, mut b, _a_id, b_id, a_dir, b_engine_dir) = connected_pair("SpeedA", "SpeedB").await;
    let source_dir = match std::env::var_os("CONTINUITY_BENCH_DIR") {
        Some(base) => {
            let dir = PathBuf::from(base).join(format!("continuity-bench-{:08x}", rand::random::<u32>()));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
        None => temp_dir("speed-source"),
    };
    let b_dir = source_dir.join("received");
    b.send_command(EngineCommand::SetReceivedFilesDir { path: b_dir.display().to_string() });
    let size = 512 * MIB;
    let source = source_dir.join("big.bin");
    write_test_file(&source, size, 42);
    let source_hash = file_hash(&source);

    // Each path twice, alternating, so neither gets the warm (or cold) page
    // cache and writeback state to itself.
    let runs = [
        ("in-band FileChunk (legacy)", true),
        ("data connection (v3)", false),
        ("in-band FileChunk (legacy)", true),
        ("data connection (v3)", false),
    ];
    for (label, in_band) in runs {
        a.test_hooks().force_in_band.store(in_band, Ordering::Relaxed);
        let started = Instant::now();
        a.send_command(EngineCommand::SendFile { peer_crypto_id: b_id.clone(), path: source.display().to_string() });
        let log = collect_until(&mut a, &mut b, Duration::from_secs(600), |log| {
            log.iter().any(|(s, e)| *s == Side::A && matches!(e, SyncEvent::FileSent { .. }))
                && received_path(log, Side::B, None).is_some()
                || log.iter().any(|(_, e)| matches!(e, SyncEvent::FileTransferFailed { .. }))
        })
        .await;
        let elapsed = started.elapsed();
        assert!(failures(&log).is_empty(), "{label}: transfer failed: {:?}", failures(&log));
        let received = received_path(&log, Side::B, None).expect("received");
        assert_eq!(file_hash(&received), source_hash, "{label}: content differs");
        let mb_per_s = size as f64 / 1_000_000.0 / elapsed.as_secs_f64();
        println!("{label}: {size} bytes in {:.2}s = {mb_per_s:.0} MB/s", elapsed.as_secs_f64());
        std::fs::remove_file(&received).unwrap();
    }
    assert!(part_files_in(&b_dir).is_empty());

    finish(vec![a, b], &[&a_dir, &b_engine_dir], &[&source_dir]);
}

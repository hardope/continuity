//! v3 file transfer: one dedicated TLS data connection per transfer, the
//! file's bytes as raw 1 MiB frames, and resume after an interruption. See
//! `docs/protocol.md` ("File transfer") for the wire shape; this is why it
//! looks the way it does.
//!
//! **Why a separate connection.** The in-band path (`FileChunk`s on the mesh
//! connection, still used with peers older than v3) shares one ordered
//! message queue with clipboard updates, pings and remote-control input, so
//! a large file held all of them up behind it (the "write-starves-ping" bug
//! class, see Connection liveness in the protocol doc) — and base64 inside
//! JSON, 64 KiB at a time with a flush per message, capped it at a fraction
//! of what the link could carry. The screen stream already learned this
//! lesson (`handle_screen_stream_connection`); this is the same shape for
//! files: the **sender** dials the receiver's normal listener, mutual TLS
//! proves who it is, and a `FileStreamHandshake` binds the connection to
//! one transfer.
//!
//! **Why the state lives here, not in a connection's loop.** The in-band
//! path keeps a receiving file in the mesh connection's own `receiving`
//! map, so a dropped connection meant a failed transfer and a half-written
//! file left under its final name. Here the receiver's partial file (open
//! `.part` file, incremental hash, bytes written) is engine-wide, keyed by
//! (peer, transfer), and outlives both the data connection and the mesh
//! connection for `RESUME_WINDOW`; the sender's side is simply its own
//! long-running task. Either connection dropping stalls the transfer
//! (`SyncEvent::FileTransferStalled`) rather than failing it: the sender
//! re-offers with `resume: true` — straight away with a short backoff while
//! the mesh is still up, or on the peer's next `Connected` if it isn't —
//! and the receiver answers with how much it has.
//!
//! **Pipelining.** Disk and hashing run on blocking threads, joined to the
//! network side by small bounded channels (`PIPELINE_DEPTH` frames): the
//! sender's reader thread reads and hashes the next frames while the async
//! side writes the previous ones, and the receiver's writer thread writes
//! and hashes while the async side reads. Memory per transfer stays at a
//! few MiB whatever the file size — Android in particular has little to
//! spare.

use super::{
    handle_connection, unique_destination_path, SharedState, FILE_ACCEPT_TIMEOUT, PROGRESS_EVENT_BYTES,
};
use crate::events::{FileTransferDirection, SyncEvent};
use continuity_crypto::IncrementalHash;
use continuity_net::{
    connect, read_frame, read_frame_up_to, read_message, write_frame_unflushed, write_message, Connection, FramingError,
};
use continuity_proto::{DeviceInfo, Message};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::{AbortHandle, JoinHandle};

/// Size of each raw frame on a data connection. Big enough that the 4-byte
/// length prefix and per-frame bookkeeping vanish next to the payload,
/// small enough that `PIPELINE_DEPTH` of them in flight is cheap.
pub(super) const FRAME_SIZE: usize = 1024 * 1024;

/// Frames buffered between a blocking disk thread and the network side, in
/// each direction — enough that neither stalls waiting on the other's
/// momentary hiccups, while capping memory at a few MiB per transfer.
const PIPELINE_DEPTH: usize = 4;

/// How long an interrupted transfer stays resumable on either side. After
/// this, both report `FileTransferFailed` and the receiver deletes its
/// `.part` file.
pub(super) const RESUME_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Headroom kept free on the receiving volume on top of the file itself —
/// filling a phone's storage to the last byte breaks other apps, and the
/// free-space figure is only a snapshot anyway.
const FREE_SPACE_MARGIN: u64 = 64 * 1024 * 1024;

/// Bounds the TCP connect + TLS handshake of a data connection. A peer
/// behind a firewall that silently drops the SYN would otherwise hold the
/// transfer for the OS's own connect timeout (minutes) before falling back.
const DATA_DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an inbound connection from a peer with a transfer under way may
/// take to send its first message before it's dropped (see
/// `route_inbound`). A mesh reconnect sends its `DeviceAnnounce` at once,
/// and a data connection its handshake.
const FIRST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one frame write on a data connection (or the end of the
/// stream) may go without completing before the sender gives up on that
/// connection and resumes on a new one. **Real bug fixed here**: these
/// writes had no limit at all. A receiver that vanishes without a reset (a
/// laptop docking onto Ethernet and getting a new address, a phone leaving
/// the network) leaves unacknowledged data in flight, and in that state the
/// OS sends no keepalive probes: the write just blocked until TCP's own
/// retransmission limit, about 15 minutes. By then the receiver, whose
/// idle end keepalive closes in about 2, had expired its partial file, so
/// the resume started over from 0. A minute of no progress at all is far
/// past any link that's merely slow (it's 1 MiB, so under 17 KiB/s), and
/// what it costs a live but stalled one is a resume, not the transfer.
const WRITE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the sender waits for `FileVerified` after `FileComplete`. The
/// receiver only has its last few queued frames left to write by then (it
/// hashes as it goes), plus a rename.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(120);

/// Data-connection attempts in a row, with the mesh still up, before the
/// sender gives up — with `RETRY_BASE_DELAY` doubling between them (1, 2,
/// 4, 8, 16 s). Any attempt that moves data resets the count.
const RETRY_LIMIT: u32 = 5;
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// How often the receiver looks for partial files whose sender never came
/// back. Coarse on purpose: it only has to beat a 10-minute window.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// How long a resume offer waits for the transfer's previous data
/// connection to let go of the partial file (see `answer_offer`).
const RELEASE_WAIT: Duration = Duration::from_secs(30);

/// Files this side sends one peer at once over data connections; any more
/// wait their turn (see `send_over_data_channel`). They share one link, so
/// more at once wouldn't finish any sooner — it would only hold more files
/// open on both sides, and a share of a hundred photos used to open them
/// all at the same moment.
pub(super) const SENDS_PER_PEER: usize = 4;

/// Unfinished incoming transfers one peer may have here at once — four
/// times what this engine sends at a time, so only a misbehaving sender
/// meets it. Each holds a `.part` file open for up to `RESUME_WINDOW`, so
/// without a limit a peer could use up this process's file descriptors
/// (macOS gives an app launched from the Finder 256).
const INCOMING_PER_PEER: usize = 16;

/// Serializes picking a final file name and renaming onto it, so two
/// transfers of the same name finishing at the same moment can't both pick
/// — and the second silently overwrite — the same free name.
static FINALIZE_LOCK: Mutex<()> = Mutex::new(());

/// A `FileAccept`, as delivered to the `send_file` task waiting for it.
pub(super) struct FileAcceptReply {
    pub accepted: bool,
    pub data_channel: bool,
    pub offset: u64,
    pub reason: Option<String>,
}

/// What `send_file` hands the data-channel sender.
pub(super) struct Outgoing {
    pub transfer_id: String,
    pub peer_id: String,
    pub peer_name: String,
    pub path: PathBuf,
    pub file_name: String,
    pub size_bytes: u64,
}

/// An outgoing v3 transfer, for cancelling it from outside its task
/// (`DisconnectPeer`, `RevokeDevice`, `Reset`, shutdown).
pub(super) struct OutgoingTransfer {
    peer_id: String,
    cancel: Arc<Notify>,
}

/// How `send_over_data_channel` ended, for `send_file`.
pub(super) enum DataChannelEnd {
    /// Sent, failed or cancelled — already reported.
    Done,
    /// No data connection after all: send in-band from the start. With
    /// `reoffer`, the receiver set up for a data connection and must be
    /// re-offered the transfer in-band first; without, it already chose
    /// in-band itself. `announced`: `FileSending` was already emitted.
    InBand { tx: mpsc::UnboundedSender<Message>, reoffer: bool, announced: bool },
}

/// Receiver-side state of one v3 transfer, engine-wide — see the module
/// docs. Keyed by (peer id, transfer id) in `SharedState::incoming_transfers`.
pub(super) struct IncomingTransfer {
    /// Unique per entry — a data connection's writer thread that finishes
    /// after its entry was replaced must not touch the replacement.
    epoch: u64,
    peer_name: String,
    file_name: String,
    size_bytes: u64,
    /// `<final name>.part` in the received-files directory the transfer
    /// started in; renamed to a free final name only once verified.
    part_path: PathBuf,
    /// The open file and hash so far — `None` while a data connection's
    /// writer thread owns them (`Phase::Streaming`/`Finalizing`).
    part: Option<PartFile>,
    phase: Phase,
    /// The current data connection's task, so a resume offer can end a
    /// connection this side hasn't noticed is dead yet, and a cancel can
    /// end one that's alive.
    stream: Option<AbortHandle>,
    /// When it last went idle (`Waiting`, `Completed` or `Failed`) — what
    /// `RESUME_WINDOW` counts from.
    idle_since: Instant,
}

enum Phase {
    /// No data connection: accepted and not yet dialed, or interrupted.
    Waiting,
    Streaming,
    /// Verifying and renaming — not to be interrupted by a resume offer.
    Finalizing,
    /// Saved. Kept for `RESUME_WINDOW` so that if the sender lost the
    /// connection right before `FileVerified` reached it, its resume is
    /// told "you're done" instead of being taken as a new transfer and the
    /// file saved twice.
    Completed { content_hash: String },
    /// Writing it failed (a full disk, a FAT32 volume's 4 GiB limit, an
    /// I/O error), already reported here, partial file deleted. Kept for
    /// `RESUME_WINDOW` like `Completed`, so the sender's resume offer is
    /// refused with `reason` — see `return_part`.
    Failed { reason: String },
}

struct PartFile {
    file: std::fs::File,
    hasher: IncrementalHash,
    written: u64,
    /// Bytes still to come, counted against new offers while this file
    /// exists (see `SpaceReservations`).
    reserved: Arc<AtomicU64>,
}

fn emit_progress(state: &SharedState, transfer_id: &str, before: u64, now: u64, total: u64, direction: FileTransferDirection) {
    // Same cadence as the in-band path: once per `PROGRESS_EVENT_BYTES`
    // boundary crossed, and at the end.
    if now >= total || before / PROGRESS_EVENT_BYTES != now / PROGRESS_EVENT_BYTES {
        state.emit(SyncEvent::FileTransferProgress {
            transfer_id: transfer_id.to_string(),
            bytes_transferred: now,
            total_bytes: total,
            direction,
        });
    }
}

// ---------------------------------------------------------------------------
// Free space and the received-files directory
// ---------------------------------------------------------------------------

/// Why a file of which `remaining` bytes are still to come can't be taken
/// with `available` bytes free, `promised` of them to other files already
/// on their way — `None` if it can. Pure, so the decision is unit-tested
/// without a full disk.
pub(super) fn free_space_refusal(device_name: &str, remaining: u64, available: u64, promised: u64) -> Option<String> {
    let needed = remaining.saturating_add(FREE_SPACE_MARGIN);
    let free = available.saturating_sub(promised);
    if free >= needed {
        return None;
    }
    let free = if promised > 0 {
        format!("{} free once the files already on their way are in", format_bytes(free))
    } else {
        format!("{} free", format_bytes(free))
    };
    Some(format!("Not enough free space on {device_name} (needs {}, {free})", format_bytes(needed)))
}

/// Space promised to files being received, so that a new offer is checked
/// against what will be free once they're all in, not just what's free
/// now. **Real bug fixed here**: each offer used to be checked on its own,
/// so three 3 GB files offered together to a phone with 5 GB free all
/// passed — nothing had been written yet — and the disk then filled up
/// partway through, which the margin is there to prevent.
///
/// Each incoming file, on either path, holds an `Arc` counter of the
/// bytes it still has to write and lowers it as it writes; this keeps only
/// a `Weak` to it, so a transfer that ends any way at all — saved, failed,
/// cancelled, expired, its connection gone — stops counting the moment
/// its state is dropped, with no cleanup call anywhere to forget.
#[derive(Default)]
pub(super) struct SpaceReservations {
    /// The received-files directory each file started in, and its counter.
    /// Only files in the directory being checked count: one on another
    /// volume takes nothing from this one.
    entries: Mutex<Vec<(PathBuf, Weak<AtomicU64>)>>,
}

impl SpaceReservations {
    /// Whether `remaining` more bytes fit in `dir` alongside everything
    /// already promised there; `own` is a resuming transfer's own share of
    /// that, which it doesn't need twice. A volume that can't report its
    /// free space isn't grounds to refuse — creating the file will fail on
    /// its own if the directory is really unusable.
    pub(super) fn check(&self, dir: &Path, device_name: &str, remaining: u64, own: u64) -> Option<String> {
        self.check_with(dir, device_name, remaining, own, || fs4::available_space(dir))
    }

    /// `check`, and if the file fits, the counter for it to hold — under one
    /// lock, so offers answered at the same moment can't each find the same
    /// space free.
    pub(super) fn reserve(&self, dir: &Path, device_name: &str, size: u64) -> Result<Arc<AtomicU64>, String> {
        self.reserve_with(dir, device_name, size, || fs4::available_space(dir))
    }

    fn check_with(
        &self,
        dir: &Path,
        device_name: &str,
        remaining: u64,
        own: u64,
        available: impl FnOnce() -> std::io::Result<u64>,
    ) -> Option<String> {
        let mut entries = self.entries.lock().unwrap();
        Self::refusal(&mut entries, dir, device_name, remaining, own, available)
    }

    fn reserve_with(
        &self,
        dir: &Path,
        device_name: &str,
        size: u64,
        available: impl FnOnce() -> std::io::Result<u64>,
    ) -> Result<Arc<AtomicU64>, String> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(reason) = Self::refusal(&mut entries, dir, device_name, size, 0, available) {
            return Err(reason);
        }
        let counter = Arc::new(AtomicU64::new(size));
        entries.push((dir.to_path_buf(), Arc::downgrade(&counter)));
        Ok(counter)
    }

    fn refusal(
        entries: &mut Vec<(PathBuf, Weak<AtomicU64>)>,
        dir: &Path,
        device_name: &str,
        remaining: u64,
        own: u64,
        available: impl FnOnce() -> std::io::Result<u64>,
    ) -> Option<String> {
        entries.retain(|(_, counter)| counter.strong_count() > 0);
        let promised: u64 = entries
            .iter()
            .filter(|(d, _)| d == dir)
            .filter_map(|(_, counter)| counter.upgrade())
            .map(|counter| counter.load(Ordering::Relaxed))
            .fold(0, u64::saturating_add);
        match available() {
            Ok(available) => free_space_refusal(device_name, remaining, available, promised.saturating_sub(own)),
            Err(e) => {
                tracing::debug!("couldn't read the free space in {}: {e}", dir.display());
                None
            }
        }
    }
}

/// Decimal units, as file managers on every platform this runs on show
/// them, with a decimal place only where it says something ("1.2 GB",
/// "300 MB").
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    let text = if value < 100.0 { format!("{value:.1}") } else { format!("{value:.0}") };
    format!("{} {}", text.trim_end_matches(".0"), UNITS[unit])
}

/// For `EngineCommand::SetReceivedFilesDir`: creates `dir` if needed and
/// proves a file can actually be created in it — a read-only mount or a
/// folder without write permission passes `create_dir_all` and `is_dir`
/// both.
pub(super) fn check_usable_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    if !dir.is_dir() {
        return Err("it isn't a folder".to_string());
    }
    let probe = dir.join(format!(".continuity-write-test-{:016x}", rand::random::<u64>()));
    std::fs::File::create(&probe).map_err(|e| format!("can't write there: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Creates `<name>.part` for a new incoming file, where `<name>` is the
/// name it would be saved under right now — `file_name`, or `stem (n).ext`
/// if that's taken — skipping any name whose `.part` already exists too
/// (another transfer of the same name, or one left behind by a previous
/// run). `create_new`, so it can never truncate someone else's file. The
/// final name is picked again at completion, since the folder can change
/// in the meantime.
fn create_part_file(dir: &Path, file_name: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let stem = Path::new(file_name).file_stem().and_then(|s| s.to_str()).unwrap_or(file_name);
    let ext = Path::new(file_name).extension().and_then(|s| s.to_str());
    for i in 0..1000 {
        let name = match (i, ext) {
            (0, _) => file_name.to_string(),
            (_, Some(ext)) => format!("{stem} ({i}).{ext}"),
            (_, None) => format!("{stem} ({i})"),
        };
        if dir.join(&name).exists() {
            continue;
        }
        let part_path = dir.join(format!("{name}.part"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&part_path) {
            Ok(file) => return Ok((part_path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free file name left"))
}

// ---------------------------------------------------------------------------
// Receiving: offers
// ---------------------------------------------------------------------------

/// Answers a `FileOffer { data_channel: true, .. }` — new or a resume —
/// from its own task, so the mesh connection's reader isn't held up by
/// disk work or by waiting for an old data connection to let go.
pub(super) fn handle_data_channel_offer(
    state: &Arc<SharedState>,
    tx: &mpsc::UnboundedSender<Message>,
    peer: &DeviceInfo,
    transfer_id: String,
    file_name: String,
    size_bytes: u64,
    resume: bool,
) {
    let state = state.clone();
    let tx = tx.clone();
    let peer = peer.clone();
    tokio::spawn(async move {
        let reply = answer_offer(&state, &peer, transfer_id, file_name, size_bytes, resume).await;
        let _ = tx.send(reply);
    });
}

async fn answer_offer(
    state: &Arc<SharedState>,
    peer: &DeviceInfo,
    transfer_id: String,
    file_name: String,
    size_bytes: u64,
    resume: bool,
) -> Message {
    let key = (peer.id.clone(), transfer_id.clone());
    let accept = |offset: u64| Message::FileAccept {
        transfer_id: transfer_id.clone(),
        accepted: true,
        data_channel: true,
        offset,
        reason: None,
    };
    let refuse = |reason: String| Message::FileAccept {
        transfer_id: transfer_id.clone(),
        accepted: false,
        data_channel: false,
        offset: 0,
        reason: Some(reason),
    };

    // Anything already here under this id is the same transfer coming
    // back (an id is random per send, and only this peer's are looked at).
    enum Found {
        Nothing,
        Resumable { written: u64, size: u64, dir: PathBuf, reserved: u64 },
        Done { size: u64 },
        Failed { reason: String },
        Busy,
    }
    let deadline = Instant::now() + RELEASE_WAIT;
    loop {
        // Created before looking, so a change between the look and the
        // wait below still wakes it.
        let changed = state.incoming_transfers_changed.notified();
        let found = {
            let mut incoming = state.incoming_transfers.lock().unwrap();
            match incoming.get_mut(&key) {
                None => Found::Nothing,
                Some(entry) => match &entry.phase {
                    Phase::Waiting => {
                        entry.idle_since = Instant::now();
                        Found::Resumable {
                            written: entry.part.as_ref().map_or(0, |p| p.written),
                            size: entry.size_bytes,
                            dir: entry.part_path.parent().map(Path::to_path_buf).unwrap_or_default(),
                            reserved: entry.part.as_ref().map_or(0, |p| p.reserved.load(Ordering::Relaxed)),
                        }
                    }
                    Phase::Completed { .. } => Found::Done { size: entry.size_bytes },
                    Phase::Failed { reason } => Found::Failed { reason: reason.clone() },
                    // The sender only re-offers once it's given up on its
                    // data connection, so this side's end of it is dead
                    // too, just not noticed yet (a half-open TCP
                    // connection can take minutes to error). End it; its
                    // writer hands the partial file back and this loop
                    // goes around again.
                    Phase::Streaming => {
                        if let Some(stream) = entry.stream.take() {
                            stream.abort();
                        }
                        Found::Busy
                    }
                    Phase::Finalizing => Found::Busy,
                },
            }
        };
        match found {
            Found::Nothing => break,
            Found::Done { size } => return accept(size),
            Found::Failed { reason } => return refuse(reason),
            Found::Resumable { written, size, dir, reserved } => {
                if let Some(reason) = state.reserved_space.check(&dir, &state.my_device.name, size - written, reserved) {
                    tracing::warn!("can't resume '{file_name}' from '{}': {reason}", peer.name);
                    discard_incoming(state, &key, Some(&reason));
                    return refuse(reason);
                }
                if written > 0 {
                    state.test_hooks.last_resume_offset.store(written, Ordering::Relaxed);
                }
                return accept(written);
            }
            Found::Busy => {
                if Instant::now() >= deadline {
                    return refuse("the previous connection for this transfer is still closing".to_string());
                }
                let _ = tokio::time::timeout(Duration::from_millis(100), changed).await;
            }
        }
    }

    if resume {
        // Restarted since, or the partial file expired: nothing to resume
        // from, so it's a new transfer from 0. An old `.part` file from
        // before a restart is left alone — this run doesn't know it.
        tracing::debug!("resume offer for unknown transfer {transfer_id} from '{}' — starting over", peer.name);
    }
    let too_many = format!("{} is already receiving {INCOMING_PER_PEER} files from this device", state.my_device.name);
    if unfinished_from(&state.incoming_transfers.lock().unwrap(), &peer.id) >= INCOMING_PER_PEER {
        tracing::warn!("refusing '{file_name}' from '{}': {too_many}", peer.name);
        return refuse(too_many);
    }
    let dir = state.received_files_dir.lock().unwrap().clone();
    let device_name = state.my_device.name.clone();
    let name = file_name.clone();
    let blocking_state = state.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let reserved = blocking_state.reserved_space.reserve(&dir, &device_name, size_bytes)?;
        let (part_path, file) =
            create_part_file(&dir, &name).map_err(|e| format!("{device_name} couldn't create the file: {e}"))?;
        Ok((part_path, file, reserved))
    })
    .await
    .unwrap_or_else(|e| Err(format!("couldn't prepare the file: {e}")));
    let (part_path, file, reserved) = match prepared {
        Ok(prepared) => prepared,
        Err(reason) => {
            tracing::warn!("refusing '{file_name}' from '{}': {reason}", peer.name);
            return refuse(reason);
        }
    };

    {
        let mut incoming = state.incoming_transfers.lock().unwrap();
        if incoming.contains_key(&key) {
            // A second offer for the same id raced this one in — keep the
            // first, drop what this one created.
            drop(incoming);
            let _ = std::fs::remove_file(&part_path);
            return refuse("a transfer with this id is already under way".to_string());
        }
        if unfinished_from(&incoming, &peer.id) >= INCOMING_PER_PEER {
            // Others from the same peer were set up while this one created
            // its file.
            drop(incoming);
            let _ = std::fs::remove_file(&part_path);
            return refuse(too_many);
        }
        incoming.insert(
            key,
            IncomingTransfer {
                epoch: state.next_transfer_epoch.fetch_add(1, Ordering::Relaxed),
                peer_name: peer.name.clone(),
                file_name: file_name.clone(),
                size_bytes,
                part_path,
                part: Some(PartFile { file, hasher: IncrementalHash::new(), written: 0, reserved }),
                phase: Phase::Waiting,
                stream: None,
                idle_since: Instant::now(),
            },
        );
    }
    state.emit(SyncEvent::FileReceiving {
        transfer_id: transfer_id.clone(),
        from_name: peer.name.clone(),
        file_name,
        size_bytes,
    });
    accept(0)
}

/// Transfers from `peer_id` still holding a partial file here — not the
/// `Completed` and `Failed` records kept for a late resume offer.
fn unfinished_from(incoming: &std::collections::HashMap<(String, String), IncomingTransfer>, peer_id: &str) -> usize {
    incoming
        .iter()
        .filter(|((peer, _), e)| peer == peer_id && !matches!(e.phase, Phase::Completed { .. } | Phase::Failed { .. }))
        .count()
}

/// Removes an incoming transfer and its `.part` file. With `reason`,
/// reports it failed (unless it had already completed). If a data
/// connection's writer still holds the file, the writer deletes it when it
/// finds its entry gone — deleting an open file fails on Windows.
fn discard_incoming(state: &Arc<SharedState>, key: &(String, String), reason: Option<&str>) -> bool {
    let Some(entry) = state.incoming_transfers.lock().unwrap().remove(key) else {
        return false;
    };
    if let Some(stream) = entry.stream {
        stream.abort();
    }
    if let Some(part) = entry.part {
        drop(part);
        let _ = std::fs::remove_file(&entry.part_path);
    }
    if let (Some(reason), false) = (reason, matches!(entry.phase, Phase::Completed { .. } | Phase::Failed { .. })) {
        state.emit(SyncEvent::FileTransferFailed { transfer_id: key.1.clone(), reason: reason.to_string() });
    }
    state.incoming_transfers_changed.notify_waiters();
    true
}

/// For the in-band `handle_file_offer`: a v3 sender that couldn't open
/// its data connection re-offers the same transfer in-band, so whatever
/// was set up for the data connection goes. Returns whether there was
/// anything — i.e. this transfer was already announced with
/// `FileReceiving`.
pub(super) fn discard_for_in_band_fallback(state: &Arc<SharedState>, peer_id: &str, transfer_id: &str) -> bool {
    discard_incoming(state, &(peer_id.to_string(), transfer_id.to_string()), None)
}

// ---------------------------------------------------------------------------
// Receiving: the data connection
// ---------------------------------------------------------------------------

/// Whether an inbound connection from `peer_id` could be one of its data
/// connections — true while any transfer from it is known here at all.
pub(super) fn may_dial_data_connection(state: &SharedState, peer_id: &str) -> bool {
    state.incoming_transfers.lock().unwrap().keys().any(|(peer, _)| peer == peer_id)
}

/// An inbound connection from a peer that may be dialing a data connection
/// (see `may_dial_data_connection`): reads its first message and sends it
/// where it belongs — a `FileStreamHandshake` to the transfer it names,
/// anything else (a mesh `DeviceAnnounce`) on to `handle_connection` with
/// that message already read.
pub(super) async fn route_inbound(mut conn: Connection, state: Arc<SharedState>, peer_id: String, addr: SocketAddr) {
    let first = match tokio::time::timeout(FIRST_MESSAGE_TIMEOUT, read_message(&mut conn)).await {
        Ok(Ok(message)) => message,
        Ok(Err(e)) => {
            tracing::debug!("inbound connection from {addr} closed before its first message: {e}");
            return;
        }
        Err(_) => {
            tracing::debug!("inbound connection from {addr} sent nothing in {FIRST_MESSAGE_TIMEOUT:?}, dropping it");
            return;
        }
    };
    let Message::FileStreamHandshake { transfer_id, offset } = first else {
        state.test_hooks.mesh_connections_read_first.fetch_add(1, Ordering::Relaxed);
        let id_for_registry = peer_id.clone();
        let conn_state = state.clone();
        let join = tokio::spawn(async move {
            if let Err(e) = handle_connection(conn, conn_state, peer_id, Some(first)).await {
                tracing::debug!("inbound connection from {addr} ended: {e}");
            }
        });
        state.connection_handles.lock().unwrap().insert(id_for_registry, join.abort_handle());
        return;
    };

    let key = (peer_id, transfer_id);
    match claim_for_stream(&state, &key, offset) {
        Ok(Claim::Stream(claim)) => {
            let epoch = claim.epoch;
            let stream = start_writer(&state, &key, claim);
            let join = tokio::spawn(receive_stream(conn, state.clone(), key.clone(), stream));
            let mut incoming = state.incoming_transfers.lock().unwrap();
            if let Some(entry) = incoming.get_mut(&key) {
                if entry.epoch == epoch && matches!(entry.phase, Phase::Streaming) && entry.stream.is_none() {
                    entry.stream = Some(join.abort_handle());
                }
            }
        }
        Ok(Claim::AlreadyComplete { content_hash }) => {
            tokio::spawn(confirm_already_complete(conn, key.1, content_hash));
        }
        Err(e) => tracing::debug!("refusing a data connection from {addr} for transfer {}: {e}", key.1),
    }
}

enum Claim {
    Stream(StreamClaim),
    AlreadyComplete { content_hash: String },
}

struct StreamClaim {
    epoch: u64,
    part: PartFile,
    part_path: PathBuf,
    file_name: String,
    peer_name: String,
    size_bytes: u64,
}

/// Validates a `FileStreamHandshake` against what this side has for that
/// transfer and, if it fits, hands the data connection the partial file.
fn claim_for_stream(state: &SharedState, key: &(String, String), offset: u64) -> Result<Claim, String> {
    let mut incoming = state.incoming_transfers.lock().unwrap();
    let entry = incoming.get_mut(key).ok_or("no such transfer from this peer")?;
    match &entry.phase {
        Phase::Waiting => {
            let written = entry.part.as_ref().map_or(0, |p| p.written);
            if written != offset {
                return Err(format!("it starts at {offset} but {written} bytes are here"));
            }
            let part = entry.part.take().ok_or("its file is missing")?;
            entry.phase = Phase::Streaming;
            Ok(Claim::Stream(StreamClaim {
                epoch: entry.epoch,
                part,
                part_path: entry.part_path.clone(),
                file_name: entry.file_name.clone(),
                peer_name: entry.peer_name.clone(),
                size_bytes: entry.size_bytes,
            }))
        }
        Phase::Completed { content_hash } if offset == entry.size_bytes => {
            Ok(Claim::AlreadyComplete { content_hash: content_hash.clone() })
        }
        _ => Err("another connection is using it".to_string()),
    }
}

/// A claimed data connection's partial file, already in its writer
/// thread's hands, and what the async read side needs to feed it.
struct ReceivingStream {
    epoch: u64,
    /// Bytes already in the file when this connection took it over.
    offset: u64,
    part_path: PathBuf,
    file_name: String,
    peer_name: String,
    size_bytes: u64,
    frames: mpsc::Sender<Vec<u8>>,
    writer: JoinHandle<()>,
}

/// Hands a claimed partial file straight to its writer thread
/// (`write_part`), before the data connection's async half is even
/// spawned. **Real bug fixed here**: the async half used to start the
/// writer itself, on its first poll — so a task aborted before it was
/// ever polled (a resume offer ending it in `answer_offer`, a cancel or
/// shutdown in `discard_incoming`, both of which can land in that window)
/// dropped the file without handing it back. The entry was then stuck in
/// `Streaming` with no file: never expired, its resume refused, its `.part`
/// file never deleted. With the writer started first, dropping that task
/// at any point just closes the channel, and the writer hands the file
/// back as for any other end of the stream.
fn start_writer(state: &Arc<SharedState>, key: &(String, String), claim: StreamClaim) -> ReceivingStream {
    let StreamClaim { epoch, part, part_path, file_name, peer_name, size_bytes } = claim;
    let offset = part.written;
    let (frames, frames_rx) = mpsc::channel::<Vec<u8>>(PIPELINE_DEPTH);
    let writer = {
        let state = state.clone();
        let key = key.clone();
        let part_path = part_path.clone();
        tokio::task::spawn_blocking(move || write_part(&state, &key, epoch, part, &part_path, frames_rx, size_bytes))
    };
    ReceivingStream { epoch, offset, part_path, file_name, peer_name, size_bytes, frames, writer }
}

/// How a data connection's read side ended, short of a `FileComplete`.
enum ReceiveError {
    /// The connection broke — resumable.
    Interrupted(String),
    /// The sender broke the protocol — the transfer is over.
    Fatal(String),
    /// The writer thread stopped (a disk error); it already reported it.
    WriterStopped,
}

async fn receive_stream(mut conn: Connection, state: Arc<SharedState>, key: (String, String), stream: ReceivingStream) {
    let ReceivingStream { epoch, offset, part_path, file_name, peer_name, size_bytes, frames: frames_tx, writer } = stream;
    let transfer_id = key.1.clone();
    if offset > 0 {
        // Picking up after a stall — the next progress event is what tells
        // a shell it resumed (see `SyncEvent::FileTransferStalled`).
        state.emit(SyncEvent::FileTransferProgress {
            transfer_id: transfer_id.clone(),
            bytes_transferred: offset,
            total_bytes: size_bytes,
            direction: FileTransferDirection::Receiving,
        });
    }

    let read = async {
        let mut received = offset;
        loop {
            // Nothing a sender writes here is over `FRAME_SIZE`, so a
            // longer frame isn't buffered at all (it could be 16 MiB, the
            // screen stream's limit, times every frame in the pipeline).
            let frame = match read_frame_up_to(&mut conn, FRAME_SIZE as u32).await {
                Ok(frame) => frame,
                Err(FramingError::TooLarge(len)) => {
                    return Err(ReceiveError::Fatal(format!("the sender sent a {len}-byte frame, over the {FRAME_SIZE}-byte limit")));
                }
                Err(e) => return Err(ReceiveError::Interrupted(e.to_string())),
            };
            if frame.is_empty() {
                break;
            }
            received += frame.len() as u64;
            if received > size_bytes {
                return Err(ReceiveError::Fatal("the sender sent more than the file's size".to_string()));
            }
            frames_tx.send(frame).await.map_err(|_| ReceiveError::WriterStopped)?;
        }
        match read_message(&mut conn).await {
            Ok(Message::FileComplete { transfer_id: id, content_hash }) if id == transfer_id => Ok(content_hash),
            Ok(other) => Err(ReceiveError::Fatal(format!("expected FileComplete, got {other:?}"))),
            Err(e) => Err(ReceiveError::Interrupted(e.to_string())),
        }
    }
    .await;
    // Ending the read side's sender is what tells the writer to finish:
    // it writes whatever is still queued (whole frames — a frame cut off
    // by a dropped connection never got this far) and hands the file back.
    let still_ours = |state: &SharedState| {
        state.incoming_transfers.lock().unwrap().get(&key).is_some_and(|e| e.epoch == epoch)
    };

    let claimed_hash = match read {
        Ok(hash) => hash,
        Err(error) => {
            drop(frames_tx);
            let _ = writer.await;
            match error {
                ReceiveError::Interrupted(e) if still_ours(&state) => {
                    tracing::debug!("data connection for {transfer_id} from '{peer_name}' broke: {e}");
                    state.emit(SyncEvent::FileTransferStalled {
                        transfer_id,
                        reason: format!("Connection to '{peer_name}' lost — waiting for it to resume"),
                    });
                }
                ReceiveError::Fatal(reason) => {
                    tracing::warn!("data connection for {transfer_id} from '{peer_name}': {reason}");
                    discard_incoming(&state, &key, Some(&reason));
                    let _ = write_message(&mut conn, &Message::FileVerified { transfer_id, ok: false, reason: Some(reason) }).await;
                }
                // A disk error: the writer reported it and left the entry
                // `Failed`, which is what the sender hears when it re-offers
                // — it's still writing frames, so it wouldn't read a
                // `FileVerified` sent here.
                ReceiveError::WriterStopped => {}
                // Cancelled, expired or replaced meanwhile.
                ReceiveError::Interrupted(_) => {}
            }
            return;
        }
    };

    if let Some(entry) = state.incoming_transfers.lock().unwrap().get_mut(&key).filter(|e| e.epoch == epoch) {
        entry.phase = Phase::Finalizing;
    }
    drop(frames_tx);
    let _ = writer.await;
    let part = state
        .incoming_transfers
        .lock()
        .unwrap()
        .get_mut(&key)
        .filter(|e| e.epoch == epoch)
        .and_then(|e| e.part.take());
    let Some(part) = part else {
        // The writer failed (and reported it), or this was cancelled —
        // either way it's over here, and the sender should hear that
        // rather than take a closed connection as something to resume.
        let failed = state.incoming_transfers.lock().unwrap().get(&key).and_then(|e| match &e.phase {
            Phase::Failed { reason } => Some(reason.clone()),
            _ => None,
        });
        let reason = failed.unwrap_or_else(|| format!("{} couldn't save the file", state.my_device.name));
        let _ = write_message(&mut conn, &Message::FileVerified { transfer_id, ok: false, reason: Some(reason) }).await;
        return;
    };

    let name = file_name.clone();
    let finalized = tokio::task::spawn_blocking(move || finalize_part(part, &part_path, &name, size_bytes, &claimed_hash))
        .await
        .unwrap_or_else(|e| Err(format!("couldn't save the file: {e}")));
    let verdict = match finalized {
        Ok((path, content_hash)) => {
            if let Some(entry) = state.incoming_transfers.lock().unwrap().get_mut(&key).filter(|e| e.epoch == epoch) {
                entry.phase = Phase::Completed { content_hash };
                entry.idle_since = Instant::now();
                entry.stream = None;
            }
            state.incoming_transfers_changed.notify_waiters();
            state.test_hooks.data_channel_files_received.fetch_add(1, Ordering::Relaxed);
            state.emit(SyncEvent::FileReceived {
                transfer_id: transfer_id.clone(),
                file_name,
                path: path.display().to_string(),
            });
            Message::FileVerified { transfer_id, ok: true, reason: None }
        }
        Err(reason) => {
            // `finalize_part` already deleted the `.part` file.
            state.incoming_transfers.lock().unwrap().remove(&key);
            state.incoming_transfers_changed.notify_waiters();
            state.emit(SyncEvent::FileTransferFailed { transfer_id: transfer_id.clone(), reason: reason.clone() });
            Message::FileVerified { transfer_id, ok: false, reason: Some(reason) }
        }
    };
    let _ = write_message(&mut conn, &verdict).await;
}

/// The receiver's blocking half of a data connection: writes and hashes
/// each frame as it comes off the channel, then hands the file back to the
/// transfer's entry (`return_part`) whether the stream finished, broke, or
/// was aborted — the one place the partial file goes back, so none of
/// those paths can lose it. Started before the async half (see
/// `start_writer`), so it owns the file from the moment it's claimed.
fn write_part(
    state: &SharedState,
    key: &(String, String),
    epoch: u64,
    mut part: PartFile,
    part_path: &Path,
    mut frames: mpsc::Receiver<Vec<u8>>,
    size_bytes: u64,
) {
    let mut failure = None;
    let fail_after = state.test_hooks.fail_writes_after.swap(0, Ordering::Relaxed);
    while let Some(frame) = frames.blocking_recv() {
        let written = if fail_after > 0 && part.written + frame.len() as u64 > fail_after {
            Err(std::io::Error::other("disk full (test hook)"))
        } else {
            part.file.write_all(&frame)
        };
        if let Err(e) = written {
            failure = Some(e);
            break;
        }
        part.hasher.update(&frame);
        let before = part.written;
        part.written += frame.len() as u64;
        part.reserved.store(size_bytes.saturating_sub(part.written), Ordering::Relaxed);
        emit_progress(state, &key.1, before, part.written, size_bytes, FileTransferDirection::Receiving);
    }
    drop(frames);
    return_part(state, key, epoch, part, part_path, failure);
}

/// **Real bug fixed here**: a disk error used to remove the transfer's
/// entry outright. The sender, still writing frames, only saw its data
/// connection close — which looks like any other interruption — so it
/// re-offered with `resume: true`, the receiver found nothing under that
/// id and took it as a new transfer from 0, and the file was sent again,
/// failed at the same point (a full disk, a FAT32 volume's 4 GiB limit),
/// and went round again forever. The entry now stays as `Phase::Failed` for
/// `RESUME_WINDOW`, so that re-offer is refused with the reason.
fn return_part(state: &SharedState, key: &(String, String), epoch: u64, part: PartFile, part_path: &Path, failure: Option<std::io::Error>) {
    let mut incoming = state.incoming_transfers.lock().unwrap();
    let ours = incoming.get(key).is_some_and(|e| e.epoch == epoch);
    if !ours || failure.is_some() {
        // Cancelled, expired or shut down while this held the file (its
        // remover couldn't delete an open file everywhere), or a disk
        // error: either way this partial file is finished with.
        if let (true, Some(e)) = (ours, &failure) {
            let entry = incoming.get_mut(key).expect("checked just above");
            entry.phase = Phase::Failed { reason: format!("{} couldn't save the file: {e}", state.my_device.name) };
            entry.idle_since = Instant::now();
            entry.stream = None;
        }
        drop(incoming);
        drop(part);
        let _ = std::fs::remove_file(part_path);
        if let (true, Some(e)) = (ours, failure) {
            state.emit(SyncEvent::FileTransferFailed { transfer_id: key.1.clone(), reason: format!("couldn't write the file: {e}") });
        }
        state.incoming_transfers_changed.notify_waiters();
        return;
    }
    let entry = incoming.get_mut(key).expect("checked just above");
    entry.part = Some(part);
    if matches!(entry.phase, Phase::Streaming) {
        // Broke or aborted mid-stream (not finishing, which set
        // `Finalizing` first): back to waiting for a resume.
        entry.phase = Phase::Waiting;
        entry.idle_since = Instant::now();
        entry.stream = None;
    }
    drop(incoming);
    state.incoming_transfers_changed.notify_waiters();
}

/// Checks a complete `.part` file against the sender's claim and moves it
/// to its final name. On any failure the `.part` file is deleted.
fn finalize_part(part: PartFile, part_path: &Path, file_name: &str, size_bytes: u64, claimed_hash: &str) -> Result<(PathBuf, String), String> {
    let PartFile { file, hasher, written, .. } = part;
    // Closed before the rename — Windows can't rename an open file.
    drop(file);
    let actual_hash = hasher.finalize_hex();
    if written != size_bytes || actual_hash != claimed_hash {
        let _ = std::fs::remove_file(part_path);
        return Err("integrity check failed after transfer".to_string());
    }
    let dir = part_path.parent().unwrap_or_else(|| Path::new("."));
    let _picking = FINALIZE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let destination = unique_destination_path(dir, file_name);
    if let Err(e) = std::fs::rename(part_path, &destination) {
        let _ = std::fs::remove_file(part_path);
        return Err(format!("couldn't save the file: {e}"));
    }
    Ok((destination, actual_hash))
}

/// A data connection for a transfer that already completed here: the
/// sender lost the connection before `FileVerified` reached it and resumed
/// at the very end. Confirms the hash again without saving anything twice.
///
/// **Real bug fixed here**: the whole exchange used to be under
/// `VERIFY_TIMEOUT`. But the sender only sends its end frame once it has
/// re-hashed the entire file from disk (`read_for_stream` hashes
/// `[0, offset)`, and here the offset is the whole size), which for a big
/// file on slow storage takes minutes — so a transfer saved here could be
/// answered "different contents" and reported failed on the sender. Like
/// `receive_stream`, the wait for the end of the data has no limit of its
/// own: this side is idle reading meanwhile, so TCP keepalive notices a
/// sender that's gone. Only the `FileComplete` that follows the end frame
/// straight away is timed.
async fn confirm_already_complete<S: AsyncRead + AsyncWrite + Unpin>(mut conn: S, transfer_id: String, content_hash: String) {
    let checked = async {
        if !read_frame(&mut conn).await?.is_empty() {
            return Ok(false);
        }
        let Ok(complete) = tokio::time::timeout(VERIFY_TIMEOUT, read_message(&mut conn)).await else {
            return Ok(false);
        };
        Ok::<_, FramingError>(matches!(
            complete?,
            Message::FileComplete { content_hash: claimed, .. } if claimed == content_hash
        ))
    }
    .await;
    let ok = matches!(checked, Ok(true));
    let reason = (!ok).then(|| "this transfer already finished here with different contents".to_string());
    let _ = write_message(&mut conn, &Message::FileVerified { transfer_id, ok, reason }).await;
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

/// Sends one `FileOffer` and waits for its `FileAccept` — `None` if none
/// came within `FILE_ACCEPT_TIMEOUT` or the mesh connection it went out on
/// closed first. Shared with the in-band path.
#[allow(clippy::too_many_arguments)]
pub(super) async fn offer(
    state: &Arc<SharedState>,
    tx: &mpsc::UnboundedSender<Message>,
    peer_id: &str,
    transfer_id: &str,
    file_name: &str,
    size_bytes: u64,
    data_channel: bool,
    resume: bool,
) -> Option<FileAcceptReply> {
    let (accept_tx, accept_rx) = oneshot::channel();
    state.pending_file_accepts.lock().unwrap().insert(transfer_id.to_string(), (peer_id.to_string(), accept_tx));
    let sent = tx
        .send(Message::FileOffer {
            transfer_id: transfer_id.to_string(),
            origin_device: state.my_device.id.clone(),
            file_name: file_name.to_string(),
            size_bytes,
            mime: None,
            data_channel,
            resume,
        })
        .is_ok();
    let reply = if sent {
        tokio::select! {
            reply = accept_rx => reply.ok(),
            _ = tokio::time::sleep(FILE_ACCEPT_TIMEOUT) => None,
            _ = tx.closed() => None,
        }
    } else {
        None
    };
    state.pending_file_accepts.lock().unwrap().remove(transfer_id);
    reply
}

fn peer_sender(state: &SharedState, peer_id: &str) -> Option<mpsc::UnboundedSender<Message>> {
    state.peer_senders.lock().unwrap().get(peer_id).cloned()
}

/// Waits for `peer_id`'s mesh connection to (re)appear, until `until`.
async fn wait_for_peer(state: &SharedState, peer_id: &str, until: Instant) -> Option<mpsc::UnboundedSender<Message>> {
    loop {
        // Created before the check, so a connect in between still wakes it.
        let connected = state.peer_connected.notified();
        if let Some(tx) = peer_sender(state, peer_id) {
            return Some(tx);
        }
        let now = Instant::now();
        if now >= until {
            return None;
        }
        let _ = tokio::time::timeout((until - now).min(Duration::from_secs(1)), connected).await;
    }
}

/// Sends `outgoing` over a data connection, resuming through interruptions
/// for up to `RESUME_WINDOW` — or reports it should go in-band after all.
/// Runs as long as the transfer does; cancellable through
/// `outgoing_transfers` (see `cancel_transfers`).
pub(super) async fn send_over_data_channel(state: &Arc<SharedState>, outgoing: Outgoing) -> DataChannelEnd {
    let cancel = Arc::new(Notify::new());
    state
        .outgoing_transfers
        .lock()
        .unwrap()
        .insert(outgoing.transfer_id.clone(), OutgoingTransfer { peer_id: outgoing.peer_id.clone(), cancel: cancel.clone() });
    let slots = state
        .outgoing_slots
        .lock()
        .unwrap()
        .entry(outgoing.peer_id.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(SENDS_PER_PEER)))
        .clone();
    let ended = tokio::select! {
        // `notify_one` stores a permit, so a cancel that lands before this
        // is first polled isn't missed — waiting for its turn included.
        _ = cancel.notified() => None,
        ended = async {
            // Its turn: nothing is offered until one of this peer's other
            // sends ends, and the turn is held through any resume.
            let _turn = slots.acquire().await;
            run_outgoing(state, &outgoing).await
        } => Some(ended),
    };
    // Whoever cancelled it removed the entry and already reported it.
    let cancelled = state.outgoing_transfers.lock().unwrap().remove(&outgoing.transfer_id).is_none();
    let transfer_id = outgoing.transfer_id;
    match ended {
        _ if cancelled => DataChannelEnd::Done,
        None => DataChannelEnd::Done,
        Some(Ended::Sent) => {
            state.emit(SyncEvent::FileSent { transfer_id, file_name: outgoing.file_name, to_name: outgoing.peer_name });
            DataChannelEnd::Done
        }
        Some(Ended::Failed(reason)) => {
            state.emit(SyncEvent::FileTransferFailed { transfer_id, reason });
            DataChannelEnd::Done
        }
        Some(Ended::InBand { tx, reoffer, announced }) => DataChannelEnd::InBand { tx, reoffer, announced },
    }
}

enum Ended {
    Sent,
    Failed(String),
    InBand { tx: mpsc::UnboundedSender<Message>, reoffer: bool, announced: bool },
}

/// How one data-connection attempt went wrong.
enum StreamError {
    /// Couldn't open the connection or send its handshake — no data went.
    Dial(String),
    /// Broke after the handshake, `sent` bytes of file data in.
    Interrupted { sent: u64, reason: String },
    /// The receiver checked the file and said no (it deleted its copy).
    Rejected(String),
    /// This side couldn't read its own file.
    Local(String),
}

/// Records that the transfer is stalled and tells the shell — again only
/// when the reason changes, not on every retry.
fn note_stall(state: &SharedState, transfer_id: &str, since: &mut Option<Instant>, last_reason: &mut Option<String>, reason: String) {
    if since.is_none() {
        *since = Some(Instant::now());
    }
    if last_reason.as_deref() != Some(reason.as_str()) {
        state.emit(SyncEvent::FileTransferStalled { transfer_id: transfer_id.to_string(), reason: reason.clone() });
        *last_reason = Some(reason);
    }
}

async fn run_outgoing(state: &Arc<SharedState>, o: &Outgoing) -> Ended {
    let mut resume = false;
    let mut announced = false;
    // Whether any attempt got file data onto a data connection — until one
    // has, a connection that can't be opened falls back to in-band.
    let mut data_sent = false;
    let mut stalled_since: Option<Instant> = None;
    let mut stall_reason: Option<String> = None;
    let mut retries: u32 = 0;

    loop {
        if stalled_since.is_some_and(|since| since.elapsed() >= RESUME_WINDOW) {
            return Ended::Failed(format!("couldn't resume sending to '{}' within 10 minutes", o.peer_name));
        }

        // Every (re-)offer goes over the mesh connection. Only missing
        // after a first attempt, or after waiting for its turn —
        // `send_file` checked it before starting.
        let tx = match peer_sender(state, &o.peer_id) {
            Some(tx) => tx,
            None => {
                let reason = format!("Waiting for '{}' to reconnect", o.peer_name);
                note_stall(state, &o.transfer_id, &mut stalled_since, &mut stall_reason, reason);
                let until = stalled_since.expect("just set") + RESUME_WINDOW;
                match wait_for_peer(state, &o.peer_id, until).await {
                    Some(tx) => {
                        retries = 0;
                        tx
                    }
                    None => continue,
                }
            }
        };

        let reply = match offer(state, &tx, &o.peer_id, &o.transfer_id, &o.file_name, o.size_bytes, true, resume).await {
            Some(reply) => reply,
            None if !resume => return Ended::Failed("declined, or the peer didn't respond in time".to_string()),
            None => {
                // No answer to a re-offer — most likely the mesh connection
                // went down under it, which the top of the loop waits out.
                let reason = format!("Connection to '{}' lost — retrying", o.peer_name);
                note_stall(state, &o.transfer_id, &mut stalled_since, &mut stall_reason, reason);
                retries += 1;
                if retries > RETRY_LIMIT && peer_sender(state, &o.peer_id).is_some() {
                    return Ended::Failed(format!("'{}' stopped answering", o.peer_name));
                }
                tokio::time::sleep(retry_delay(retries)).await;
                continue;
            }
        };
        if !reply.accepted {
            return Ended::Failed(reply.reason.unwrap_or_else(|| "declined, or the peer didn't respond in time".to_string()));
        }
        if !reply.data_channel {
            return Ended::InBand { tx, reoffer: false, announced };
        }
        if reply.offset > o.size_bytes {
            return Ended::Failed("the receiver reported having more of the file than there is".to_string());
        }
        if !announced {
            state.emit(SyncEvent::FileSending {
                transfer_id: o.transfer_id.clone(),
                to_name: o.peer_name.clone(),
                file_name: o.file_name.clone(),
                size_bytes: o.size_bytes,
            });
            announced = true;
        }

        let addr = state.known_addresses.lock().unwrap().get(&o.peer_id).copied();
        let result = match addr {
            Some(addr) => stream_file(state, o, addr, reply.offset).await,
            None => Err(StreamError::Dial("no network address is known for it".to_string())),
        };
        let (sent, reason) = match result {
            Ok(()) => return Ended::Sent,
            Err(StreamError::Rejected(reason)) | Err(StreamError::Local(reason)) => return Ended::Failed(reason),
            Err(StreamError::Dial(reason)) if !data_sent && reply.offset == 0 => {
                tracing::info!("no data connection to '{}' ({reason}) — sending in-band instead", o.peer_name);
                return Ended::InBand { tx, reoffer: true, announced };
            }
            Err(StreamError::Dial(reason)) => (0, reason),
            Err(StreamError::Interrupted { sent, reason }) => (sent, reason),
        };
        tracing::debug!("data connection for {} to '{}' broke after {sent} bytes: {reason}", o.transfer_id, o.peer_name);
        if sent > 0 {
            // It moved: a fresh stall, with a fresh window and retries.
            data_sent = true;
            stalled_since = None;
            stall_reason = None;
            retries = 0;
        }
        note_stall(state, &o.transfer_id, &mut stalled_since, &mut stall_reason, format!("Connection to '{}' lost — retrying", o.peer_name));
        retries += 1;
        resume = true;
        if retries > RETRY_LIMIT {
            if peer_sender(state, &o.peer_id).is_some() {
                return Ended::Failed(format!("lost the connection to '{}' and couldn't resume: {reason}", o.peer_name));
            }
            // Mesh down too: the top of the loop waits for it to come back.
        } else {
            tokio::time::sleep(retry_delay(retries)).await;
        }
    }
}

fn retry_delay(retries: u32) -> Duration {
    RETRY_BASE_DELAY * 2u32.pow(retries.saturating_sub(1).min(4))
}

/// One data-connection attempt: dial, handshake, stream from `offset`,
/// end, and wait for the receiver's verdict.
async fn stream_file(state: &Arc<SharedState>, o: &Outgoing, addr: SocketAddr, offset: u64) -> Result<(), StreamError> {
    // Reading (and re-hashing what the receiver already has) starts before
    // the dial, so the disk is busy while the TLS handshake happens.
    let (chunks_tx, mut chunks_rx) = mpsc::channel::<Vec<u8>>(PIPELINE_DEPTH);
    let reader = {
        let path = o.path.clone();
        let size = o.size_bytes;
        tokio::task::spawn_blocking(move || read_for_stream(&path, offset, size, chunks_tx))
    };

    let mut conn = match tokio::time::timeout(DATA_DIAL_TIMEOUT, connect(addr, &state.tls_identity)).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => return Err(StreamError::Dial(format!("couldn't connect to {addr}: {e}"))),
        Err(_) => return Err(StreamError::Dial(format!("timed out connecting to {addr}"))),
    };
    // The address is only a cache; make sure it's still the device the
    // file is meant for before sending it anything.
    match conn.peer_device_id() {
        Ok(id) if id == o.peer_id => {}
        _ => return Err(StreamError::Dial(format!("a different device answered at {addr}"))),
    }
    let handshake = Message::FileStreamHandshake { transfer_id: o.transfer_id.clone(), offset };
    if let Err(e) = write_message(&mut conn, &handshake).await {
        return Err(StreamError::Dial(format!("couldn't start the data connection: {e}")));
    }

    if offset > 0 {
        state.emit(SyncEvent::FileTransferProgress {
            transfer_id: o.transfer_id.clone(),
            bytes_transferred: offset,
            total_bytes: o.size_bytes,
            direction: FileTransferDirection::Sending,
        });
    }
    let cut_after = state.test_hooks.cut_data_connection_after.swap(0, Ordering::Relaxed);
    let mut sent = 0u64;
    let stalled = || format!("the receiver took nothing for {} seconds", WRITE_PROGRESS_TIMEOUT.as_secs());
    while let Some(chunk) = chunks_rx.recv().await {
        match tokio::time::timeout(WRITE_PROGRESS_TIMEOUT, write_frame_unflushed(&mut conn, &chunk)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(StreamError::Interrupted { sent, reason: e.to_string() }),
            Err(_) => return Err(StreamError::Interrupted { sent, reason: stalled() }),
        }
        let before = offset + sent;
        sent += chunk.len() as u64;
        emit_progress(state, &o.transfer_id, before, offset + sent, o.size_bytes, FileTransferDirection::Sending);
        if cut_after > 0 && sent >= cut_after {
            return Err(StreamError::Interrupted { sent, reason: "cut by a test hook".to_string() });
        }
    }
    let content_hash = match reader.await {
        Ok(Ok(hash)) => hash,
        Ok(Err(e)) => return Err(StreamError::Local(e)),
        Err(e) => return Err(StreamError::Local(format!("reading the file crashed: {e}"))),
    };

    let end = async {
        write_frame_unflushed(&mut conn, &[]).await?;
        // `write_message` flushes, which also pushes out any frame still
        // buffered from the loop above.
        write_message(&mut conn, &Message::FileComplete { transfer_id: o.transfer_id.clone(), content_hash }).await
    };
    match tokio::time::timeout(WRITE_PROGRESS_TIMEOUT, end).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(StreamError::Interrupted { sent, reason: e.to_string() }),
        Err(_) => return Err(StreamError::Interrupted { sent, reason: stalled() }),
    }
    match tokio::time::timeout(VERIFY_TIMEOUT, read_message(&mut conn)).await {
        Ok(Ok(Message::FileVerified { ok: true, .. })) => Ok(()),
        Ok(Ok(Message::FileVerified { ok: false, reason, .. })) => {
            Err(StreamError::Rejected(reason.unwrap_or_else(|| "the receiver couldn't verify the file".to_string())))
        }
        Ok(Ok(other)) => Err(StreamError::Interrupted { sent, reason: format!("expected FileVerified, got {other:?}") }),
        Ok(Err(e)) => Err(StreamError::Interrupted { sent, reason: e.to_string() }),
        Err(_) => Err(StreamError::Interrupted { sent, reason: "timed out waiting for the receiver to verify the file".to_string() }),
    }
}

/// The sender's blocking half: hashes `[0, offset)` — the receiver keeps
/// hashing from where it is, so the hash sent at the end must cover what
/// it already has, and that's re-read from disk rather than remembered,
/// because the receiver can be behind what this side last wrote (anything
/// in flight when a connection dies is lost) — then reads, hashes and
/// queues the rest one frame at a time. Exactly `size - offset` bytes: the
/// size offered is the size sent.
fn read_for_stream(path: &Path, offset: u64, size: u64, chunks: mpsc::Sender<Vec<u8>>) -> Result<String, String> {
    let read_error = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            format!("'{}' got shorter while it was being sent", path.display())
        } else {
            format!("error reading '{}': {e}", path.display())
        }
    };
    let mut file = std::fs::File::open(path).map_err(|e| format!("couldn't read '{}': {e}", path.display()))?;
    let mut hasher = IncrementalHash::new();
    if offset > 0 {
        let mut buf = vec![0u8; FRAME_SIZE];
        let mut hashed = 0u64;
        while hashed < offset {
            if chunks.is_closed() {
                // The dial failed meanwhile — don't hash gigabytes for nothing.
                return Err("stopped".to_string());
            }
            let want = (offset - hashed).min(FRAME_SIZE as u64) as usize;
            file.read_exact(&mut buf[..want]).map_err(read_error)?;
            hasher.update(&buf[..want]);
            hashed += want as u64;
        }
    }
    let mut remaining = size - offset;
    while remaining > 0 {
        let want = remaining.min(FRAME_SIZE as u64) as usize;
        let mut chunk = vec![0u8; want];
        file.read_exact(&mut chunk).map_err(read_error)?;
        hasher.update(&chunk);
        remaining -= want as u64;
        if chunks.blocking_send(chunk).is_err() {
            // The connection side stopped; nobody will read this.
            return Err("stopped".to_string());
        }
    }
    Ok(hasher.finalize_hex())
}

// ---------------------------------------------------------------------------
// Cancelling and expiry
// ---------------------------------------------------------------------------

/// Ends every v3 transfer with `peer_id` (or with everyone, for `None`),
/// both directions, reporting each with `reason` and deleting partial
/// files. For `DisconnectPeer`, `RevokeDevice` and `Reset` — v3 transfers
/// outlive mesh connections on purpose, so aborting those doesn't stop
/// them.
pub(super) fn cancel_transfers(state: &Arc<SharedState>, peer_id: Option<&str>, reason: &str) {
    end_transfers(state, peer_id, Some(reason));
}

/// `EngineHandle`'s drop: everything stops, nothing reported. Received
/// partial files go too — nothing could resume them once this engine is
/// gone.
pub(super) fn shut_down_transfers(state: &Arc<SharedState>) {
    end_transfers(state, None, None);
}

fn end_transfers(state: &Arc<SharedState>, peer_id: Option<&str>, reason: Option<&str>) {
    let matches = |peer: &str| peer_id.is_none_or(|p| p == peer);
    let outgoing: Vec<(String, OutgoingTransfer)> = {
        let mut map = state.outgoing_transfers.lock().unwrap();
        let ids: Vec<String> = map.iter().filter(|(_, o)| matches(&o.peer_id)).map(|(id, _)| id.clone()).collect();
        ids.into_iter().filter_map(|id| map.remove(&id).map(|o| (id, o))).collect()
    };
    for (transfer_id, transfer) in outgoing {
        transfer.cancel.notify_one();
        if let Some(reason) = reason {
            state.emit(SyncEvent::FileTransferFailed { transfer_id, reason: reason.to_string() });
        }
    }
    let incoming: Vec<(String, String)> = state
        .incoming_transfers
        .lock()
        .unwrap()
        .keys()
        .filter(|(peer, _)| matches(peer))
        .cloned()
        .collect();
    for key in incoming {
        discard_incoming(state, &key, reason);
    }
}

/// Drops incoming transfers idle for longer than `RESUME_WINDOW`: a partial
/// file whose sender never came back is reported failed and deleted; a
/// completed or failed one's record just goes.
pub(super) fn spawn_expiry_sweeper(state: Arc<SharedState>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let expired: Vec<((String, String), bool, String)> = {
                let incoming = state.incoming_transfers.lock().unwrap();
                incoming
                    .iter()
                    .filter(|(_, e)| {
                        matches!(e.phase, Phase::Waiting | Phase::Completed { .. } | Phase::Failed { .. })
                            && e.idle_since.elapsed() >= RESUME_WINDOW
                    })
                    .map(|(key, e)| (key.clone(), matches!(e.phase, Phase::Waiting), e.peer_name.clone()))
                    .collect()
            };
            for (key, waiting, peer_name) in expired {
                let reason = waiting.then(|| format!("'{peer_name}' didn't resume sending within 10 minutes"));
                discard_incoming(&state, &key, reason.as_deref());
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn enough_space_for_the_file_plus_the_margin_is_accepted() {
        assert_eq!(free_space_refusal("Laptop", 100 * MIB, 100 * MIB + FREE_SPACE_MARGIN, 0), None);
        assert_eq!(free_space_refusal("Laptop", 0, FREE_SPACE_MARGIN, 0), None);
    }

    #[test]
    fn less_than_the_file_plus_the_margin_is_refused_with_both_figures() {
        // Fits the file itself, but not the margin on top.
        let reason = free_space_refusal("Laptop", 100 * MIB, 100 * MIB, 0).expect("refused");
        assert!(reason.starts_with("Not enough free space on Laptop"), "{reason}");

        let reason = free_space_refusal("Pixel", 1_200_000_000, 300_000_000, 0).expect("refused");
        assert_eq!(reason, "Not enough free space on Pixel (needs 1.3 GB, 300 MB free)");
    }

    #[test]
    fn a_huge_file_does_not_overflow_the_check() {
        assert!(free_space_refusal("Laptop", u64::MAX, u64::MAX - 1, 0).is_some());
        assert!(free_space_refusal("Laptop", 1, u64::MAX, u64::MAX).is_some());
    }

    const GB: u64 = 1_000_000_000;

    #[test]
    fn files_on_their_way_take_their_space_from_later_offers() {
        let space = SpaceReservations::default();
        let dir = Path::new("/received");
        let five_gb_free = || Ok(5 * GB);
        let first = space.reserve_with(dir, "Pixel", 3 * GB, five_gb_free).expect("fits");
        // Nothing written yet, but the first file's 3 GB are spoken for.
        let reason = space.reserve_with(dir, "Pixel", 3 * GB, five_gb_free).expect_err("doesn't fit beside it");
        assert_eq!(reason, "Not enough free space on Pixel (needs 3.1 GB, 2 GB free once the files already on their way are in)");

        // As it writes, the disk's free space and its promise go down
        // together: with 2 GB of it written, the same 2 GB are left.
        first.store(GB, Ordering::Relaxed);
        assert!(space.reserve_with(dir, "Pixel", 3 * GB, || Ok(3 * GB)).is_err());
        // Ended (any way at all): its state is dropped and its space is free.
        drop(first);
        assert!(space.reserve_with(dir, "Pixel", 3 * GB, five_gb_free).is_ok());
    }

    #[test]
    fn a_resume_does_not_count_its_own_promise_twice_and_other_folders_dont_count() {
        let space = SpaceReservations::default();
        let dir = Path::new("/received");
        let own = space.reserve_with(dir, "Pixel", 3 * GB, || Ok(4 * GB)).expect("fits");
        own.store(2 * GB, Ordering::Relaxed);
        // 1 GB written, 3 GB free now, 2 GB to go: fits, as its own 2 GB
        // promise is what it's asking for.
        assert_eq!(space.check_with(dir, "Pixel", 2 * GB, 2 * GB, || Ok(3 * GB)), None);
        // A file going to a folder on another volume takes nothing here.
        assert!(space.reserve_with(Path::new("/elsewhere"), "Pixel", 3 * GB, || Ok(4 * GB)).is_ok());
        drop(own);
    }

    #[test]
    fn a_volume_that_cant_report_its_free_space_is_not_refused() {
        let space = SpaceReservations::default();
        let unreadable = || Err(std::io::Error::other("not supported"));
        assert!(space.reserve_with(Path::new("/received"), "Pixel", 3 * GB, unreadable).is_ok());
    }

    #[test]
    fn byte_counts_read_like_a_file_manager() {
        assert_eq!(format_bytes(512), "512 bytes");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(64 * 1024 * 1024), "67.1 MB");
        assert_eq!(format_bytes(300_000_000), "300 MB");
        assert_eq!(format_bytes(2_000_000_000), "2 GB");
        assert_eq!(format_bytes(1_234_567_890_123), "1.2 TB");
    }

    #[test]
    fn part_files_skip_taken_names_and_existing_part_files() {
        let dir = std::env::temp_dir().join(format!("continuity-part-test-{:08x}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("photo.jpg"), b"already here").unwrap();
        std::fs::write(dir.join("photo (1).jpg.part"), b"left over").unwrap();

        let (part, _file) = create_part_file(&dir, "photo.jpg").unwrap();
        assert_eq!(part, dir.join("photo (2).jpg.part"));
        // Nothing that was already there was touched.
        assert_eq!(std::fs::read(dir.join("photo (1).jpg.part")).unwrap(), b"left over");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_usable_dir_is_created_and_a_file_in_place_of_one_is_not() {
        let base = std::env::temp_dir().join(format!("continuity-dir-test-{:08x}", rand::random::<u32>()));
        let nested = base.join("a").join("b");
        assert_eq!(check_usable_dir(&nested), Ok(()));
        assert!(nested.is_dir());
        // The write probe cleans up after itself.
        assert_eq!(std::fs::read_dir(&nested).unwrap().count(), 0);

        let file = base.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(check_usable_dir(&file).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    async fn confirm_over_a_slow_sender(claimed_hash: &str) -> (bool, Option<String>) {
        let (receiver_end, mut sender_end) = tokio::io::duplex(64 * 1024);
        let confirm = tokio::spawn(confirm_already_complete(receiver_end, "t".to_string(), "the hash".to_string()));
        // The sender re-hashing a big file on slow storage before it sends
        // anything — well past `VERIFY_TIMEOUT`, in paused time.
        tokio::time::sleep(VERIFY_TIMEOUT * 3).await;
        write_frame_unflushed(&mut sender_end, &[]).await.unwrap();
        let complete = Message::FileComplete { transfer_id: "t".to_string(), content_hash: claimed_hash.to_string() };
        write_message(&mut sender_end, &complete).await.unwrap();
        let verdict = read_message(&mut sender_end).await.unwrap();
        confirm.await.unwrap();
        match verdict {
            Message::FileVerified { ok, reason, .. } => (ok, reason),
            other => panic!("expected FileVerified, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_transfer_is_confirmed_however_long_the_sender_takes_to_rehash() {
        let (ok, reason) = confirm_over_a_slow_sender("the hash").await;
        assert!(ok, "refused: {reason:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_transfer_with_another_hash_is_still_refused() {
        let (ok, reason) = confirm_over_a_slow_sender("some other hash").await;
        assert!(!ok);
        assert_eq!(reason.as_deref(), Some("this transfer already finished here with different contents"));
    }

    #[test]
    fn retries_back_off_exponentially_and_cap() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(5), Duration::from_secs(16));
        assert_eq!(retry_delay(9), Duration::from_secs(16));
    }
}

//! The desktop app's log file (see `main.rs::init_logging`), kept to a
//! bounded size.
//!
//! **Real bug fixed here**: the log was only ever appended to, so it grew
//! without end — 5 and 6 GB on two Macs, about 100 MB again within three
//! hours of one being cleared (most of it one mDNS event repeated; see the
//! discovery loop in `continuity_daemon::engine`). Now, once the file
//! reaches `MAX_LOG_BYTES`, it starts over and what it had is kept as one
//! older file beside it (`continuityd.old.log`), so the two together stay
//! under twice that.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// How big the log gets before it starts over. Enough for days of normal
/// use at debug level.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

pub struct LogFile {
    path: PathBuf,
    file: File,
    /// The file's length as far as this knows: what it had when opened,
    /// plus what's been written since.
    len: u64,
    limit: u64,
}

impl LogFile {
    /// Opens `path` for appending. A file that's already at the limit
    /// starts over first; one far past it (left by a version without the
    /// limit) keeps only its last `MAX_LOG_BYTES` as the older file.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_limit(path, MAX_LOG_BYTES)
    }

    fn open_with_limit(path: &Path, limit: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let len = file.metadata()?.len();
        let mut log = Self { path: path.to_path_buf(), file, len, limit };
        if log.len >= limit {
            log.start_over()?;
        }
        Ok(log)
    }

    /// Moves what the file has to the older file and carries on in an
    /// empty one at the same path. If the file can't be moved (on Windows,
    /// something else may have it open without allowing that), it's
    /// emptied instead — the limit holds either way.
    fn start_over(&mut self) -> io::Result<()> {
        let old = old_path(&self.path);
        let _ = fs::remove_file(&old);
        let kept = if self.len > 2 * self.limit { keep_tail(&self.path, &old, self.limit) } else { fs::rename(&self.path, &old) };
        self.file = match kept {
            Ok(()) => OpenOptions::new().create(true).append(true).open(&self.path)?,
            Err(_) => OpenOptions::new().create(true).write(true).truncate(true).open(&self.path)?,
        };
        self.len = 0;
        Ok(())
    }
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.len >= self.limit {
            self.start_over()?;
        }
        let written = self.file.write(buf)?;
        self.len += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// `continuityd.log` → `continuityd.old.log` (and `continuityd.a.log` →
/// `continuityd.a.old.log` for profile `a`).
pub fn old_path(path: &Path) -> PathBuf {
    path.with_extension("old.log")
}

/// Writes the last `keep` bytes of `path` — from the first whole line in
/// them — to `old`, then removes `path`.
fn keep_tail(path: &Path, old: &Path, keep: u64) -> io::Result<()> {
    let mut source = File::open(path)?;
    let len = source.metadata()?.len();
    source.seek(SeekFrom::Start(len.saturating_sub(keep)))?;
    let mut tail = Vec::with_capacity(keep as usize);
    (&mut source).take(keep).read_to_end(&mut tail)?;
    let start = tail.iter().position(|&b| b == b'\n').map_or(0, |newline| newline + 1);
    fs::write(old, &tail[start..])?;
    drop(source);
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize) -> String {
        format!("line {n:04} {}\n", "x".repeat(40))
    }

    #[test]
    fn a_full_log_starts_over_and_keeps_one_older_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("continuityd.log");
        let mut log = LogFile::open_with_limit(&path, 1000).unwrap();
        for n in 0..100 {
            log.write_all(line(n).as_bytes()).unwrap();
        }
        let current = fs::read_to_string(&path).unwrap();
        let old = fs::read_to_string(old_path(&path)).unwrap();
        assert!(current.len() as u64 <= 1000 + line(0).len() as u64, "{} bytes", current.len());
        assert!(current.ends_with(&line(99)));
        assert!(old.len() as u64 <= 1000 + line(0).len() as u64);
        // The older file is the one just before, not anything earlier.
        let first_current: usize = current[5..9].parse().unwrap();
        assert!(old.ends_with(&line(first_current - 1)));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2, "only the log and one older file");
    }

    #[test]
    fn a_huge_log_from_before_the_limit_keeps_only_its_last_part() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("continuityd.log");
        let huge: String = (0..1000).map(line).collect();
        fs::write(&path, &huge).unwrap();
        fs::write(old_path(&path), "an even older file").unwrap();

        let mut log = LogFile::open_with_limit(&path, 1000).unwrap();
        log.write_all(b"started\n").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "started\n");
        let old = fs::read_to_string(old_path(&path)).unwrap();
        assert!(old.len() <= 1000);
        assert!(old.starts_with("line "), "starts at a whole line: {:?}", &old[..20]);
        assert!(old.ends_with(&line(999)));
    }

    #[test]
    fn a_log_under_the_limit_is_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("continuityd.log");
        fs::write(&path, "earlier\n").unwrap();
        let mut log = LogFile::open_with_limit(&path, 1000).unwrap();
        log.write_all(b"now\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "earlier\nnow\n");
        assert!(!old_path(&path).exists());
    }

    #[test]
    fn the_older_file_sits_beside_the_log() {
        assert_eq!(old_path(Path::new("/x/continuityd.log")), Path::new("/x/continuityd.old.log"));
        assert_eq!(old_path(Path::new("/x/continuityd.a.log")), Path::new("/x/continuityd.a.old.log"));
    }
}

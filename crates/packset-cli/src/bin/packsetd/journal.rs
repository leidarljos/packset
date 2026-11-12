//! Small writes admitted while the host is short of memory.
//!
//! Each record is a length-prefixed JSON object. The append is flushed
//! and synced before the caller is told the write was stored, so a crash
//! keeps it. The file has a byte cap. Past the cap the append is refused
//! and the request is answered 503. Replay upserts each record; writing
//! the same atom twice stores it once.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Eight mebibytes of lesson appends. One remember is a few hundred
/// bytes, so a burst fits and a further one waits instead of growing.
pub const DEFAULT_CAP: usize = 8 * 1024 * 1024;

/// The append would pass the cap. Nothing was written.
#[derive(Debug)]
pub struct Full;

impl std::fmt::Display for Full {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the append log is full; try again")
    }
}

impl std::error::Error for Full {}

/// The on-disk log and how full it is.
pub struct AppendLog {
    file: File,
    path: PathBuf,
    cap: usize,
    len: u64,
}

impl AppendLog {
    /// Open or create the log at `path`. A torn tail is cut back to the
    /// last complete record.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be created or read.
    pub fn open(path: impl Into<PathBuf>, cap: usize) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        let mut log = Self {
            file,
            path,
            cap: cap.max(1),
            len: 0,
        };
        log.recover()?;
        Ok(log)
    }

    #[must_use]
    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        usize::try_from(self.len).unwrap_or(usize::MAX)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Drop a trailing partial record left by a crash.
    fn recover(&mut self) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut good = 0u64;
        loop {
            let mut head = [0u8; 4];
            match self.file.read_exact(&mut head) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(err),
            }
            let n = u64::from(u32::from_le_bytes(head));
            let pos = self.file.stream_position()?;
            let end = self.file.seek(SeekFrom::End(0))?;
            if pos.saturating_add(n) > end {
                break;
            }
            self.file.seek(SeekFrom::Start(pos + n))?;
            good = pos + n;
        }
        self.file.set_len(good)?;
        self.len = good;
        self.file.seek(SeekFrom::Start(good))?;
        Ok(())
    }

    /// Append `record` and sync it. [`Full`] when it would pass the cap,
    /// and the file is left as it was.
    ///
    /// # Errors
    ///
    /// [`Full`] when the cap would be passed, or the io error from the
    /// write. A failed write truncates back to the previous length.
    pub fn append(&mut self, record: &[u8]) -> Result<(), AppendFailure> {
        let n = u32::try_from(record.len()).map_err(|_| AppendFailure::Full(Full))?;
        let add = 4u64.saturating_add(u64::from(n));
        if self.len.saturating_add(add) > self.cap as u64 {
            return Err(AppendFailure::Full(Full));
        }
        let prior = self.len;
        let write = (|| {
            self.file.write_all(&n.to_le_bytes())?;
            self.file.write_all(record)?;
            self.file.flush()?;
            self.file.sync_all()?;
            Ok(())
        })();
        if let Err(err) = write {
            let _ = self.file.set_len(prior);
            let _ = self.file.seek(SeekFrom::Start(prior));
            self.len = prior;
            return Err(AppendFailure::Io(err));
        }
        self.len = prior.saturating_add(add);
        Ok(())
    }

    /// The records in the file, in order.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read.
    pub fn records(&mut self) -> std::io::Result<Vec<Vec<u8>>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut out = Vec::new();
        let mut pos = 0u64;
        while pos.saturating_add(4) <= self.len {
            let mut head = [0u8; 4];
            self.file.read_exact(&mut head)?;
            let n = u32::from_le_bytes(head) as usize;
            let mut buf = vec![0u8; n];
            self.file.read_exact(&mut buf)?;
            out.push(buf);
            pos = pos.saturating_add(4).saturating_add(n as u64);
        }
        self.file.seek(SeekFrom::Start(self.len))?;
        Ok(out)
    }

    /// Cut the file back to `prior` bytes. A store write that failed after
    /// [`Self::append`] uses this so the log does not keep a record the
    /// database refused.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be truncated.
    pub fn rewind(&mut self, prior: usize) -> std::io::Result<()> {
        let prior = prior as u64;
        if prior >= self.len {
            return Ok(());
        }
        self.file.set_len(prior)?;
        self.file.seek(SeekFrom::Start(prior))?;
        self.file.sync_all()?;
        self.len = prior;
        Ok(())
    }

    /// Forget every record. The caller has applied them.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be truncated.
    pub fn clear(&mut self) -> std::io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        self.len = 0;
        Ok(())
    }
}

/// Why an append did not land.
#[derive(Debug)]
pub enum AppendFailure {
    Full(Full),
    Io(std::io::Error),
}

impl std::fmt::Display for AppendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(full) => full.fmt(f),
            Self::Io(err) => write!(f, "the append log could not be written: {err}"),
        }
    }
}

impl std::error::Error for AppendFailure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_log_refuses_and_a_replay_reads_what_was_synced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("append.log");
        let mut log = AppendLog::open(&path, 32).unwrap();
        log.append(b"one").unwrap();
        assert!(log.append(b"this record is far too long").is_err());
        assert_eq!(log.records().unwrap(), vec![b"one".to_vec()]);
        log.append(b"two").unwrap();
        assert_eq!(
            log.records().unwrap(),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
        drop(log);
        // A torn tail is not a record.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[4, 0, 0, 0, b'x']).unwrap();
        drop(file);
        let mut again = AppendLog::open(&path, 32).unwrap();
        assert_eq!(
            again.records().unwrap(),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
        again.clear().unwrap();
        assert!(again.is_empty());
        let mut reopened = AppendLog::open(&path, 32).unwrap();
        assert!(reopened.records().unwrap().is_empty());
    }
}

//! Advisory filesystem lock on `<root>/.lock` — prevents two concurrent `sync`
//! processes against the same mirror root.
//!
//! Backed by `flock(2)` (via the `fs2` crate). The lock is held for the lifetime
//! of the [`MirrorLock`] handle and auto-released when the handle drops (and also
//! by the kernel when the process exits — even on crash). The PID of the holder
//! is written into the file so a contending process can surface it in the error.

use std::fs::{File, OpenOptions};
use std::io::{Seek, Write};
use std::path::Path;

use fs2::FileExt;

use crate::{Error, Result};

const LOCK_FILENAME: &str = ".lock";

#[derive(Debug)]
pub struct MirrorLock {
    file: File,
}

impl MirrorLock {
    /// Acquire an exclusive non-blocking flock on `<root>/.lock`, creating the
    /// file if absent. On contention, reads the existing PID and returns
    /// [`Error::Locked`].
    pub fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(LOCK_FILENAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                file.set_len(0)?;
                file.seek(std::io::SeekFrom::Start(0))?;
                writeln!(file, "{}", std::process::id())?;
                file.sync_all()?;
                Ok(Self { file })
            }
            Err(_) => {
                let holder_pid = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                Err(Error::Locked { holder_pid })
            }
        }
    }
}

impl Drop for MirrorLock {
    fn drop(&mut self) {
        // Kernel releases the flock when the fd closes, but unlock explicitly for clarity.
        // Leave the pidfile in place — removing it racily between acquire and flock would
        // let two processes briefly both hold "the lock" on different inodes.
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_acquire_then_contend() {
        let td = TempDir::new().unwrap();
        let lock1 = MirrorLock::acquire(td.path()).unwrap();
        let err = MirrorLock::acquire(td.path()).unwrap_err();
        match err {
            Error::Locked { holder_pid } => {
                assert_eq!(holder_pid, std::process::id());
            }
            other => panic!("expected Locked, got {other:?}"),
        }
        drop(lock1);
        // After release, a fresh acquire succeeds.
        let _lock2 = MirrorLock::acquire(td.path()).unwrap();
    }
}

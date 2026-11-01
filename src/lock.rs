//! Advisory lock, so overlapping runs never work on the same state at once.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Held until dropped.
#[derive(Debug)]
pub struct LibraryLock {
    _file: File,
    pub path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another tidepool is running{}; try again when it's done", .pid.map(|p| format!(" (pid {p})")).unwrap_or_default())]
    Held { pid: Option<u32> },
    #[error("cannot lock {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Take the lock at `path`, waiting for any other holder to finish.
pub fn acquire_blocking(path: &Path) -> Result<LibraryLock, LockError> {
    let io = |source| LockError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io)?;
    file.lock().map_err(io)?;
    record_pid(&mut file).map_err(io)?;
    Ok(LibraryLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

fn record_pid(file: &mut File) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{}", std::process::id())?;
    file.flush()
}

/// Take the lock at `path` without blocking, recording our PID in it.
pub fn acquire(path: &Path) -> Result<LibraryLock, LockError> {
    let io = |source| LockError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    // Don't truncate before locking: the holder's PID must stay readable.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut text = String::new();
            let _ = file.read_to_string(&mut text);
            return Err(LockError::Held {
                pid: text.trim().parse().ok(),
            });
        }
        Err(TryLockError::Error(e)) => return Err(io(e)),
    }
    record_pid(&mut file).map_err(io)?;
    Ok(LibraryLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_reports_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib/lock");
        let held = acquire(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
        match acquire(&path) {
            Err(LockError::Held { pid }) => assert_eq!(pid, Some(std::process::id())),
            other => panic!("expected Held, got {other:?}"),
        }
        drop(held);
        // Other tests spawn processes; a child holds a copy of the lock's file
        // descriptor until it execs, so the release can lag a moment.
        let released = (0..100).any(|_| {
            acquire(&path).is_ok() || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }
        });
        assert!(released, "released on drop");
    }
}

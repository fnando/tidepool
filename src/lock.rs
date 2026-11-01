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

/// The PID holding the lock at `path`, if anyone does (without taking it
/// away from anyone: only a shared hold, let go right away).
pub fn holder(path: &Path) -> Option<u32> {
    let mut file = OpenOptions::new().read(true).open(path).ok()?;
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            None
        }
        Err(_) => {
            let mut text = String::new();
            let _ = file.read_to_string(&mut text);
            Some(text.trim().parse().unwrap_or(0))
        }
    }
}

/// The library lock shared by the daemon's services: the first to take it
/// locks the file, the last to let go unlocks it. Services in the same
/// process never exclude each other; other processes are still excluded.
#[derive(Debug)]
pub struct Shared {
    path: PathBuf,
    held: std::sync::Mutex<(usize, Option<LibraryLock>)>,
}

/// One service's hold on a [`Shared`] lock, until dropped.
#[derive(Debug)]
pub struct SharedGuard {
    shared: std::sync::Arc<Shared>,
}

impl Shared {
    pub fn new(path: &Path) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            path: path.to_path_buf(),
            held: std::sync::Mutex::new((0, None)),
        })
    }

    /// Hold it, locking the file if nobody in this process does yet.
    pub fn acquire(self: &std::sync::Arc<Self>) -> Result<SharedGuard, LockError> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if held.0 == 0 {
            held.1 = Some(acquire(&self.path)?);
        }
        held.0 += 1;
        Ok(SharedGuard {
            shared: self.clone(),
        })
    }
}

impl Drop for SharedGuard {
    fn drop(&mut self) {
        let mut held = self.shared.held.lock().unwrap_or_else(|e| e.into_inner());
        held.0 -= 1;
        if held.0 == 0 {
            held.1 = None;
        }
    }
}

/// A hold on the library: tidepool's own lock, or a share of the daemon's.
#[derive(Debug)]
pub enum Hold {
    Own(LibraryLock),
    Shared(SharedGuard),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_who_holds_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        assert_eq!(holder(&path), None, "no file yet");
        let held = acquire(&path).unwrap();
        assert_eq!(holder(&path), Some(std::process::id()));
        drop(held);
        let free = (0..100).any(|_| {
            holder(&path).is_none() || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }
        });
        assert!(free);
    }

    #[test]
    fn a_shared_lock_is_held_while_anyone_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let shared = Shared::new(&path);
        let a = shared.acquire().unwrap();
        let b = shared.acquire().unwrap();
        assert!(matches!(acquire(&path), Err(LockError::Held { .. })));
        drop(a);
        assert!(
            matches!(acquire(&path), Err(LockError::Held { .. })),
            "b still holds it"
        );
        drop(b);
        // A spawned child may hold a copy of the descriptor for a moment.
        let released = (0..100).any(|_| {
            acquire(&path).is_ok() || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }
        });
        assert!(released, "released by the last one");
    }

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

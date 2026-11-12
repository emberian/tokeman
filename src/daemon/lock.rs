//! The rotation lock serializes every change to Claude's settings and to
//! rotation state, across the daemon and every CLI command.

use anyhow::Result;

use crate::fs::FileLock;
use crate::paths;

pub struct RotationLock {
    _lock: FileLock,
}

impl RotationLock {
    /// Blocks until held. A daemon cycle can own the lock for a network
    /// timeout, so a command that has to wait says so instead of looking hung.
    pub fn acquire() -> Result<Self> {
        Ok(Self {
            _lock: FileLock::acquire_announced(
                &paths::rotation_lock(),
                "the rotation lock (another tokeman holds it, usually the daemon mid-cycle)",
            )?,
        })
    }
}

/// The daemon's lifetime lock. A surface's liveness check holds the file
/// shared for an instant, so taking it retries briefly before concluding
/// that another daemon is running.
pub struct DaemonLock {
    _lock: FileLock,
}

impl DaemonLock {
    const ATTEMPTS: u32 = 50;
    const RETRY: std::time::Duration = std::time::Duration::from_millis(100);

    /// `None` when another daemon holds it.
    pub fn take() -> Result<Option<Self>> {
        Self::take_at(&paths::daemon_lock())
    }

    pub fn take_at(path: &std::path::Path) -> Result<Option<Self>> {
        for attempt in 0..Self::ATTEMPTS {
            if let Some(lock) = FileLock::try_acquire(path)? {
                return Ok(Some(Self { _lock: lock }));
            }
            if attempt + 1 < Self::ATTEMPTS {
                std::thread::sleep(Self::RETRY);
            }
        }
        Ok(None)
    }
}

/// Whether a daemon is running: its lifetime lock is held.
pub fn daemon_alive() -> bool {
    FileLock::is_held(&paths::daemon_lock())
}

/// Whether rotation is paused (`tokeman rotate pause`).
pub fn is_paused() -> bool {
    paths::pause_marker().exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_lock_is_held_for_its_lifetime_and_admits_one_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        assert!(!FileLock::is_held(&path), "a missing file is not held");
        assert!(!path.exists(), "the check creates nothing");

        let lock = DaemonLock::take_at(&path).unwrap().expect("first daemon");
        assert!(FileLock::is_held(&path));
        assert!(
            FileLock::try_acquire(&path).unwrap().is_none(),
            "a second daemon cannot take it"
        );
        drop(lock);
        assert!(!FileLock::is_held(&path), "released when the daemon exits");
    }
}

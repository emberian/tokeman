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

/// Whether rotation is paused (`tokeman rotate pause`).
pub fn is_paused() -> bool {
    paths::pause_marker().exists()
}

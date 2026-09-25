//! Owner-only files. Everything tokeman persists either is a credential or is
//! derived from one, so every write goes through here: created 0700/0600,
//! replaced atomically, and serialized with an advisory lock where two
//! tokeman processes can race.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Directory holding `tokens.toml` and every piece of tokeman state.
pub fn state_dir() -> Result<PathBuf> {
    crate::config::Config::path()?
        .parent()
        .map(Path::to_path_buf)
        .context("tokeman config path has no parent")
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

pub fn ensure_private_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        set_mode(parent, 0o700)?;
    }
    Ok(())
}

/// Replace `path` with `bytes` so a reader never sees a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    ensure_private_parent(path)?;
    let parent = path.parent().context("output path has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    set_mode(temp.path(), mode)?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    set_mode(path, mode)
}

/// Append one line to an owner-only file, creating it if needed.
pub fn append_line(path: &Path, line: &str) -> Result<()> {
    ensure_private_parent(path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    set_mode(path, 0o600)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// An exclusive advisory lock, released on drop.
pub struct FileLock {
    _file: File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        use fs2::FileExt;
        ensure_private_parent(path)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("failed to open lock {}", path.display()))?;
        set_mode(path, 0o600)?;
        file.lock_exclusive()
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

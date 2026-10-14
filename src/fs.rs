//! Owner-only files. Everything tokeman persists is a credential or derived
//! from one: directories are 0700, files 0600, replacements atomic, and
//! anything two processes can write is serialized with an advisory lock.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

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

/// Create the parent directory, owner-only.
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

/// Read a file that may not exist yet.
pub fn read_if_present(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// An exclusive advisory lock, released on drop.
pub struct FileLock {
    _file: File,
}

impl FileLock {
    /// Block until the lock is held.
    pub fn acquire(path: &Path) -> Result<Self> {
        use fs2::FileExt;
        let file = open_lock_file(path)?;
        file.lock_exclusive()
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(Self { _file: file })
    }

    /// Take the lock only if nobody holds it; `None` means another process does.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        use fs2::FileExt;
        let file = open_lock_file(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == fs2::lock_contended_error().kind() => Ok(None),
            Err(error) => Err(error).with_context(|| format!("failed to lock {}", path.display())),
        }
    }

    /// Take the lock, saying so on stderr when another process holds it, so a
    /// command that has to wait never looks hung.
    pub fn acquire_announced(path: &Path, holder: &str) -> Result<Self> {
        if let Some(lock) = Self::try_acquire(path)? {
            return Ok(lock);
        }
        eprintln!("waiting for {holder}");
        Self::acquire(path)
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    ensure_private_parent(path)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open lock {}", path.display()))?;
    set_mode(path, 0o600)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_is_seen_as_contended_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = FileLock::acquire(&path).unwrap();
        assert!(FileLock::try_acquire(&path).unwrap().is_none());
        drop(held);
        assert!(FileLock::try_acquire(&path).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writes_land_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inner").join("f.txt");
        write_atomic(&path, b"hi", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hi");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(read_if_present(&dir.path().join("missing")).unwrap(), None);
    }
}

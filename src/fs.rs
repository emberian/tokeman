//! Owner-only files. Everything tokeman persists is a credential or derived
//! from one: directories are 0700, files 0600, replacements atomic, and
//! anything two processes can write is serialized with an advisory lock.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const MAX_SYMLINK_HOPS: usize = 40;

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

/// Follow up to 40 symbolic links at `path` so atomic replacements update
/// the link's target instead of clobbering the link.
pub fn resolve_link_chain(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    for hop in 0..=MAX_SYMLINK_HOPS {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to stat {}", current.display()));
            }
        };
        if !meta.file_type().is_symlink() {
            if hop > 0 && meta.permissions().readonly() {
                bail!(
                    "refusing to write through symlink {} to read-only file {}",
                    path.display(),
                    current.display()
                );
            }
            return Ok(current);
        }
        if hop == MAX_SYMLINK_HOPS {
            bail!("too many symbolic link levels resolving {}", path.display());
        }
        let target = std::fs::read_link(&current)
            .with_context(|| format!("failed to read symlink {}", current.display()))?;
        current = if target.is_absolute() {
            target
        } else {
            match current.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.join(target),
                _ => target,
            }
        };
    }
    unreachable!()
}

fn ensure_parent_for_mode(path: &Path, file_mode: u32) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    let existed = parent.exists();
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let private_file = file_mode & 0o077 == 0;
    if !existed {
        set_mode(parent, if private_file { 0o700 } else { 0o755 })?;
    } else if private_file && crate::paths::is_private_dir(parent) {
        set_mode(parent, 0o700)?;
    }
    Ok(())
}

/// Create the parent directory, owner-only when newly created or inside
/// tokeman's own state trees.
pub fn ensure_private_parent(path: &Path) -> Result<()> {
    ensure_parent_for_mode(path, 0o600)
}

/// Replace `path` (or its symlink target) with `bytes` so a reader never sees
/// a partial file and a dotfile symlink is never replaced by a regular file.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let resolved = resolve_link_chain(path)?;
    ensure_parent_for_mode(&resolved, mode)?;
    let parent = match resolved.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    set_mode(temp.path(), mode)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(&resolved)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", resolved.display()))?;
    set_mode(&resolved, mode)
}

/// Atomically replace `link_path` with a symbolic link pointing to `target`.
#[cfg(unix)]
pub fn write_symlink_atomic(link_path: &Path, target: &Path) -> Result<()> {
    ensure_parent_for_mode(link_path, 0o600)?;
    let parent = match link_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let scratch = tempfile::Builder::new()
        .prefix(".tokeman-link-")
        .tempdir_in(parent)
        .with_context(|| format!("failed to create staging directory in {}", parent.display()))?;
    let staged = scratch.path().join("link");
    std::os::unix::fs::symlink(target, &staged).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })?;
    std::fs::rename(&staged, link_path)
        .with_context(|| format!("failed to replace {}", link_path.display()))
}

/// Append one line to an owner-only file in a single write, healing any
/// missing trailing newline left by an earlier interrupted write.
pub fn append_line(path: &Path, line: &str) -> Result<()> {
    let resolved = resolve_link_chain(path)?;
    ensure_private_parent(&resolved)?;
    let mut options = OpenOptions::new();
    options.read(true).create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&resolved)
        .with_context(|| format!("failed to open {}", resolved.display()))?;
    set_mode(&resolved, 0o600)?;
    let needs_leading_newline = match file.metadata() {
        Ok(meta) if meta.len() > 0 => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                let mut last = [0u8; 1];
                file.read_at(&mut last, meta.len() - 1)
                    .is_ok_and(|n| n == 1 && last[0] != b'\n')
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
        _ => false,
    };
    let mut buf = Vec::with_capacity(line.len() + 2);
    if needs_leading_newline {
        buf.push(b'\n');
    }
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    file.write_all(&buf)?;
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

    /// Whether another process holds the lock at `path` exclusively. Opens
    /// the file read-only and never creates it: a missing file is not held.
    pub fn is_held(path: &Path) -> bool {
        let Ok(file) = File::open(path) else {
            return false;
        };
        match fs2::FileExt::try_lock_shared(&file) {
            Ok(()) => false,
            Err(error) => error.kind() == fs2::lock_contended_error().kind(),
        }
    }

    /// Take the lock, saying so (a notice) when another process holds it, so a
    /// command that has to wait never looks hung.
    pub fn acquire_announced(path: &Path, holder: &str) -> Result<Self> {
        if let Some(lock) = Self::try_acquire(path)? {
            return Ok(lock);
        }
        crate::notice::notice(&format!("waiting for {holder}"));
        Self::acquire(path)
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    let resolved = resolve_link_chain(path)?;
    ensure_private_parent(&resolved)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&resolved)
        .with_context(|| format!("failed to open lock {}", resolved.display()))?;
    set_mode(&resolved, 0o600)?;
    Ok(file)
}

/// Deserialize a map entry by entry and leave out the entries that do not
/// parse. A state file outlives the build that wrote it: one entry in an
/// older or newer shape costs that entry, not the file.
pub fn lenient_map<'de, D, T>(
    deserializer: D,
) -> Result<std::collections::BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    use serde::Deserialize;
    let raw = std::collections::BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|(key, value)| Some((key, serde_json::from_value(value).ok()?)))
        .collect())
}

/// [`lenient_map`] for a list.
pub fn lenient_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    use serde::Deserialize;
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
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
        // Only a missing file reads as absent.
        assert!(read_if_present(dir.path()).is_err());

        let log = dir.path().join("log").join("events.jsonl");
        append_line(&log, "one").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        append_line(&log, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        assert_eq!(mode(&log), 0o600);
        assert_eq!(mode(log.parent().unwrap()), 0o700);

        // Symlinked files keep their symlink and update the target; healing a
        // truncated tail inserts a newline before the next JSONL record.
        let target = dir.path().join("real.json");
        std::fs::write(&target, b"old").unwrap();
        let link = dir.path().join("linked.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_atomic(&link, b"new", 0o600).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new");

        let partial_log = dir.path().join("partial.jsonl");
        std::fs::write(&partial_log, b"{\"partial\":true}").unwrap();
        append_line(&partial_log, "{\"complete\":true}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&partial_log).unwrap(),
            "{\"partial\":true}\n{\"complete\":true}\n"
        );

        let sym = dir.path().join("atomic-link");
        write_symlink_atomic(&sym, &target).unwrap();
        assert_eq!(std::fs::read_link(&sym).unwrap(), target);
    }
}

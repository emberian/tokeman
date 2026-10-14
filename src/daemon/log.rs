//! The rotation log: one timestamped line per event, rotated past 4 MiB.
//! `history` parses its default-change lines for the period before the
//! credential history existed, so the line shapes `explicitly activated X`,
//! `switch A -> B; ...` and `rotation paused; ...` are part of its format.

use crate::fs::append_line;
use crate::paths;

const ROTATE_ABOVE_BYTES: u64 = 4 * 1024 * 1024;

/// Append one line. Tests never write to the real log.
pub fn log(message: &str) {
    if cfg!(test) {
        return;
    }
    let path = paths::rotation_log();
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > ROTATE_ABOVE_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let _ = append_line(&path, &format!("{stamp} {message}"));
}

/// Log, and print to stderr as well, for commands run by hand.
pub fn log_and_say(message: &str) {
    log(message);
    eprintln!("{message}");
}

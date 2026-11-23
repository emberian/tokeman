//! Progress lines for whoever is watching: a lock that is being waited on, a
//! retry that sleeps, what a dry run would have done, the daemon's log echo.
//!
//! The library never writes to the terminal. A surface installs a sink
//! ([`set_sink`]); until one is installed a notice goes nowhere.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static SINK: OnceLock<fn(&str)> = OnceLock::new();
static MUTED: AtomicBool = AtomicBool::new(false);

/// Route notices to `sink`. The first caller wins.
pub fn set_sink(sink: fn(&str)) {
    let _ = SINK.set(sink);
}

/// Silence notices while an alternate-screen TUI owns the terminal.
pub fn set_muted(muted: bool) {
    MUTED.store(muted, Ordering::Relaxed);
}

/// Say `message` to the installed sink.
pub fn notice(message: &str) {
    if !MUTED.load(Ordering::Relaxed)
        && let Some(sink) = SINK.get()
    {
        sink(message);
    }
}

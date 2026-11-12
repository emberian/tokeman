//! Progress lines for whoever is watching: a lock that is being waited on, a
//! retry that sleeps, what a dry run would have done, the daemon's log echo.
//!
//! The library never writes to the terminal. A surface installs a sink
//! ([`set_sink`]); until one is installed a notice goes nowhere.

use std::sync::OnceLock;

static SINK: OnceLock<fn(&str)> = OnceLock::new();

/// Route notices to `sink`. The first caller wins.
pub fn set_sink(sink: fn(&str)) {
    let _ = SINK.set(sink);
}

/// Say `message` to the installed sink.
pub fn notice(message: &str) {
    if let Some(sink) = SINK.get() {
        sink(message);
    }
}

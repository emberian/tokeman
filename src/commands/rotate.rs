//! `tokeman rotate ...`: one cycle, the status report, pausing, the
//! background service, quarantines, and the SessionStart hook's record.

use anyhow::{Result, bail};
use std::path::PathBuf;

use crate::account::Fleet;
use crate::daemon::{self, cycle, status};
use crate::observe::{self, Family, LimitWindow};
use crate::quota::ModelKey;

/// Probe and apply the policy once.
pub async fn run(fleet: &Fleet, options: cycle::Options) -> Result<cycle::Outcome> {
    cycle::run(fleet, options).await
}

/// The loop the background service runs.
pub async fn daemon() -> Result<()> {
    daemon::run().await
}

/// Probe and report the monitor, the default, live sessions and the mode.
pub async fn status(fleet: &Fleet) -> Result<status::Report> {
    status::report(fleet).await
}

pub fn pause() -> Result<()> {
    daemon::pause()
}

/// Resume managing and run one cycle.
pub async fn resume(fleet: &Fleet) -> Result<cycle::Outcome> {
    daemon::resume(fleet).await
}

/// Install and start the background service; the plist it wrote. Also
/// rewrites the launcher script and the SessionStart hook for this binary,
/// which is what repairs an install made by an earlier build.
pub fn install() -> Result<PathBuf> {
    daemon::install()
}

pub fn uninstall() -> Result<()> {
    daemon::uninstall()
}

/// A capacity rejection tokeman did not observe itself.
#[derive(Debug, Clone)]
pub struct Quarantine {
    pub family: Family,
    pub exact_model: Option<String>,
    pub window: LimitWindow,
    /// RFC 3339 or a Unix timestamp.
    pub until: String,
}

/// Record the rejection, then run one cycle so the default moves off the
/// account if it has to.
pub async fn quarantine(
    fleet: &Fleet,
    name: &str,
    quarantine: Quarantine,
) -> Result<cycle::Outcome> {
    if fleet.account(name).is_none() {
        bail!("account '{name}' is not configured");
    }
    observe::quarantine(
        name,
        quarantine.family,
        quarantine.exact_model.as_deref().map(ModelKey::new),
        quarantine.window,
        observe::parse_reset_spec(&quarantine.until)?,
    )?;
    cycle::run(fleet, cycle::Options::default()).await
}

/// Remove observed and manual quarantines; how many were removed.
pub fn clear_quarantine(name: &str, family: Option<Family>) -> Result<usize> {
    observe::clear_quarantine(name, family)
}

/// The SessionStart hook: record which account the Claude process `pid`
/// spends. `input` is the hook's JSON, as it arrived on stdin.
pub async fn observe_session(pid: u32, input: &[u8]) -> Result<()> {
    observe::record_session_from_hook(pid, input).await
}

//! `tokeman rotate ...`: one cycle, the status view, pausing, the
//! background service, quarantines, and the SessionStart hook's record.

use anyhow::{Result, bail};
use std::path::PathBuf;

use crate::account::Fleet;
use crate::anthropic::profile::Cache;
use crate::anthropic::{Client, probe, resets};
use crate::claude::{sessions, settings as claude_settings};
use crate::daemon::cadence::Cadence;
use crate::daemon::lock::RotationLock;
use crate::daemon::{self, cycle};
use crate::observe::{self, Family, LimitWindow};
use crate::policy::{self, Mode};
use crate::quota::{ModelKey, Source};
use crate::ui::overview::{Facts, Overview, ResetOffer};

/// Probe and apply the policy once.
pub async fn run(fleet: &Fleet, options: cycle::Options) -> Result<cycle::Outcome> {
    cycle::run(fleet, options).await
}

/// The loop the background service runs.
pub async fn daemon() -> Result<()> {
    daemon::run().await
}

/// `tokeman rotate status`: read every Claude account the way a cycle
/// does (its backoff, its target-model samples, the rejections on record),
/// read the reset offers, take a census of live sessions, and report the
/// fleet view with the mode and floors the next cycle would use. Nothing is
/// decided or installed.
pub async fn status(fleet: &Fleet) -> Result<Overview> {
    fleet.policy.validate()?;
    let _lock = RotationLock::acquire()?;
    let settings = claude_settings::load()?;
    let target_model = claude_settings::target_model(&settings).map(str::to_owned);
    let model = target_model.as_deref().map(ModelKey::new);
    let _ = observe::scan_transcripts(&fleet.accounts);
    let mut cadence = Cadence::load();
    let now = daemon::now_epoch();
    let readings = cycle::probe_fleet(
        fleet,
        &mut cadence,
        target_model.as_deref(),
        now,
        true,
        false,
        true,
    )
    .await;
    let mode = policy::safe_mode_for(
        &readings,
        fleet.accounts.len(),
        &fleet.policy,
        cadence.mode.unwrap_or(Mode::Normal),
        model.as_ref(),
    );
    let with_profile = readings
        .iter()
        .filter(|reading| {
            reading
                .models()
                .any(|limit| limit.source == Source::Profile)
        })
        .count();
    let live_sessions = match sessions::live().await {
        Ok(sessions) => Some(sessions),
        Err(error) => {
            daemon::log::log(&format!("WARNING process census failed: {error:#}"));
            None
        }
    };
    let at = now as i64;
    let mut facts = Facts::on_disk(fleet, at).with_fresh(readings);
    facts.mode = mode;
    facts.probe_interval_secs = None;
    facts.coverage = Some(format!(
        "profile limits {with_profile}/{} accounts; {} active rejection quarantine(s)",
        fleet.accounts.len(),
        facts.observed.len()
    ));
    facts.sampling = Some(describe_sampling(
        fleet,
        &cadence,
        target_model.as_deref(),
        now,
    ));
    facts.offers = resets_on_offer(fleet, at).await;
    facts.live_sessions = live_sessions;
    Ok(Overview::build(
        fleet,
        facts,
        Some(crate::quota::Service::Claude),
        at,
    ))
}

/// Whether and how the startup model's own limit is being sampled.
fn describe_sampling(
    fleet: &Fleet,
    cadence: &Cadence,
    target_model: Option<&str>,
    now: f64,
) -> String {
    let Some(model) = probe::sample_model(target_model) else {
        return "none for the startup model".into();
    };
    let interval = fleet.policy.target_model_sample_secs;
    if interval == 0 {
        return format!("{model} has one, but sampling is off (target_model_sample_secs = 0)");
    }
    let eligible: Vec<_> = fleet
        .accounts
        .iter()
        .filter(|account| account.profile_token((now * 1000.0) as i64).is_none())
        .filter(|account| !cadence.is_parked(account, now))
        .collect();
    if eligible.is_empty() {
        return format!("{model}: no account needs sampling");
    }
    let ages: Vec<f64> = eligible
        .iter()
        .filter_map(|account| cadence.samples.get(&account.name))
        .map(|sample| ((now - sample.sampled_at) / 60.0).max(0.0))
        .collect();
    let sampled = match ages.iter().copied().reduce(f64::max) {
        Some(oldest) => format!("{} sampled, oldest {oldest:.0}m ago", ages.len()),
        None => "none sampled yet".into(),
    };
    format!(
        "{model} probe every {}m on {} account(s) without profile reads ({sampled})",
        interval / 60,
        eligible.len()
    )
}

/// The usable reset grants of every logged-in account, read now.
async fn resets_on_offer(fleet: &Fleet, now: i64) -> Vec<ResetOffer> {
    let cache = Cache::default();
    let client = Client::shared();
    let accounts: Vec<_> = fleet
        .accounts
        .iter()
        .filter(|account| account.profile_token(now * 1000).is_some())
        .collect();
    let statuses = futures::future::join_all(
        accounts
            .iter()
            .map(|account| resets::status(client, &cache, account, false, now)),
    )
    .await;
    let mut offers = Vec::new();
    for (account, status) in accounts.iter().zip(statuses) {
        let Some(program) = status.ok().and_then(|status| status.cedar_ember) else {
            continue;
        };
        offers.extend(
            program
                .grants()
                .iter()
                .filter(|grant| grant.is_usable(now))
                .map(|grant| ResetOffer {
                    account: account.name.clone(),
                    offer: resets::describe_grant(grant, now),
                    read_at: now,
                }),
        );
    }
    offers
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

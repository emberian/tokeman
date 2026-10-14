//! `tokeman rotate status`: one probe and everything the daemon knows.

use anyhow::Result;
use serde::Serialize;

use super::cadence::Cadence;
use super::lock::{RotationLock, is_paused};
use super::log::log;
use super::{cycle, now_epoch, service};
use crate::account::Fleet;
use crate::anthropic::profile::Cache;
use crate::anthropic::{Client, probe, resets};
use crate::claude::{sessions, settings as claude_settings};
use crate::observe::{self, ObservedLimit};
use crate::policy::{self, Mode};
use crate::quota::{ModelKey, Source};

#[derive(Debug, Clone, Serialize)]
pub struct BucketReport {
    pub key: ModelKey,
    pub label: String,
    pub remaining: f64,
    pub resets_at: i64,
    pub source: Source,
    /// Whether this bucket limits the startup model.
    pub relevant: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountReport {
    pub name: String,
    pub remaining_session: Option<f64>,
    pub remaining_weekly: Option<f64>,
    pub buckets: Vec<BucketReport>,
    pub limits: Vec<ObservedLimit>,
    pub viable: bool,
    pub is_default: bool,
    pub error: Option<String>,
    /// Which credential the account offers Claude, without secrets.
    pub credential: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub monitor: &'static str,
    pub service_installed: bool,
    pub mode: Mode,
    pub probe_interval_secs: u64,
    pub default_account: Option<String>,
    pub default_is_managed: bool,
    pub target_model: Option<String>,
    pub min_session_remaining: f64,
    pub min_weekly_remaining: f64,
    /// How many accounts have profile buckets, and how many quarantines stand.
    pub coverage: String,
    /// Whether and how the startup model's own limit is being sampled.
    pub sampling: String,
    /// Usage-limit resets on offer, one line each.
    pub resets_on_offer: Vec<String>,
    /// `None` when the process census failed or timed out.
    pub live_sessions: Option<Vec<sessions::Session>>,
    pub accounts: Vec<AccountReport>,
}

pub async fn report(fleet: &Fleet) -> Result<Report> {
    fleet.policy.validate()?;
    let _lock = RotationLock::acquire()?;
    let settings = claude_settings::load()?;
    let target_model = claude_settings::target_model(&settings).map(str::to_owned);
    let model = target_model.as_deref().map(ModelKey::new);
    let current_value = claude_settings::configured_token(&settings);
    let current_name = current_value
        .and_then(|value| fleet.account_holding(value))
        .map(|account| account.name.clone());
    let _ = observe::scan_transcripts(&fleet.accounts);
    let mut cadence = Cadence::load();
    let now = now_epoch();
    let now_ms = chrono::Utc::now().timestamp_millis();
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
    let previous_mode = cadence.mode.unwrap_or(Mode::Normal);
    let mode = policy::safe_mode_for(
        &readings,
        fleet.accounts.len(),
        &fleet.policy,
        previous_mode,
        model.as_ref(),
    );
    let floors = fleet.policy.floors(mode);
    let limits = observe::active_limits();
    let profile_count = readings
        .iter()
        .filter(|reading| {
            reading
                .buckets
                .iter()
                .any(|bucket| bucket.source == Source::Profile)
        })
        .count();
    let accounts = readings
        .iter()
        .map(|reading| AccountReport {
            name: reading.account.clone(),
            remaining_session: reading.session.map(|w| w.remaining()),
            remaining_weekly: reading.weekly.map(|w| w.remaining()),
            buckets: reading
                .buckets
                .iter()
                .map(|bucket| BucketReport {
                    relevant: model
                        .as_ref()
                        .is_some_and(|model| model.is_limited_by(&bucket.key)),
                    key: bucket.key.clone(),
                    label: bucket.label.clone(),
                    remaining: bucket.window.remaining(),
                    resets_at: bucket.window.resets_at,
                    source: bucket.source,
                })
                .collect(),
            limits: limits
                .iter()
                .filter(|limit| limit.account == reading.account)
                .cloned()
                .collect(),
            viable: policy::viable(reading, mode, &fleet.policy, model.as_ref()),
            is_default: current_name.as_deref() == Some(reading.account.as_str()),
            error: reading.error.as_ref().map(ToString::to_string),
            credential: fleet
                .account(&reading.account)
                .map(|account| claude_settings::describe_credential(account, now_ms))
                .unwrap_or_default(),
        })
        .collect();
    let live_sessions = match sessions::live().await {
        Ok(sessions) => Some(sessions),
        Err(error) => {
            log(&format!("WARNING process census failed: {error:#}"));
            None
        }
    };
    Ok(Report {
        monitor: if is_paused() { "paused" } else { "running" },
        service_installed: service::is_installed(),
        mode,
        probe_interval_secs: fleet.policy.interval_secs(mode),
        default_account: current_name
            .or_else(|| current_value.map(|_| "unmanaged OAuth token".to_string())),
        default_is_managed: current_value
            .is_some_and(|value| fleet.account_holding(value).is_some()),
        target_model: target_model.clone(),
        min_session_remaining: floors.session,
        min_weekly_remaining: floors.weekly,
        coverage: format!(
            "profile buckets {profile_count}/{} accounts; {} active rejection quarantine(s)",
            fleet.accounts.len(),
            limits.len()
        ),
        sampling: describe_sampling(fleet, &cadence, target_model.as_deref(), now),
        resets_on_offer: resets_on_offer(fleet).await,
        live_sessions,
        accounts,
    })
}

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

async fn resets_on_offer(fleet: &Fleet) -> Vec<String> {
    let now = chrono::Utc::now().timestamp();
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
        for grant in program.grants() {
            if grant.resets_left > 0 && !grant.paused {
                offers.push(format!(
                    "{}: {}",
                    account.name,
                    resets::describe_grant(&grant)
                ));
            }
        }
    }
    offers
}

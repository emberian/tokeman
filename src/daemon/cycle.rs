//! One rotation cycle: observe, probe, decide, install, record.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use super::cadence::Cadence;
use super::lock::{RotationLock, is_paused};
use super::log::log;
use super::now_epoch;
use crate::account::Fleet;
use crate::anthropic::Client;
use crate::anthropic::probe::{self, PROBE_MODEL, PROBE_TIMEOUT};
use crate::claude::{keychain, settings as claude_settings};
use crate::history::EventKind;
use crate::observe;
use crate::policy::{self, Decision, Hold, Mode, Situation, Trigger};
use crate::quota::{ModelKey, Reading};
use crate::store::Store;

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Take the best account even over a healthy default.
    pub force: bool,
    /// Decide without writing anything; say what would have been written.
    pub dry_run: bool,
    /// A daemon tick: honours the cadence and skips the Keychain check.
    pub scheduled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Paused,
    /// The next probe was not due yet.
    NotDue,
    /// No account answered with quota.
    ProbeFailed,
    Held(Hold),
    Kept,
    /// Kept, but draining in place with nowhere better to land.
    Draining,
    NoneViable,
    WouldSwitch,
    Switched,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub action: Action,
    pub mode: Mode,
    pub before: Option<String>,
    pub after: Option<String>,
    pub message: String,
}

impl Outcome {
    pub fn changed(&self) -> bool {
        self.action == Action::Switched
    }

    /// Worth printing from a daemon that otherwise stays quiet.
    pub fn is_notable(&self) -> bool {
        matches!(self.action, Action::Switched | Action::NoneViable)
    }

    fn held(
        action: Action,
        mode: Mode,
        current: Option<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            action,
            mode,
            before: current.clone(),
            after: current,
            message: message.into(),
        }
    }
}

/// Probe every account that is not parked, sample the startup model where
/// due, fold in carried samples and observed rejections, and store the
/// readings. Parked accounts get their refusal restated.
pub(super) async fn probe_fleet(
    fleet: &Fleet,
    cadence: &mut Cadence,
    model: Option<&str>,
    now: f64,
    force: bool,
    sample: bool,
    store: bool,
) -> Vec<Reading> {
    let (due, parked): (Vec<_>, Vec<_>) = fleet
        .accounts
        .iter()
        .partition(|account| force || !cadence.is_parked(account, now));
    let sample_with = sample.then(|| probe::sample_model(model)).flatten();
    let to_sample: Vec<_> = match sample_with {
        Some(_) => due
            .iter()
            .copied()
            .filter(|account| cadence.due_for_sample(account, &fleet.policy, now))
            .collect(),
        None => Vec::new(),
    };
    let client = Client::shared();
    let at = now as i64;
    let (mut readings, samples) = futures::join!(
        probe::probe_all(client, due.iter().copied(), PROBE_MODEL, PROBE_TIMEOUT, at),
        probe::sample_all(
            client,
            to_sample.iter().copied(),
            sample_with.unwrap_or(PROBE_MODEL),
            PROBE_TIMEOUT,
            at
        ),
    );
    for account in cadence.record_samples(&samples, now) {
        log(&format!(
            "target-model sample for {account} got no quota headers; next try in one interval"
        ));
    }
    cadence.apply_samples(&mut readings, now);
    observe::apply(&mut readings);
    if store && let Ok(store) = Store::open() {
        for reading in &readings {
            let _ = store.insert(reading);
        }
    }
    readings.extend(
        parked
            .iter()
            .filter_map(|account| cadence.synthetic_refusal(account, now)),
    );
    readings.sort_by_key(|reading| {
        fleet
            .accounts
            .iter()
            .position(|account| account.name == reading.account)
    });
    readings
}

/// `n/m quota readings; unavailable [cause: a, b; cause: c]`.
fn coverage_summary(readings: &[Reading], expected: usize) -> String {
    let available = readings
        .iter()
        .filter(|reading| reading.has_quota())
        .count();
    let mut by_reason = BTreeMap::<String, Vec<&str>>::new();
    for reading in readings.iter().filter(|reading| !reading.has_quota()) {
        let reason = reading
            .error
            .as_ref()
            .map_or_else(|| "no quota headers".to_string(), |error| error.reason());
        by_reason.entry(reason).or_default().push(&reading.account);
    }
    if by_reason.is_empty() {
        return format!("{available}/{expected} quota readings");
    }
    let groups: Vec<String> = by_reason
        .into_iter()
        .map(|(reason, accounts)| format!("{reason}: {}", accounts.join(", ")))
        .collect();
    format!(
        "{available}/{expected} quota readings; unavailable [{}]",
        groups.join("; ")
    )
}

/// `5h 32% left, 7d 25% left, Fable 7d 9% left`.
pub fn describe(reading: Option<&Reading>, model: Option<&ModelKey>) -> String {
    let Some(reading) = reading else {
        return "quota unavailable".into();
    };
    let Some((session, weekly)) = reading.remaining() else {
        return "quota unavailable".into();
    };
    let mut text = format!(
        "5h {:.0}% left, 7d {:.0}% left",
        session * 100.0,
        weekly * 100.0
    );
    for bucket in reading
        .buckets
        .iter()
        .filter(|bucket| model.is_some_and(|model| model.is_limited_by(&bucket.key)))
    {
        text.push_str(&format!(
            ", {} 7d {:.0}% left",
            bucket.label,
            bucket.window.remaining() * 100.0
        ));
    }
    text
}

pub async fn run(fleet: &Fleet, options: Options) -> Result<Outcome> {
    fleet.policy.validate()?;
    if fleet.accounts.is_empty() {
        bail!(
            "No accounts configured. Use `tokeman login <name>` or `tokeman add <name> <key>` to add one."
        );
    }
    let _lock = RotationLock::acquire()?;
    let mut cadence = Cadence::load();
    cadence.retain(&fleet.accounts);
    let previous_mode = cadence.mode.unwrap_or(Mode::Normal);
    if is_paused() {
        return Ok(Outcome::held(
            Action::Paused,
            previous_mode,
            None,
            "rotation is paused",
        ));
    }
    let note = |message: &str| {
        if options.dry_run {
            eprintln!("dry run: {message}");
        } else {
            log(message);
        }
    };
    if !options.scheduled && !options.dry_run && keychain::suppress(false)? {
        log("suppressed short-lived /login Keychain auth before managed rotation");
    }
    let observed = match observe::scan_transcripts(&fleet.accounts) {
        Ok(observed) => observed,
        Err(error) => {
            note(&format!(
                "ERROR Claude admission feedback scan failed: {error:#}"
            ));
            0
        }
    };
    if observed > 0 {
        note(&format!(
            "observed {observed} Claude quota rejection(s); reevaluating default now"
        ));
    }
    let started = now_epoch();
    if options.scheduled && observed == 0 {
        let interval = cadence
            .probe_interval_secs
            .unwrap_or_else(|| fleet.policy.interval_secs(previous_mode))
            as f64;
        if started - cadence.last_probe_epoch < interval {
            return Ok(Outcome::held(
                Action::NotDue,
                previous_mode,
                None,
                format!("next probe is not due ({interval:.0}s cadence)"),
            ));
        }
        cadence.last_probe_epoch = started;
        if !options.dry_run {
            cadence.save()?;
        }
    }

    let mut settings = claude_settings::load()?;
    let model_raw = claude_settings::target_model(&settings).map(str::to_owned);
    let model = model_raw.as_deref().map(ModelKey::new);
    let current_value = claude_settings::configured_token(&settings).map(str::to_owned);
    let current_name = current_value
        .as_deref()
        .and_then(|value| fleet.account_holding(value))
        .map(|account| account.name.clone());
    if let Some(name) = current_name.as_deref()
        && !options.dry_run
    {
        let account = fleet
            .account(name)
            .context("the managed default left the fleet")?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Some(kind) = claude_settings::reinstall_reason(&settings, account, now_ms) {
            let event = claude_settings::install(&mut settings, account, now_ms, kind)?;
            claude_settings::commit(&settings, Some(event))?;
            if kind != EventKind::Repair {
                log(&format!(
                    "{}: {name} now offers {}",
                    kind.label(),
                    claude_settings::describe_credential(account, now_ms)
                ));
            }
        }
    }

    let now = now_epoch();
    // Only accounts actually asked this cycle update the backoff table; a
    // parked account's restated refusal is not a new failure.
    let probed: Vec<String> = fleet
        .accounts
        .iter()
        .filter(|account| options.force || !cadence.is_parked(account, now))
        .map(|account| account.name.clone())
        .collect();
    let readings = probe_fleet(
        fleet,
        &mut cadence,
        model_raw.as_deref(),
        now,
        options.force,
        !options.dry_run,
        !options.dry_run,
    )
    .await;
    let mut backoff = cadence.clone();
    let asked = readings
        .iter()
        .filter(|reading| probed.contains(&reading.account));
    for account in backoff.record_refusals(&fleet.accounts, asked, now) {
        note(&format!(
            "{account} refused authentication; rechecking on a backoff instead of every cycle"
        ));
    }
    for account in cadence
        .backoff
        .keys()
        .filter(|name| !backoff.backoff.contains_key(*name))
    {
        note(&format!(
            "{account} answered again; clearing authentication backoff"
        ));
    }
    if !options.dry_run {
        cadence.backoff = backoff.backoff;
    }

    let expected = fleet.accounts.len();
    let complete = policy::complete(&readings, expected);
    let mode = policy::safe_mode_for(
        &readings,
        expected,
        &fleet.policy,
        previous_mode,
        model.as_ref(),
    );
    let current_reading = current_name
        .as_deref()
        .and_then(|name| readings.iter().find(|reading| reading.account == name));
    let active = current_name.clone().or_else(|| {
        current_value
            .as_ref()
            .map(|_| "unmanaged OAuth token".to_string())
    });
    let next_interval = policy::next_interval_secs(mode, current_reading, complete, &fleet.policy);
    cadence.last_probe_epoch = started;
    cadence.mode = Some(mode);
    cadence.probe_interval_secs = Some(next_interval);
    cadence.consecutive_degraded_probes = if complete {
        0
    } else {
        cadence.consecutive_degraded_probes.saturating_add(1)
    };
    if options.dry_run {
        eprintln!(
            "dry run: would record {} mode with the next probe in {next_interval}s",
            mode.label()
        );
    } else {
        cadence.save()?;
    }

    if !readings.iter().any(Reading::has_quota) {
        let message = format!(
            "probe failed for every account; kept {}; retrying in {next_interval}s ({})",
            active.as_deref().unwrap_or("/login"),
            coverage_summary(&readings, expected)
        );
        note(&message);
        return Ok(Outcome::held(Action::ProbeFailed, mode, active, message));
    }
    if mode != previous_mode {
        note(&format!(
            "entered {} mode; sampling every {}s",
            mode.label(),
            fleet.policy.interval_secs(mode)
        ));
    }

    let situation = Situation {
        readings: &readings,
        expected,
        current: current_name.as_deref(),
        model: model.as_ref(),
        force: options.force,
    };
    let decision = policy::decide(&situation, &fleet.policy);
    let kept =
        |action, message: String| Ok(Outcome::held(action, mode, current_name.clone(), message));
    match decision {
        Decision::Hold(Hold::PartialProbe) => {
            let message = format!(
                "partial probe; kept default {}; retrying in {next_interval}s ({})",
                active.as_deref().unwrap_or("/login"),
                coverage_summary(&readings, expected)
            );
            note(&message);
            Ok(Outcome::held(
                Action::Held(Hold::PartialProbe),
                mode,
                active,
                message,
            ))
        }
        Decision::Hold(Hold::DefaultUnreadable) => {
            let message = format!(
                "default quota unavailable; kept {}; retrying in {next_interval}s ({})",
                current_name.as_deref().unwrap_or("default account"),
                coverage_summary(&readings, expected)
            );
            note(&message);
            kept(Action::Held(Hold::DefaultUnreadable), message)
        }
        Decision::Stay { healthy: true } => kept(
            Action::Kept,
            format!(
                "kept {} ({})",
                current_name.as_deref().unwrap_or("default account"),
                describe(current_reading, model.as_ref())
            ),
        ),
        Decision::Stay { healthy: false } => kept(
            Action::Draining,
            format!(
                "kept {} ({}; draining in place, no account above the landing floors)",
                current_name.as_deref().unwrap_or("default account"),
                describe(current_reading, model.as_ref())
            ),
        ),
        Decision::NoneViable => {
            let message = format!(
                "no viable account; kept {}",
                active.as_deref().unwrap_or("/login")
            );
            note(&message);
            Ok(Outcome::held(Action::NoneViable, mode, active, message))
        }
        Decision::Switch { to, trigger } => {
            let trigger_text = match trigger {
                Trigger::NoDefault => "no managed account was active".to_string(),
                Trigger::Forced => format!(
                    "forced best-account selection from {}",
                    describe(current_reading, None)
                ),
                Trigger::Refused => format!(
                    "authentication rejected ({})",
                    describe(current_reading, None)
                ),
                Trigger::SipFloor => format!(
                    "sip floor reached ({})",
                    describe(current_reading, model.as_ref())
                ),
                Trigger::NormalFloor => format!(
                    "normal floor reached ({})",
                    describe(current_reading, model.as_ref())
                ),
            };
            let message = format!(
                "switch {} -> {}; {trigger_text}; new {}",
                current_name.as_deref().unwrap_or("/login"),
                to.account,
                describe(Some(to), model.as_ref())
            );
            if options.dry_run {
                return Ok(Outcome {
                    action: Action::WouldSwitch,
                    mode,
                    before: current_name.clone(),
                    after: Some(to.account.clone()),
                    message: format!("would {message}"),
                });
            }
            if !options.scheduled && keychain::suppress(false)? {
                log("suppressed short-lived /login Keychain auth before credential switch");
            }
            let account = fleet
                .account(&to.account)
                .context("probe returned an account absent from the fleet")?;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let event =
                claude_settings::install(&mut settings, account, now_ms, EventKind::Switch)?;
            claude_settings::commit(&settings, Some(event))?;
            log(&message);
            Ok(Outcome {
                action: Action::Switched,
                mode,
                before: current_name.clone(),
                after: Some(to.account.clone()),
                message,
            })
        }
    }
}

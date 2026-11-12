//! Usage-limit resets: Anthropic's programs that refill an account's limits
//! on request, as Claude Code's client (2.1.322) reads and claims them.
//!
//! `cedar_ember` grants are read from `/api/oauth/usage?cedar_ember=1`;
//! `juniper_tide`, a session reset offered at the 5h limit and paid for out
//! of the weekly limit, from the at-wall form. Both are claimed with
//! `POST /api/organizations/{org}/reset_rate_limits` and are eligible only
//! for Claude Code's user agent: a program the server did not evaluate
//! (`null`, or ineligible for `surface`) is unknown, never "no resets".
//!
//! A grant either refills the windows it `clears`, or, when it
//! `gives_back_weekly` (Fable grants), gives back only the weekly usage
//! spent on Fable and refills the 5h window. Neither moves the weekly reset.
//!
//! A grant claim whose outcome is unknown (no answer, an error, an unread
//! body) leaves a journal entry. While it stands the account may only resend
//! that same claim, which the server deduplicates by request id; it is never
//! retried with another grant.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::profile::{Cache, Endpoint};
use super::{Client, describe};
use crate::account::Account;
use crate::fs::write_atomic;
use crate::paths;
use crate::util::one_line;

/// What a grant resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantType {
    Session,
    Weekly,
    Full,
    #[serde(other)]
    Unrecognized,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetGrant {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<GrantType>,
    #[serde(default)]
    pub resets_total: u32,
    pub resets_left: u32,
    #[serde(default)]
    pub starts_at: Option<String>,
    #[serde(default)]
    pub ends_at: Option<String>,
    /// Windows a reset refills: `five_hour`, `seven_day`,
    /// `seven_day_overage_included` (the Fable limit), ...
    #[serde(default)]
    pub clears: Vec<String>,
    /// Gives back only the weekly usage spent on its model (Fable), not the
    /// whole of `seven_day`.
    #[serde(default)]
    pub gives_back_weekly: bool,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub usable_now: bool,
    #[serde(default = "yes")]
    pub use_requires_limit: bool,
    /// Why the grant cannot be used now (`cooldown`, `needs_limit`, ...).
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// What stands in the way (`blocked_five_hour`, ...).
    #[serde(default)]
    pub blocking: Vec<String>,
    /// Current use of each window the grant clears, 0-100.
    #[serde(default)]
    pub percent_used: BTreeMap<String, serde_json::Value>,
}

fn yes() -> bool {
    true
}

fn instant(value: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value?)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Where a grant stands at a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantState {
    Paused,
    NotStarted,
    Expired,
    Spent,
    /// Has resets left and is in its validity period; the server may still
    /// gate its use (`usable_now`).
    Usable,
}

impl ResetGrant {
    pub fn state(&self, now: i64) -> GrantState {
        let now = DateTime::from_timestamp(now, 0).unwrap_or_default();
        if self.paused {
            GrantState::Paused
        } else if self.resets_left == 0 {
            GrantState::Spent
        } else if instant(self.starts_at.as_deref()).is_some_and(|start| now < start) {
            GrantState::NotStarted
        } else if instant(self.ends_at.as_deref()).is_some_and(|end| now >= end) {
            GrantState::Expired
        } else {
            GrantState::Usable
        }
    }

    /// Counts toward the account's resets: not paused, started, not expired,
    /// with resets left.
    pub fn is_usable(&self, now: i64) -> bool {
        self.state(now) == GrantState::Usable
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrantProgram {
    pub eligible: bool,
    #[serde(default)]
    pub ineligible_reason: Option<String>,
    #[serde(default)]
    pub at_limit: bool,
    /// Windows at their limit.
    #[serde(default)]
    pub exhausted: Vec<String>,
    /// Raw: one malformed grant must not hide the others.
    #[serde(default)]
    grants: Vec<serde_json::Value>,
    #[serde(default)]
    pub next_grant_id: Option<String>,
    #[serde(default)]
    pub weekly_resets_at: Option<String>,
    #[serde(default)]
    pub cooldown_until: Option<String>,
}

/// What a program's answer establishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The server did not evaluate the account: the block was null, or it
    /// was refused for the calling client (`surface`) or an unknown reason.
    Unknown,
    Ineligible,
    Eligible,
}

/// Ineligibility reasons that describe the request rather than the account.
const UNKNOWN_REASONS: &[&str] = &["surface", "unknown", "cli_version"];

fn standing(eligible: bool, reason: Option<&str>) -> Standing {
    match (eligible, reason) {
        (true, _) => Standing::Eligible,
        (false, None) => Standing::Unknown,
        (false, Some(reason)) if UNKNOWN_REASONS.contains(&reason) => Standing::Unknown,
        (false, Some(_)) => Standing::Ineligible,
    }
}

impl GrantProgram {
    pub fn grants(&self) -> Vec<ResetGrant> {
        self.grants
            .iter()
            .filter_map(|grant| serde_json::from_value(grant.clone()).ok())
            .collect()
    }

    pub fn standing(&self) -> Standing {
        standing(self.eligible, self.ineligible_reason.as_deref())
    }

    /// Resets the account can count on at `now`; `None` when unknown.
    pub fn usable_resets(&self, now: i64) -> Option<u32> {
        (self.standing() == Standing::Eligible).then(|| {
            self.grants()
                .iter()
                .filter(|grant| grant.is_usable(now))
                .map(|grant| grant.resets_left)
                .sum()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionProgram {
    #[serde(default)]
    pub eligible: bool,
    #[serde(default)]
    pub ineligible_reason: Option<String>,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub next_available_at: Option<String>,
    #[serde(default)]
    pub weekly_resets_at: Option<String>,
    #[serde(default)]
    pub resets_per_week: Option<u32>,
}

impl SessionProgram {
    pub fn standing(&self) -> Standing {
        standing(
            self.eligible || self.available,
            self.ineligible_reason.as_deref(),
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResetStatus {
    #[serde(default)]
    pub cedar_ember: Option<GrantProgram>,
    #[serde(default)]
    pub juniper_tide: Option<SessionProgram>,
    /// Why a program's read failed, when one did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// What a claim did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimResult {
    Reset,
    AlreadyUsed,
    NotLimited,
    Cooldown,
    Ineligible,
    Unavailable,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimResponse {
    pub result: ClaimResult,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub resets_left: Option<u32>,
    #[serde(default)]
    pub cleared: Vec<String>,
    #[serde(default)]
    pub weekly_resets_at: Option<String>,
    #[serde(default)]
    pub cooldown_until: Option<String>,
}

fn profile_token(account: &Account, now: i64) -> Result<&str> {
    account.profile_token(now * 1000).with_context(|| {
        format!(
            "{} has no live profile credential; run `tokeman login {}`",
            account.name, account.name
        )
    })
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    bearer: &str,
    path: &str,
) -> Result<T> {
    let response = client
        .oauth_get(path, bearer)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("request to {path} failed: {}", describe(&error)))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{path} answered HTTP {status}: {}", one_line(&body, 200));
    }
    serde_json::from_str(&body).with_context(|| format!("unreadable {path} response"))
}

/// Reset offers, from the shared profile-read cache unless `fresh`: the
/// granted resets from the cedar_ember read, the session reset from the
/// at-wall read. A failed read leaves its program unknown and says why.
/// `now` is epoch seconds.
pub async fn status(
    client: &Client,
    cache: &Cache,
    account: &Account,
    fresh: bool,
    now: i64,
) -> Result<ResetStatus> {
    let bearer = profile_token(account, now)?;
    let mut status = ResetStatus::default();
    for endpoint in [Endpoint::CedarEmber, Endpoint::AtWall] {
        let body = match cache
            .read(client, &account.name, bearer, endpoint, fresh, now)
            .await
        {
            Ok(body) => body,
            Err(error) => {
                status.errors.push(format!("{error:#}"));
                continue;
            }
        };
        let read: ResetStatus = match serde_json::from_value(body) {
            Ok(read) => read,
            Err(error) => {
                status
                    .errors
                    .push(format!("unreadable reset status: {error}"));
                continue;
            }
        };
        match endpoint {
            Endpoint::CedarEmber => status.cedar_ember = read.cedar_ember,
            _ => status.juniper_tide = read.juniper_tide,
        }
    }
    if status.cedar_ember.is_none() && status.juniper_tide.is_none() && !status.errors.is_empty() {
        bail!("{}", status.errors.join("; "));
    }
    Ok(status)
}

/// The usable granted resets in the cached cedar_ember read of `account`,
/// one line each, and when that read was made. No request is made; `None`
/// when the account was never read.
pub fn stored_offers(cache: &Cache, account: &str, now: i64) -> Option<(i64, Vec<String>)> {
    let (fetched_at, body) = cache.stored(account, Endpoint::CedarEmber)?;
    let status: ResetStatus = serde_json::from_value(body).ok()?;
    let offers = status
        .cedar_ember
        .map(|program| program.grants())
        .unwrap_or_default()
        .iter()
        .filter(|grant| grant.is_usable(now))
        .map(|grant| describe_grant(grant, now))
        .collect();
    Some((fetched_at, offers))
}

async fn organization_uuid(client: &Client, bearer: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Profile {
        organization: Option<Organization>,
    }
    #[derive(Deserialize)]
    struct Organization {
        uuid: String,
    }
    get_json::<Profile>(client, bearer, "/api/oauth/profile")
        .await?
        .organization
        .map(|organization| organization.uuid)
        .context("the profile names no organization")
}

/// A grant claim sent without a known outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingClaim {
    pub grant_id: String,
    pub request_id: String,
    /// Epoch seconds the claim was first sent.
    pub sent_at: i64,
}

/// The pending-claim journal: one file per account, written before a grant
/// claim is sent and removed only once the server's answer is read.
#[derive(Debug, Clone)]
pub struct Journal {
    dir: PathBuf,
}

impl Default for Journal {
    fn default() -> Self {
        Self::at(paths::reset_claims_dir())
    }
}

impl Journal {
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, account: &str) -> PathBuf {
        let safe: String = account
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.@".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.pending"))
    }

    /// The account's pending claim. An unreadable entry is reported as an
    /// error rather than ignored: it still stands for a claim in flight.
    pub fn pending(&self, account: &str) -> Result<Option<PendingClaim>> {
        let path = self.path(account);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .with_context(|| format!("unreadable pending claim {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn record(&self, account: &str, claim: &PendingClaim) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(claim)?;
        write_atomic(&self.path(account), &bytes, 0o600)
    }

    /// Forget the account's pending claim, once its outcome is known (also
    /// from Claude's own Settings > Usage page).
    pub fn clear(&self, account: &str) -> Result<()> {
        match std::fs::remove_file(self.path(account)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }
}

/// Claim one reset. `grant_id` selects a `cedar_ember` grant; `None` claims
/// the `juniper_tide` session reset. A grant claim goes through `journal`:
/// with a claim pending, only the same grant may be claimed, and it is
/// resent with its original request id.
pub async fn claim(
    client: &Client,
    account: &Account,
    grant_id: Option<&str>,
    journal: &Journal,
    now: i64,
) -> Result<ClaimResponse> {
    let bearer = profile_token(account, now)?;
    let pending = match grant_id {
        Some(grant_id) => Some(match journal.pending(&account.name)? {
            Some(pending) if pending.grant_id == grant_id => pending,
            Some(pending) => bail!(
                "a claim of grant {} sent at {} has no known outcome; check Settings > Usage on \
                 claude.ai, then resend it with `tokeman resets use {} --grant {}` or forget it \
                 with `tokeman resets clear {}`",
                pending.grant_id,
                DateTime::from_timestamp(pending.sent_at, 0).unwrap_or_default(),
                account.name,
                pending.grant_id,
                account.name
            ),
            None => PendingClaim {
                grant_id: grant_id.to_owned(),
                request_id: request_id()?,
                sent_at: now,
            },
        }),
        None => None,
    };
    let organization = organization_uuid(client, bearer).await?;
    let body = match &pending {
        Some(pending) => {
            journal.record(&account.name, pending)?;
            serde_json::json!({
                "program": "cedar_ember",
                "grant_id": pending.grant_id,
                "request_id": pending.request_id,
            })
        }
        None => serde_json::json!({ "program": "juniper_tide" }),
    };
    let path = format!("/api/organizations/{organization}/reset_rate_limits");
    let response = client
        .oauth_post(&path, bearer)
        .json(&body)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("reset request failed: {}", describe(&error)))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("reset answered HTTP {status}: {}", one_line(&text, 200));
    }
    let response: ClaimResponse =
        serde_json::from_str(&text).context("unreadable reset response")?;
    if pending.is_some() && response.result != ClaimResult::Unknown {
        journal.clear(&account.name)?;
    }
    Ok(response)
}

/// 16 random bytes as hex.
fn request_id() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .context("could not read /dev/urandom for a request id")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Human names for the windows a grant clears.
pub fn window_label(window: &str) -> &str {
    match window {
        "five_hour" => "5h",
        "seven_day" => "7d",
        "seven_day_overage_included" => "Fable 7d",
        "seven_day_opus" => "Opus 7d",
        "seven_day_sonnet" => "Sonnet 7d",
        other => other,
    }
}

/// What a grant restores, in words: a Fable grant gives back only what
/// Fable used of the weekly limit.
fn restores(grant: &ResetGrant) -> String {
    grant
        .clears
        .iter()
        .map(|window| match window.as_str() {
            "seven_day" if grant.gives_back_weekly => "Fable's use of 7d",
            other => window_label(other),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line per grant, e.g. `Claude Opus 5.5 launch: 1/1 left, usable now
/// (clears 5h 7d Fable 7d; now at 0% 0% 0%), use by 2026-10-22`. `now` is
/// epoch seconds.
pub fn describe_grant(grant: &ResetGrant, now: i64) -> String {
    let used = grant
        .clears
        .iter()
        .map(
            |window| match grant.percent_used.get(window).and_then(|v| v.as_f64()) {
                Some(percent) => format!("{percent:.0}%"),
                None => "?".into(),
            },
        )
        .collect::<Vec<_>>()
        .join(" ");
    let day = |at: &str| at.get(..10).unwrap_or(at).to_owned();
    let when = match grant.state(now) {
        GrantState::Paused => "paused".to_owned(),
        GrantState::Spent => "used up".to_owned(),
        GrantState::Expired => "expired".to_owned(),
        GrantState::NotStarted => format!(
            "starts {}",
            grant.starts_at.as_deref().map(day).unwrap_or_default()
        ),
        GrantState::Usable if grant.usable_now => "usable now".to_owned(),
        GrantState::Usable => match grant.unavailable_reason.as_deref() {
            Some(reason) => format!("not usable now: {reason}"),
            None if grant.use_requires_limit => "usable at a limit".to_owned(),
            None => "not usable yet".to_owned(),
        },
    };
    let verb = if grant.gives_back_weekly {
        "gives back"
    } else {
        "clears"
    };
    let deadline = grant
        .ends_at
        .as_deref()
        .map(|end| format!(", use by {}", day(end)))
        .unwrap_or_default();
    format!(
        "{}: {}/{} left, {when} ({verb} {}; now at {used}){deadline}",
        if grant.label.is_empty() {
            &grant.id
        } else {
            &grant.label
        },
        grant.resets_left,
        grant.resets_total.max(grant.resets_left),
        restores(grant),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::testing::{serve, temp_cache};
    use crate::testing::{profile_only_account, setup_account};
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use std::sync::{Arc, Mutex};

    fn at(iso: &str) -> i64 {
        DateTime::parse_from_rfc3339(iso).unwrap().timestamp()
    }

    const CEDAR_STATUS: &str = r#"{"cedar_ember": {"eligible": true, "ineligible_reason": null, "at_limit": false,
        "exhausted": [], "grants": [{"id": "opus55-launch-promax-20260921",
        "label": "Claude Opus 5.5 launch: one usage-limit reset for Pro and Max",
        "resets_total": 1, "resets_left": 1, "starts_at": "2026-09-22T16:00:00+00:00",
        "ends_at": "2026-10-22T16:00:00+00:00",
        "clears": ["five_hour", "seven_day", "seven_day_overage_included"],
        "paused": false, "usable_now": true, "use_requires_limit": false,
        "percent_used": {"five_hour": 0, "seven_day": 12, "seven_day_overage_included": 0},
        "blocking": [], "arm": null}, {"id": "broken"}],
        "next_grant_id": "opus55-launch-promax-20260921",
        "weekly_resets_at": "2026-10-09T09:00:00+00:00"},
      "juniper_tide": null}"#;

    const AT_WALL_STATUS: &str = r#"{"cedar_ember": null,
      "juniper_tide": {"eligible": false, "ineligible_reason": "not_at_wall",
        "in_experiment": false, "available": false, "resets_per_week": 1}}"#;

    #[test]
    fn a_live_status_block_parses_and_describes() {
        let status: ResetStatus = serde_json::from_str(CEDAR_STATUS).unwrap();
        let program = status.cedar_ember.clone().unwrap();
        let grants = program.grants();
        assert_eq!(grants.len(), 1, "a malformed grant is skipped, not fatal");
        assert_eq!(
            describe_grant(&grants[0], at("2026-10-01T00:00:00Z")),
            "Claude Opus 5.5 launch: one usage-limit reset for Pro and Max: 1/1 left, \
             usable now (clears 5h 7d Fable 7d; now at 0% 12% 0%), use by 2026-10-22"
        );
        assert_eq!(program.usable_resets(at("2026-10-01T00:00:00Z")), Some(1));
        assert_eq!(program.usable_resets(at("2026-10-23T00:00:00Z")), Some(0));
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<ResetStatus>(&json).unwrap(), status);
        assert_eq!(
            serde_json::from_str::<ResetStatus>("{}").unwrap(),
            ResetStatus::default()
        );
    }

    #[test]
    fn a_grant_counts_only_while_unpaused_started_and_unexpired() {
        let grant: ResetGrant = serde_json::from_str(
            r#"{"id": "fable52-launch", "type": "weekly", "resets_left": 2, "resets_total": 3,
                "starts_at": "2026-11-09T18:00:00Z", "ends_at": "2027-03-09T07:59:00Z",
                "clears": ["five_hour", "seven_day", "seven_day_overage_included"],
                "gives_back_weekly": true, "usable_now": false, "use_requires_limit": false,
                "unavailable_reason": "cooldown", "blocking": ["blocked_seven_day"],
                "percent_used": {"five_hour": 0, "seven_day": 61, "seven_day_overage_included": 100}}"#,
        )
        .unwrap();
        assert_eq!(grant.kind, Some(GrantType::Weekly));
        assert_eq!(grant.blocking, ["blocked_seven_day"]);
        assert_eq!(
            grant.state(at("2026-11-08T00:00:00Z")),
            GrantState::NotStarted
        );
        assert_eq!(grant.state(at("2026-11-11T00:00:00Z")), GrantState::Usable);
        assert_eq!(grant.state(at("2027-03-09T07:59:00Z")), GrantState::Expired);
        assert!(!grant.is_usable(at("2026-11-08T00:00:00Z")));
        assert!(grant.is_usable(at("2026-11-11T00:00:00Z")));
        let paused = ResetGrant {
            paused: true,
            ..grant.clone()
        };
        assert!(!paused.is_usable(at("2026-11-11T00:00:00Z")));
        let spent = ResetGrant {
            resets_left: 0,
            ..grant.clone()
        };
        assert_eq!(spent.state(at("2026-11-11T00:00:00Z")), GrantState::Spent);

        // A Fable grant gives back what Fable used, not the whole weekly.
        assert_eq!(
            describe_grant(&grant, at("2026-11-11T00:00:00Z")),
            "fable52-launch: 2/3 left, not usable now: cooldown \
             (gives back 5h Fable's use of 7d Fable 7d; now at 0% 61% 100%), use by 2027-03-09"
        );
        assert!(describe_grant(&grant, at("2026-11-08T00:00:00Z")).contains("starts 2026-11-09"));
        let json = serde_json::to_string(&grant).unwrap();
        assert!(json.contains(r#""type":"weekly""#));
        assert_eq!(serde_json::from_str::<ResetGrant>(&json).unwrap(), grant);
    }

    #[test]
    fn a_program_the_server_did_not_evaluate_is_unknown_not_zero() {
        let surface: GrantProgram = serde_json::from_str(
            r#"{"eligible": false, "ineligible_reason": "surface", "grants": []}"#,
        )
        .unwrap();
        assert_eq!(surface.standing(), Standing::Unknown);
        assert_eq!(surface.usable_resets(0), None);
        let tier: GrantProgram =
            serde_json::from_str(r#"{"eligible": false, "ineligible_reason": "tier"}"#).unwrap();
        assert_eq!(tier.standing(), Standing::Ineligible);
        let no_grant: GrantProgram =
            serde_json::from_str(r#"{"eligible": false, "ineligible_reason": "no_grant"}"#)
                .unwrap();
        assert_eq!(no_grant.standing(), Standing::Ineligible);
        let session: SessionProgram =
            serde_json::from_str(r#"{"eligible": false, "ineligible_reason": "surface"}"#).unwrap();
        assert_eq!(session.standing(), Standing::Unknown);
    }

    #[test]
    fn request_ids_fit_the_server_format() {
        let id = request_id().unwrap();
        assert!(id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn claim_results_are_named_and_unknown_ones_survive() {
        let response: ClaimResponse = serde_json::from_str(
            r#"{"result": "reset", "cleared": ["five_hour"], "resets_left": 0,
                "weekly_resets_at": "2026-10-20T00:00:00+00:00"}"#,
        )
        .unwrap();
        assert_eq!(response.result, ClaimResult::Reset);
        assert_eq!(response.cleared, vec!["five_hour"]);
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains(r#""result":"reset""#));
        assert_eq!(
            serde_json::from_str::<ClaimResponse>(&json).unwrap(),
            response
        );
        let odd: ClaimResponse =
            serde_json::from_str(r#"{"result": "something_new", "reason": "x"}"#).unwrap();
        assert_eq!(odd.result, ClaimResult::Unknown);
        assert_eq!(
            serde_json::from_str::<ClaimResult>(r#""already_used""#).unwrap(),
            ClaimResult::AlreadyUsed
        );
    }

    #[test]
    fn grant_shapes_default_their_optional_fields() {
        let grant: ResetGrant = serde_json::from_str(r#"{"id": "g", "resets_left": 2}"#).unwrap();
        assert!(grant.use_requires_limit);
        assert_eq!(grant.kind, None);
        assert_eq!(
            describe_grant(&grant, 0),
            "g: 2/2 left, usable at a limit (clears ; now at )"
        );
        let json = serde_json::to_string(&grant).unwrap();
        assert_eq!(serde_json::from_str::<ResetGrant>(&json).unwrap(), grant);
        assert_eq!(window_label("seven_day_opus"), "Opus 7d");
        assert_eq!(window_label("other"), "other");
    }

    /// The usage reads answer per query; claims answer from `script` in
    /// order and are recorded.
    fn fake_api(
        script: Vec<(StatusCode, &'static str)>,
    ) -> (Router, Arc<Mutex<Vec<serde_json::Value>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let recorded = sent.clone();
        let script = Arc::new(Mutex::new(script.into_iter()));
        let router = Router::new()
            .route(
                "/api/oauth/usage",
                get(|query: axum::extract::RawQuery| async move {
                    match query.0.as_deref() {
                        Some("cedar_ember=1&skip_spend=1") => CEDAR_STATUS,
                        Some("at_wall=1&skip_spend=1") => AT_WALL_STATUS,
                        other => panic!("unexpected usage query {other:?}"),
                    }
                }),
            )
            .route(
                "/api/oauth/profile",
                get(|| async { r#"{"organization": {"uuid": "org-1"}}"#.to_owned() }),
            )
            .route(
                "/api/organizations/{org}/reset_rate_limits",
                post(
                    move |axum::extract::Path(org): axum::extract::Path<String>,
                          headers: axum::http::HeaderMap,
                          body: String| {
                        let recorded = recorded.clone();
                        let script = script.clone();
                        async move {
                            assert_eq!(org, "org-1");
                            assert_eq!(
                                headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
                                Some(super::super::OAUTH_BETA)
                            );
                            recorded
                                .lock()
                                .unwrap()
                                .push(serde_json::from_str(&body).unwrap());
                            let (status, body) = script
                                .lock()
                                .unwrap()
                                .next()
                                .unwrap_or((StatusCode::IM_A_TEAPOT, ""));
                            (status, body.to_owned())
                        }
                    },
                ),
            );
        (router, sent)
    }

    #[tokio::test]
    async fn each_program_comes_from_its_own_read_through_the_profile_token() {
        let (router, _) = fake_api(vec![]);
        let client = serve(router).await;
        let (_dir, cache) = temp_cache();
        let setup_only = setup_account("acct");
        let error = status(&client, &cache, &setup_only, false, 1)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no live profile credential"),
            "{error}"
        );

        let account = profile_only_account("acct", "profile");
        let status = status(&client, &cache, &account, false, 1).await.unwrap();
        assert_eq!(status.cedar_ember.unwrap().grants().len(), 1);
        assert_eq!(
            status.juniper_tide.unwrap().ineligible_reason.as_deref(),
            Some("not_at_wall")
        );
        assert!(status.errors.is_empty());

        // The cached read answers later without a request, however old.
        let then = at("2026-10-01T00:00:00Z");
        let (read_at, offers) = stored_offers(&cache, "acct", then).unwrap();
        assert_eq!(read_at, 1);
        assert_eq!(offers.len(), 1);
        assert!(offers[0].contains("usable now"), "{offers:?}");
        let expired = at("2026-10-23T00:00:00Z");
        assert_eq!(
            stored_offers(&cache, "acct", expired),
            Some((1, Vec::new()))
        );
        assert_eq!(stored_offers(&cache, "never-read", then), None);
    }

    #[tokio::test]
    async fn an_uncertain_claim_is_resent_as_itself_and_blocks_any_other_grant() {
        let (router, sent) = fake_api(vec![
            (StatusCode::BAD_GATEWAY, "upstream timeout"),
            (
                StatusCode::OK,
                r#"{"result": "reset", "cleared": ["five_hour"]}"#,
            ),
            (StatusCode::OK, r#"{"result": "not_limited"}"#),
        ]);
        let client = serve(router).await;
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path().join("claims"));
        let account = profile_only_account("acct", "profile");

        let error = claim(&client, &account, Some("g1"), &journal, 100)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("HTTP 502"), "{error}");
        let pending = journal.pending("acct").unwrap().expect("left pending");
        assert_eq!(pending.grant_id, "g1");
        assert_eq!(pending.sent_at, 100);

        // Another grant is refused before any request.
        let refused = claim(&client, &account, Some("g2"), &journal, 200)
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("no known outcome"),
            "{refused}"
        );
        assert_eq!(sent.lock().unwrap().len(), 1);

        // The same grant is resent with the same request id; an answer clears it.
        let claimed = claim(&client, &account, Some("g1"), &journal, 300)
            .await
            .unwrap();
        assert_eq!(claimed.result, ClaimResult::Reset);
        let bodies = sent.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["program"], "cedar_ember");
        assert_eq!(bodies[0]["request_id"], bodies[1]["request_id"]);
        assert_eq!(bodies[1]["grant_id"], "g1");
        assert_eq!(bodies[0]["request_id"].as_str().unwrap().len(), 32);
        assert_eq!(journal.pending("acct").unwrap(), None);

        // The session reset carries no grant and no journal entry.
        let session = claim(&client, &account, None, &journal, 400).await.unwrap();
        assert_eq!(session.result, ClaimResult::NotLimited);
        assert_eq!(
            sent.lock().unwrap()[2],
            serde_json::json!({"program": "juniper_tide"})
        );
        assert_eq!(journal.pending("acct").unwrap(), None);
    }

    #[test]
    fn the_journal_clears_and_reports_an_unreadable_entry() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path().to_owned());
        journal.clear("nobody").unwrap();
        std::fs::write(dir.path().join("a_b.pending"), "not json").unwrap();
        assert!(
            journal.pending("a/b").is_err(),
            "a garbled entry still blocks"
        );
        journal.clear("a/b").unwrap();
        assert_eq!(journal.pending("a/b").unwrap(), None);
    }
}

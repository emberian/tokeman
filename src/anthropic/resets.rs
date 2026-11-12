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
//! body) leaves an entry in the claim journal ([`crate::claims`]). While it
//! stands the account may only resend that same claim, which the server
//! deduplicates by request id; it is never retried with another grant.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::profile::{Cache, Endpoint};
use super::{Client, describe};
use crate::account::Account;
use crate::claims::{Conflict, Journal};
use crate::drift::Unrecognized;
use crate::quota::Service;
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
    /// Keys and words of the answers outside the known sets
    /// ([`crate::drift`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unrecognized: Vec<String>,
}

// The known sets of the two program blocks: what this parser reads, then
// what has been seen and is not read. Sources: Claude Code 2.1.322's schema
// (research/anthropic.md section 3), the captures in research/prior-art.md,
// and the answers in tokeman's own cache through 2026-11-11 (`paused_held`,
// a grant's `arm`, `juniper_tide.event_props`, `not_at_wall`).

const KNOWN_GRANT_PROGRAM_KEYS: &[&str] = &[
    "eligible",
    "ineligible_reason",
    "at_limit",
    "exhausted",
    "grants",
    "next_grant_id",
    "weekly_resets_at",
    "cooldown_until",
    "paused_held",
    "event_props",
];
const KNOWN_SESSION_PROGRAM_KEYS: &[&str] = &[
    "eligible",
    "ineligible_reason",
    "available",
    "next_available_at",
    "weekly_resets_at",
    "resets_per_week",
    "arm",
    "in_experiment",
    "event_props",
];
const KNOWN_INELIGIBLE_REASONS: &[&str] = &[
    "config_off",
    "tier",
    "seat",
    "mobile",
    "surface",
    "cli_version",
    "no_grant",
    "tenure",
    "other_experiment",
    "unavailable",
    "unknown",
    "not_at_wall",
];
const KNOWN_GRANT_KEYS: &[&str] = &[
    "id",
    "label",
    "type",
    "resets_total",
    "resets_left",
    "starts_at",
    "ends_at",
    "clears",
    "gives_back_weekly",
    "paused",
    "usable_now",
    "use_requires_limit",
    "unavailable_reason",
    "blocking",
    "percent_used",
    "arm",
];
const KNOWN_GRANT_TYPES: &[&str] = &["session", "weekly", "full"];
const KNOWN_UNAVAILABLE_REASONS: &[&str] = &[
    "cooldown",
    "blocked_five_hour",
    "blocked_seven_day",
    "blocked_opus",
    "blocked_sonnet",
    "blocked_other",
    "cannot_clear",
    "needs_limit",
    "unavailable",
];
/// The windows a grant can clear: `clears`, `exhausted`, and the keys of
/// `percent_used`.
const KNOWN_WINDOWS: &[&str] = &[
    "five_hour",
    "seven_day",
    "seven_day_overage_included",
    "seven_day_opus",
    "seven_day_sonnet",
    "seven_day_cowork",
    "seven_day_omelette",
    "seven_day_oauth_apps",
];
const KNOWN_EVENT_PROPS_KEYS: &[&str] = &[
    "surface",
    "tier",
    "tenure_bucket",
    "billing_path",
    "billing_period",
    "extra_usage_state",
];
const KNOWN_TIERS: &[&str] = &[
    "claude_pro",
    "claude_max_5x",
    "claude_max_20x",
    "claude_team",
];
const KNOWN_SESSION_ARMS: &[&str] = &["control", "reset"];

/// The keys and words of the program blocks of a usage body that are
/// outside the known sets. A null block was not evaluated and says nothing.
pub fn unrecognized(body: &serde_json::Value) -> Vec<String> {
    let mut seen = Unrecognized::new();
    let cedar = &body["cedar_ember"];
    let at = "usage.cedar_ember";
    seen.keys(at, cedar, KNOWN_GRANT_PROGRAM_KEYS);
    seen.value(
        &format!("{at}.ineligible_reason"),
        cedar.get("ineligible_reason"),
        KNOWN_INELIGIBLE_REASONS,
    );
    seen.values(
        &format!("{at}.exhausted"),
        cedar.get("exhausted"),
        KNOWN_WINDOWS,
    );
    let props = &cedar["event_props"];
    seen.keys(&format!("{at}.event_props"), props, KNOWN_EVENT_PROPS_KEYS);
    seen.value(
        &format!("{at}.event_props.tier"),
        props.get("tier"),
        KNOWN_TIERS,
    );
    let grants = format!("{at}.grants");
    seen.row_keys(&grants, &cedar["grants"], KNOWN_GRANT_KEYS);
    for grant in cedar["grants"].as_array().into_iter().flatten() {
        seen.value(
            &format!("{grants}[].type"),
            grant.get("type"),
            KNOWN_GRANT_TYPES,
        );
        seen.value(
            &format!("{grants}[].unavailable_reason"),
            grant.get("unavailable_reason"),
            KNOWN_UNAVAILABLE_REASONS,
        );
        seen.values(
            &format!("{grants}[].clears"),
            grant.get("clears"),
            KNOWN_WINDOWS,
        );
        seen.keys(
            &format!("{grants}[].percent_used"),
            &grant["percent_used"],
            KNOWN_WINDOWS,
        );
    }
    let juniper = &body["juniper_tide"];
    let at = "usage.juniper_tide";
    seen.keys(at, juniper, KNOWN_SESSION_PROGRAM_KEYS);
    seen.value(
        &format!("{at}.ineligible_reason"),
        juniper.get("ineligible_reason"),
        KNOWN_INELIGIBLE_REASONS,
    );
    seen.value(&format!("{at}.arm"), juniper.get("arm"), KNOWN_SESSION_ARMS);
    seen.into_vec()
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
            "{} has no live profile credential; run `tokeman claude login {}`",
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
        crate::drift::merge(
            &mut status.unrecognized,
            super::profile::unrecognized(&body),
        );
        crate::drift::merge(&mut status.unrecognized, unrecognized(&body));
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
    // Refused before any request: another grant's claim is still unanswered.
    if let Some(grant_id) = grant_id
        && let Some(pending) = journal.pending(Service::Claude, &account.name)?
        && pending.item.as_deref() != Some(grant_id)
    {
        bail!(unanswered(&Conflict {
            pending,
            requested: grant_id.to_owned(),
        }));
    }
    let organization = organization_uuid(client, bearer).await?;
    let pending = match grant_id {
        Some(grant_id) => Some(
            journal
                .begin(Service::Claude, &account.name, Some(grant_id), now)
                .map_err(|error| match error.downcast::<Conflict>() {
                    Ok(conflict) => anyhow::anyhow!(unanswered(&conflict)),
                    Err(error) => error,
                })?
                .0,
        ),
        None => None,
    };
    let body = match &pending {
        Some(pending) => serde_json::json!({
            "program": "cedar_ember",
            "grant_id": pending.item,
            "request_id": pending.request_id,
        }),
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
    if let Some(pending) = &pending
        && response.result != ClaimResult::Unknown
    {
        journal.finish(Service::Claude, &account.name, &pending.request_id)?;
    }
    Ok(response)
}

/// Why a grant cannot be claimed while another grant's claim is unanswered.
fn unanswered(conflict: &Conflict) -> String {
    let pending = &conflict.pending;
    let grant = pending.item.as_deref().unwrap_or_default();
    format!(
        "a claim of grant {grant} sent at {} has no known outcome; check Settings > Usage on \
         claude.ai, then resend it with `tokeman claude reset {account} --grant {grant}` or \
         forget it with `tokeman claude reset {account} --clear`",
        DateTime::from_timestamp(pending.sent_at, 0).unwrap_or_default(),
        account = pending.account,
    )
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
        let journal = Journal::at(dir.path());
        let account = profile_only_account("acct", "profile");

        let error = claim(&client, &account, Some("g1"), &journal, 100)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("HTTP 502"), "{error}");
        let pending = journal
            .pending(Service::Claude, "acct")
            .unwrap()
            .expect("left pending");
        assert_eq!(pending.item.as_deref(), Some("g1"));
        assert_eq!(pending.sent_at, 100);

        // Another grant is refused before any request.
        let refused = claim(&client, &account, Some("g2"), &journal, 200)
            .await
            .unwrap_err();
        let refused = refused.to_string();
        assert!(refused.contains("no known outcome"), "{refused}");
        assert!(
            refused.contains("`tokeman claude reset acct --grant g1`")
                && refused.contains("`tokeman claude reset acct --clear`"),
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
        assert_eq!(bodies[0]["request_id"], pending.request_id.as_str());
        assert_eq!(journal.pending(Service::Claude, "acct").unwrap(), None);

        // The session reset carries no grant and no journal entry.
        let session = claim(&client, &account, None, &journal, 400).await.unwrap();
        assert_eq!(session.result, ClaimResult::NotLimited);
        assert_eq!(
            sent.lock().unwrap()[2],
            serde_json::json!({"program": "juniper_tide"})
        );
        assert_eq!(journal.pending(Service::Claude, "acct").unwrap(), None);
    }

    #[tokio::test]
    async fn a_claim_left_pending_by_an_earlier_build_is_honoured() {
        let (router, sent) = fake_api(vec![
            (StatusCode::OK, r#"{"result": "something_new"}"#),
            (StatusCode::OK, r#"{"result": "already_used"}"#),
        ]);
        let client = serve(router).await;
        let dir = tempfile::tempdir().unwrap();
        crate::claims::tests::legacy_claude_fixture(
            dir.path(),
            "acct",
            "fable52-launch",
            "0123456789abcdef0123456789abcdef",
        );
        let journal = Journal::at(dir.path());
        let account = profile_only_account("acct", "profile");

        // Another grant is refused without a request, and nothing is written.
        let refused = claim(&client, &account, Some("other-grant"), &journal, 200)
            .await
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("a claim of grant fable52-launch sent at 2025-11-11"),
            "{refused}"
        );
        assert!(sent.lock().unwrap().is_empty());
        assert!(!dir.path().join("claims.json").exists());

        // The same grant goes out with the request id the old file held. An
        // answer this build cannot name leaves the claim pending.
        let unknown = claim(&client, &account, Some("fable52-launch"), &journal, 300)
            .await
            .unwrap();
        assert_eq!(unknown.result, ClaimResult::Unknown);
        assert!(journal.pending(Service::Claude, "acct").unwrap().is_some());
        let answered = claim(&client, &account, Some("fable52-launch"), &journal, 400)
            .await
            .unwrap();
        assert_eq!(answered.result, ClaimResult::AlreadyUsed);
        let bodies = sent.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2);
        for body in &bodies {
            assert_eq!(
                *body,
                serde_json::json!({"program": "cedar_ember", "grant_id": "fable52-launch",
                    "request_id": "0123456789abcdef0123456789abcdef"})
            );
        }
        assert_eq!(journal.pending(Service::Claude, "acct").unwrap(), None);
    }

    #[tokio::test]
    async fn a_failed_organization_lookup_journals_nothing() {
        let client = serve(Router::new()).await;
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path());
        let account = profile_only_account("acct", "profile");
        assert!(
            claim(&client, &account, Some("g1"), &journal, 1)
                .await
                .is_err()
        );
        assert_eq!(journal.pending(Service::Claude, "acct").unwrap(), None);
    }

    #[test]
    fn the_program_blocks_seen_so_far_carry_nothing_unrecognised() {
        for body in [CEDAR_STATUS, AT_WALL_STATUS] {
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(unrecognized(&body), Vec::<String>::new());
        }
        // The fields of today's cached answers that this build does not read.
        let body = serde_json::json!({
            "cedar_ember": {"eligible": true, "paused_held": false, "cooldown_until": null,
                "event_props": {"surface": "claude_code_cli", "tier": "claude_max_20x",
                    "tenure_bucket": "365+", "billing_path": "stripe",
                    "billing_period": "unknown", "extra_usage_state": "enabled"},
                "grants": [{"id": "g", "resets_left": 1, "type": "weekly", "arm": null,
                    "gives_back_weekly": true, "unavailable_reason": "needs_limit",
                    "blocking": ["blocked_seven_day"], "starts_at": null}]},
            "juniper_tide": {"eligible": false, "arm": "control", "event_props": null,
                "next_available_at": null, "weekly_resets_at": null}
        });
        assert_eq!(unrecognized(&body), Vec::<String>::new());
    }

    #[test]
    fn a_program_field_a_grant_type_or_a_window_it_does_not_know_is_reported() {
        let body = serde_json::json!({
            "cedar_ember": {"eligible": false, "ineligible_reason": "region",
                "exhausted": ["seven_day", "seven_day_haiku"], "queue": 3,
                "event_props": {"tier": "claude_ultra", "cohort": "b"},
                "grants": [{"id": "g", "resets_left": 1, "type": "monthly",
                    "unavailable_reason": "blocked_haiku", "transferable": true,
                    "clears": ["five_hour", "thirty_day"],
                    "percent_used": {"five_hour": 1, "thirty_day": 2}}]},
            "juniper_tide": {"eligible": false, "arm": "double", "price": 1}
        });
        assert_eq!(
            unrecognized(&body),
            [
                "usage.cedar_ember.event_props.cohort",
                "usage.cedar_ember.event_props.tier=claude_ultra",
                "usage.cedar_ember.exhausted=seven_day_haiku",
                "usage.cedar_ember.grants[].clears=thirty_day",
                "usage.cedar_ember.grants[].percent_used.thirty_day",
                "usage.cedar_ember.grants[].transferable",
                "usage.cedar_ember.grants[].type=monthly",
                "usage.cedar_ember.grants[].unavailable_reason=blocked_haiku",
                "usage.cedar_ember.ineligible_reason=region",
                "usage.cedar_ember.queue",
                "usage.juniper_tide.arm=double",
                "usage.juniper_tide.price",
            ]
        );
    }
}

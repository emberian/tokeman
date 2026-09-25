//! Usage-limit resets: Anthropic's programs that refill an account's limits
//! on request.
//!
//! Neither program is publicly documented; this follows Claude Code's own
//! client (2.1.282). Both are read from the profile-usage endpoint and
//! claimed with `POST /api/organizations/{org}/reset_rate_limits`:
//!
//! - `cedar_ember`: *grants*, each worth a fixed number of resets, that refill
//!   the listed windows (`clears`) without moving the weekly reset day. Some
//!   can be used any time, others only at a limit (`use_requires_limit`).
//! - `juniper_tide`: a session (5-hour) reset, offered only at the session
//!   limit and paid for out of the weekly limit.
//!
//! Eligibility depends on the client the server believes it is talking to,
//! which is why every request here identifies as Claude Code's CLI.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::Token;
use crate::probe::client_user_agent;

const API: &str = "https://api.anthropic.com";
/// The read Claude Code makes at a limit: usage plus both programs' blocks.
const STATUS_PATH: &str = "/api/oauth/usage?at_wall=1&skip_spend=1";

#[derive(Debug, Clone, Deserialize)]
pub struct Grant {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub resets_total: u32,
    pub resets_left: u32,
    #[serde(default)]
    pub ends_at: Option<String>,
    /// Windows a reset refills: `five_hour`, `seven_day`,
    /// `seven_day_overage_included` (the Fable limit), ...
    #[serde(default)]
    pub clears: Vec<String>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub usable_now: bool,
    #[serde(default = "yes")]
    pub use_requires_limit: bool,
    /// Current use of each window the grant clears, 0-100.
    #[serde(default)]
    pub percent_used: std::collections::BTreeMap<String, serde_json::Value>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct GrantProgram {
    pub eligible: bool,
    #[serde(default)]
    pub ineligible_reason: Option<String>,
    /// Kept raw: one malformed grant must not hide the others.
    #[serde(default)]
    grants: Vec<serde_json::Value>,
    #[serde(default)]
    pub next_grant_id: Option<String>,
    #[serde(default)]
    pub cooldown_until: Option<String>,
}

impl GrantProgram {
    pub fn grants(&self) -> Vec<Grant> {
        self.grants
            .iter()
            .filter_map(|grant| serde_json::from_value(grant.clone()).ok())
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionProgram {
    #[serde(default)]
    pub ineligible_reason: Option<String>,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub next_available_at: Option<String>,
    #[serde(default)]
    pub resets_per_week: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResetStatus {
    #[serde(default)]
    pub cedar_ember: Option<GrantProgram>,
    #[serde(default)]
    pub juniper_tide: Option<SessionProgram>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaimResponse {
    /// `reset`, `already_used`, `not_limited`, `cooldown`, `ineligible`, or
    /// `unavailable`.
    pub result: String,
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

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

fn profile_token(token: &Token) -> Result<&str> {
    token
        .usage_credential(chrono::Utc::now().timestamp_millis())
        .with_context(|| {
            format!(
                "{} has no live profile credential; run `tokeman login {}`",
                token.name, token.name
            )
        })
}

async fn get_json<T: serde::de::DeserializeOwned>(bearer: &str, path: &str) -> Result<T> {
    let response = client()?
        .get(format!("{API}{path}"))
        .header("Authorization", format!("Bearer {bearer}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", client_user_agent())
        .send()
        .await
        .with_context(|| format!("request to {path} failed"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "{path} answered HTTP {status}: {}",
            crate::text::one_line(&body, 200)
        );
    }
    serde_json::from_str(&body).with_context(|| format!("unreadable {path} response"))
}

pub async fn status(token: &Token) -> Result<ResetStatus> {
    get_json(profile_token(token)?, STATUS_PATH).await
}

async fn organization_uuid(bearer: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Profile {
        organization: Option<Organization>,
    }
    #[derive(Deserialize)]
    struct Organization {
        uuid: String,
    }
    get_json::<Profile>(bearer, "/api/oauth/profile")
        .await?
        .organization
        .map(|organization| organization.uuid)
        .context("the profile names no organization")
}

/// Claim one reset. `grant_id` selects a `cedar_ember` grant; `None` claims
/// the `juniper_tide` session reset. The request id makes a retried claim
/// idempotent on the server, as in Claude Code.
pub async fn claim(token: &Token, grant_id: Option<&str>) -> Result<ClaimResponse> {
    let bearer = profile_token(token)?;
    let organization = organization_uuid(bearer).await?;
    let body = match grant_id {
        Some(grant_id) => serde_json::json!({
            "program": "cedar_ember",
            "grant_id": grant_id,
            "request_id": request_id()?,
        }),
        None => serde_json::json!({ "program": "juniper_tide" }),
    };
    let response = client()?
        .post(format!(
            "{API}/api/organizations/{organization}/reset_rate_limits"
        ))
        .header("Authorization", format!("Bearer {bearer}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", client_user_agent())
        .json(&body)
        .send()
        .await
        .context("reset request failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "reset answered HTTP {status}: {}",
            crate::text::one_line(&text, 200)
        );
    }
    serde_json::from_str(&text).context("unreadable reset response")
}

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

/// One line per grant, e.g. `Claude Opus 5.5 launch: 1/1 left, usable now
/// (clears 5h 7d Fable 7d, now at 0% 0% 0%), use by 2026-10-22`.
pub fn describe_grant(grant: &Grant) -> String {
    let clears = grant
        .clears
        .iter()
        .map(|window| window_label(window))
        .collect::<Vec<_>>()
        .join(" ");
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
    let when = if grant.paused {
        "paused"
    } else if grant.usable_now {
        "usable now"
    } else if grant.use_requires_limit {
        "usable at a limit"
    } else {
        "not usable yet"
    };
    let deadline = grant
        .ends_at
        .as_deref()
        .map(|end| format!(", use by {}", end.get(..10).unwrap_or(end)))
        .unwrap_or_default();
    format!(
        "{}: {}/{} left, {when} (clears {clears}; now at {used}){deadline}",
        if grant.label.is_empty() {
            &grant.id
        } else {
            &grant.label
        },
        grant.resets_left,
        grant.resets_total.max(grant.resets_left),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_status_block_parses_and_describes() {
        let status: ResetStatus = serde_json::from_str(
            r#"{"cedar_ember": {"eligible": true, "ineligible_reason": null, "at_limit": false,
                "exhausted": [], "grants": [{"id": "opus55-launch-promax-20260921",
                "label": "Claude Opus 5.5 launch: one usage-limit reset for Pro and Max",
                "resets_total": 1, "resets_left": 1, "starts_at": "2026-09-22T16:00:00+00:00",
                "ends_at": "2026-10-22T16:00:00+00:00",
                "clears": ["five_hour", "seven_day", "seven_day_overage_included"],
                "paused": false, "usable_now": true, "use_requires_limit": false,
                "percent_used": {"five_hour": 0, "seven_day": 12, "seven_day_overage_included": 0},
                "blocking": [], "arm": null}, {"id": "broken"}],
                "next_grant_id": "opus55-launch-promax-20260921"},
              "juniper_tide": {"eligible": false, "ineligible_reason": "not_at_wall",
                "in_experiment": false, "available": false, "resets_per_week": 1}}"#,
        )
        .unwrap();
        let program = status.cedar_ember.unwrap();
        let grants = program.grants();
        assert_eq!(grants.len(), 1, "a malformed grant is skipped, not fatal");
        assert_eq!(
            describe_grant(&grants[0]),
            "Claude Opus 5.5 launch: one usage-limit reset for Pro and Max: 1/1 left, \
             usable now (clears 5h 7d Fable 7d; now at 0% 12% 0%), use by 2026-10-22"
        );
        let session = status.juniper_tide.unwrap();
        assert_eq!(session.ineligible_reason.as_deref(), Some("not_at_wall"));
    }

    #[test]
    fn request_ids_fit_the_server_format() {
        let id = request_id().unwrap();
        assert!(id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

//! Usage-limit resets: Anthropic's programs that refill an account's limits
//! on request, as Claude Code's client (2.1.282) reads and claims them.
//!
//! Both are read from the profile endpoint's at-wall form and claimed with
//! `POST /api/organizations/{org}/reset_rate_limits`. `cedar_ember` grants
//! refill the listed windows (`clears`) without moving the weekly reset day;
//! `juniper_tide` is a session reset offered at the session limit and paid
//! for out of the weekly limit. Both are eligible only for Claude Code's
//! user agent.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::profile::{Cache, Endpoint};
use super::{Client, describe, one_line};
use crate::account::Account;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetGrant {
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
    pub percent_used: BTreeMap<String, serde_json::Value>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrantProgram {
    pub eligible: bool,
    #[serde(default)]
    pub ineligible_reason: Option<String>,
    /// Raw: one malformed grant must not hide the others.
    #[serde(default)]
    grants: Vec<serde_json::Value>,
    #[serde(default)]
    pub next_grant_id: Option<String>,
    #[serde(default)]
    pub cooldown_until: Option<String>,
}

impl GrantProgram {
    pub fn grants(&self) -> Vec<ResetGrant> {
        self.grants
            .iter()
            .filter_map(|grant| serde_json::from_value(grant.clone()).ok())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResetStatus {
    #[serde(default)]
    pub cedar_ember: Option<GrantProgram>,
    #[serde(default)]
    pub juniper_tide: Option<SessionProgram>,
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

/// Reset offers, from the shared profile-read cache unless `fresh`. `now` is
/// epoch seconds.
pub async fn status(
    client: &Client,
    cache: &Cache,
    account: &Account,
    fresh: bool,
    now: i64,
) -> Result<ResetStatus> {
    let bearer = profile_token(account, now)?;
    let body = cache
        .read(client, &account.name, bearer, Endpoint::AtWall, fresh, now)
        .await?;
    serde_json::from_value(body).context("unreadable reset status")
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
/// the `juniper_tide` session reset. The request id makes a retried claim
/// idempotent on the server, as in Claude Code.
pub async fn claim(
    client: &Client,
    account: &Account,
    grant_id: Option<&str>,
    now: i64,
) -> Result<ClaimResponse> {
    let bearer = profile_token(account, now)?;
    let organization = organization_uuid(client, bearer).await?;
    let body = match grant_id {
        Some(grant_id) => serde_json::json!({
            "program": "cedar_ember",
            "grant_id": grant_id,
            "request_id": request_id()?,
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
    serde_json::from_str(&text).context("unreadable reset response")
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

/// One line per grant, e.g. `Claude Opus 5.5 launch: 1/1 left, usable now
/// (clears 5h 7d Fable 7d; now at 0% 0% 0%), use by 2026-10-22`.
pub fn describe_grant(grant: &ResetGrant) -> String {
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
    use crate::anthropic::testing::serve;
    use axum::Router;
    use axum::routing::{get, post};

    const LIVE_STATUS: &str = r#"{"cedar_ember": {"eligible": true, "ineligible_reason": null, "at_limit": false,
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
        "in_experiment": false, "available": false, "resets_per_week": 1}}"#;

    #[test]
    fn a_live_status_block_parses_and_describes() {
        let status: ResetStatus = serde_json::from_str(LIVE_STATUS).unwrap();
        let program = status.cedar_ember.clone().unwrap();
        let grants = program.grants();
        assert_eq!(grants.len(), 1, "a malformed grant is skipped, not fatal");
        assert_eq!(
            describe_grant(&grants[0]),
            "Claude Opus 5.5 launch: one usage-limit reset for Pro and Max: 1/1 left, \
             usable now (clears 5h 7d Fable 7d; now at 0% 12% 0%), use by 2026-10-22"
        );
        let session = status.juniper_tide.clone().unwrap();
        assert_eq!(session.ineligible_reason.as_deref(), Some("not_at_wall"));
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<ResetStatus>(&json).unwrap(), status);
        assert_eq!(
            serde_json::from_str::<ResetStatus>("{}").unwrap(),
            ResetStatus::default()
        );
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
        assert_eq!(
            describe_grant(&grant),
            "g: 2/2 left, usable at a limit (clears ; now at )"
        );
        let json = serde_json::to_string(&grant).unwrap();
        assert_eq!(serde_json::from_str::<ResetGrant>(&json).unwrap(), grant);
        assert_eq!(window_label("seven_day_opus"), "Opus 7d");
        assert_eq!(window_label("other"), "other");
    }

    fn fake_api() -> Router {
        Router::new()
            .route(
                "/api/oauth/usage",
                get(|query: axum::extract::RawQuery| async move {
                    assert_eq!(query.0.as_deref(), Some("at_wall=1&skip_spend=1"));
                    LIVE_STATUS
                }),
            )
            .route(
                "/api/oauth/profile",
                get(|| async { r#"{"organization": {"uuid": "org-1"}}"#.to_owned() }),
            )
            .route(
                "/api/organizations/{org}/reset_rate_limits",
                post(
                    |axum::extract::Path(org): axum::extract::Path<String>,
                     headers: axum::http::HeaderMap,
                     body: String| async move {
                        assert_eq!(org, "org-1");
                        assert_eq!(
                            headers.get("anthropic-beta").and_then(|v| v.to_str().ok()),
                            Some(super::super::OAUTH_BETA)
                        );
                        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                        let result = match body["program"].as_str() {
                            Some("cedar_ember") => {
                                assert_eq!(body["grant_id"], "g1");
                                assert_eq!(body["request_id"].as_str().unwrap().len(), 32);
                                "reset"
                            }
                            _ => "not_limited",
                        };
                        serde_json::json!({"result": result, "cleared": ["five_hour"]}).to_string()
                    },
                ),
            )
    }

    #[tokio::test]
    async fn status_and_claims_go_through_the_profile_token() {
        let client = serve(fake_api()).await;
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::at(dir.path().join("reads.json"));
        let mut account = Account::new("acct");
        account.setup_key = Some("sk-ant-oat01-setup".into());
        let error = status(&client, &cache, &account, false, 1)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("no live profile credential"),
            "{error}"
        );

        account.capture_profile_token("profile".into());
        let status = status(&client, &cache, &account, false, 1).await.unwrap();
        assert_eq!(status.cedar_ember.unwrap().grants().len(), 1);
        assert!(
            cache
                .read(&client, "acct", "profile", Endpoint::AtWall, false, 2)
                .await
                .is_ok(),
            "the at-wall read is cached under its own key"
        );

        let claimed = claim(&client, &account, Some("g1"), 1).await.unwrap();
        assert_eq!(claimed.result, ClaimResult::Reset);
        assert_eq!(claimed.cleared, vec!["five_hour"]);
        let session = claim(&client, &account, None, 1).await.unwrap();
        assert_eq!(session.result, ClaimResult::NotLimited);
    }
}

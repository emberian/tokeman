//! Rate-limit reset credits: list them, and claim one on an account that is
//! at a limit. A claim carries a `redeem_request_id` the backend treats as an
//! idempotency key; the id is written to the claim journal
//! ([`crate::claims`]) before the request goes out and removed only on a
//! definite answer, so a retry after a lost answer reuses it and cannot spend
//! a second credit.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

use super::Client;
use super::auth::CodexHome;
use super::probe::{self, State, epoch_of};
use crate::account::CodexAccount;
use crate::claims::{Conflict, Journal, Pending};
use crate::drift::Unrecognized;
use crate::quota::{Detail, ReadError, Reading, Service};

/// Where a credit stands. Spellings the backend adds later are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum CreditStatus {
    Available,
    Redeeming,
    Redeemed,
    Other(String),
}

impl CreditStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Available => "available",
            Self::Redeeming => "redeeming",
            Self::Redeemed => "redeemed",
            Self::Other(raw) => raw,
        }
    }
}

impl From<String> for CreditStatus {
    fn from(raw: String) -> Self {
        match raw.as_str() {
            "available" => Self::Available,
            "redeeming" => Self::Redeeming,
            "redeemed" => Self::Redeemed,
            _ => Self::Other(raw),
        }
    }
}

impl From<CreditStatus> for String {
    fn from(status: CreditStatus) -> Self {
        status.as_str().to_owned()
    }
}

/// One reset credit, from the list endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetCredit {
    pub id: String,
    /// `codex_rate_limits` for the credits seen so far.
    pub reset_type: Option<String>,
    pub status: CreditStatus,
    /// Epoch seconds.
    pub granted_at: Option<i64>,
    /// Epoch seconds.
    pub expires_at: Option<i64>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub supported_by_plan: Option<bool>,
}

/// `GET /wham/rate-limit-reset-credits`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CreditList {
    pub available: i64,
    pub credits: Vec<ResetCredit>,
    /// Keys and words of the answer outside the known sets
    /// ([`crate::drift`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unrecognized: Vec<String>,
}

// The known sets of the credit list: what this parser reads, then the
// fields the live answer of 2026-11-11 carried beyond the client source
// (research/codex.md, "Reset credits").

const KNOWN_LIST_KEYS: &[&str] = &[
    "available_count",
    "credits",
    "total_earned_count",
    "immediate_reset_purchase_eligible",
    "history_enabled",
];
const KNOWN_CREDIT_KEYS: &[&str] = &[
    "id",
    "reset_type",
    "status",
    "granted_at",
    "expires_at",
    "title",
    "description",
    "is_supported_by_plan",
    "redeem_started_at",
    "redeemed_at",
    "profile_image_url",
    "profile_user_id",
];
const KNOWN_CREDIT_STATUSES: &[&str] = &["available", "redeeming", "redeemed"];
const KNOWN_RESET_TYPES: &[&str] = &["codex_rate_limits"];

/// The keys and words of a credit list that are outside the known sets.
fn unrecognized(body: &Value) -> Vec<String> {
    let mut seen = Unrecognized::new();
    seen.keys("reset-credits", body, KNOWN_LIST_KEYS);
    let credits = &body["credits"];
    seen.row_keys("reset-credits.credits", credits, KNOWN_CREDIT_KEYS);
    for credit in credits.as_array().into_iter().flatten() {
        seen.value(
            "reset-credits.credits[].status",
            credit.get("status"),
            KNOWN_CREDIT_STATUSES,
        );
        seen.value(
            "reset-credits.credits[].reset_type",
            credit.get("reset_type"),
            KNOWN_RESET_TYPES,
        );
    }
    seen.into_vec()
}

impl CreditList {
    pub fn find(&self, id: &str) -> Option<&ResetCredit> {
        self.credits.iter().find(|credit| credit.id == id)
    }

    /// Epoch seconds when the first available credit to expire does.
    pub fn next_expiry(&self) -> Option<i64> {
        self.credits
            .iter()
            .filter(|credit| credit.status == CreditStatus::Available)
            .filter_map(|credit| credit.expires_at)
            .min()
    }

    /// Put [`Self::next_expiry`] beside the count in `probe`.
    pub fn date(&self, probe: &mut Reading) {
        if let Detail::Codex(codex) = &mut probe.detail
            && let Some(count) = &mut codex.reset_credits
        {
            count.expires_at = self.next_expiry();
        }
    }
}

#[derive(Deserialize)]
struct RawCredit {
    id: String,
    reset_type: Option<String>,
    status: Option<String>,
    granted_at: Option<Value>,
    expires_at: Option<Value>,
    title: Option<String>,
    description: Option<String>,
    is_supported_by_plan: Option<bool>,
}

#[derive(Deserialize)]
struct RawCreditList {
    available_count: Option<i64>,
    #[serde(default)]
    credits: Vec<RawCredit>,
}

pub fn decode_credit_list(body: &str) -> Result<CreditList, ReadError> {
    let decode = |error: serde_json::Error| ReadError::Decode {
        message: format!("failed to decode reset credit list: {error}"),
    };
    let value: Value = serde_json::from_str(body).map_err(decode)?;
    let unrecognized = unrecognized(&value);
    let raw: RawCreditList = serde_json::from_value(value).map_err(decode)?;
    let credits: Vec<ResetCredit> = raw
        .credits
        .into_iter()
        .map(|credit| ResetCredit {
            id: credit.id,
            reset_type: credit.reset_type,
            status: credit.status.unwrap_or_default().into(),
            granted_at: epoch_of(credit.granted_at.as_ref()),
            expires_at: epoch_of(credit.expires_at.as_ref()),
            title: credit.title,
            description: credit.description,
            supported_by_plan: credit.is_supported_by_plan,
        })
        .collect();
    let available = raw.available_count.unwrap_or_else(|| {
        credits
            .iter()
            .filter(|credit| credit.status == CreditStatus::Available)
            .count() as i64
    });
    Ok(CreditList {
        available,
        credits,
        unrecognized,
    })
}

/// List an account's reset credits. The endpoint is less reliable than the
/// usage body (it has answered 500); callers fall back to
/// `reset_credits` in the reading's Codex detail.
pub async fn list(client: &Client, account: &CodexAccount) -> Result<CreditList, ReadError> {
    let auth = CodexHome::new(account.home())
        .read()
        .map_err(|error| ReadError::Credential {
            message: format!("{error:#}"),
        })?;
    let body = probe::call(
        client,
        &account.name,
        &auth,
        reqwest::Method::GET,
        client.credits_url(),
        None,
    )
    .await?;
    decode_credit_list(&body)
}

/// Read the credit list of every account whose probe counts reset credits
/// and put the first expiry beside the count. A list that cannot be read
/// leaves the count as the usage body gave it.
pub async fn with_expiry(client: &Client, accounts: &[CodexAccount], probes: &mut [Reading]) {
    let lists = futures::future::join_all(probes.iter().map(|probe| async {
        let holds = probe
            .codex()
            .and_then(|codex| codex.reset_credits)
            .is_some_and(|count| count.available > 0);
        let account = accounts
            .iter()
            .find(|account| account.name == probe.account)?;
        if !holds {
            return None;
        }
        list(client, account).await.ok()
    }))
    .await;
    for (probe, list) in probes.iter_mut().zip(lists) {
        if let Some(list) = list {
            list.date(probe);
        }
    }
}

/// The backend's answer to a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumeCode {
    /// A credit was spent and the limit cleared.
    Reset,
    /// Nothing was at a limit; no credit was spent.
    NothingToReset,
    /// The account has no credit to spend.
    NoCredit,
    /// This request id was already redeemed: an earlier attempt went through.
    AlreadyRedeemed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumed {
    pub code: ConsumeCode,
    #[serde(default)]
    pub windows_reset: i64,
}

/// Why a reset is not offered.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    Unreadable(ReadError),
    /// Nothing to clear: a credit spent now would be wasted.
    NotLimited {
        state: State,
    },
    NoApplicableCredit {
        available: Option<i64>,
        applicable: Option<i64>,
    },
    UnknownCredit {
        id: String,
    },
    CreditNotAvailable {
        id: String,
        status: CreditStatus,
    },
    /// A claim for another credit is still unanswered.
    PendingForOtherCredit {
        pending: Option<String>,
        requested: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(error) => write!(f, "cannot read the account's usage: {error}"),
            Self::NotLimited { state } => write!(
                f,
                "the account is {state}, not at a limit; a reset credit would clear nothing"
            ),
            Self::NoApplicableCredit {
                available,
                applicable,
            } => write!(
                f,
                "no reset credit applies right now ({} held, {} applicable)",
                available.map_or("unknown".to_owned(), |n| n.to_string()),
                applicable.map_or("unknown".to_owned(), |n| n.to_string()),
            ),
            Self::UnknownCredit { id } => write!(f, "no reset credit {id} on this account"),
            Self::CreditNotAvailable { id, status } => {
                write!(f, "reset credit {id} is {}", status.as_str())
            }
            Self::PendingForOtherCredit { pending, requested } => write!(
                f,
                "an unanswered claim for {} is pending; re-run without --credit (or with --credit {}) to retry it before claiming {requested}",
                pending
                    .as_deref()
                    .unwrap_or("the backend's choice of credit"),
                pending.as_deref().unwrap_or("<none>"),
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// What a claim would do, shown before asking.
#[derive(Debug, Clone)]
pub struct ResetPlan {
    pub account: CodexAccount,
    pub probe: Reading,
    /// The list endpoint's answer; the count in the usage body stands in
    /// when it fails.
    pub credits: Result<CreditList, ReadError>,
    /// The credit asked for; `None` lets the backend choose.
    pub credit_id: Option<String>,
    /// An earlier claim without a definite answer; this one retries it.
    pub pending: Option<Pending>,
}

impl ResetPlan {
    /// The credit asked for, when the list names it.
    pub fn chosen(&self) -> Option<&ResetCredit> {
        let id = self.credit_id.as_deref()?;
        self.credits.as_ref().ok()?.find(id)
    }
}

/// Check that a claim makes sense now. A pending claim is a retry and skips
/// the limit check: if the first attempt went through, the account is no
/// longer limited, and the retry only learns that.
pub async fn plan(
    client: &Client,
    account: &CodexAccount,
    credit: Option<&str>,
    journal: &Journal,
) -> Result<ResetPlan> {
    let pending = journal.pending(Service::Codex, &account.name)?;
    let probe = probe::probe(client, account).await;
    let credits = list(client, account).await;
    let credit_id = credit
        .map(str::to_owned)
        .or_else(|| pending.as_ref().and_then(|claim| claim.item.clone()));
    let plan = ResetPlan {
        account: account.clone(),
        probe,
        credits,
        credit_id,
        pending,
    };
    if let Some(pending) = &plan.pending {
        if credit.is_some() && credit != pending.item.as_deref() {
            bail!(Refusal::PendingForOtherCredit {
                pending: pending.item.clone(),
                requested: credit.unwrap_or_default().to_owned(),
            });
        }
        return Ok(plan);
    }
    if let Some(error) = &plan.probe.error {
        bail!(Refusal::Unreadable(error.clone()));
    }
    if plan.probe.accepts_work() {
        bail!(Refusal::NotLimited {
            state: State::of(&plan.probe)
        });
    }
    let count = plan.probe.codex().and_then(|codex| codex.reset_credits);
    let applicable = count.and_then(|count| count.applicable);
    if applicable.is_none_or(|applicable| applicable <= 0) {
        bail!(Refusal::NoApplicableCredit {
            available: count.map(|count| count.available),
            applicable,
        });
    }
    if let (Some(id), Ok(list)) = (&plan.credit_id, &plan.credits) {
        match list.find(id) {
            None => bail!(Refusal::UnknownCredit { id: id.clone() }),
            Some(credit) if credit.status != CreditStatus::Available => {
                bail!(Refusal::CreditNotAvailable {
                    id: id.clone(),
                    status: credit.status.clone(),
                })
            }
            Some(_) => {}
        }
    }
    Ok(plan)
}

/// How a claim ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Claim {
    /// The backend answered; the journal entry is gone.
    Answered {
        redeem_request_id: String,
        consumed: Consumed,
        /// This was a retry of an earlier unanswered claim.
        retried: bool,
    },
    /// No definite answer. The id stays in the journal and the next claim on
    /// this account resends it.
    Uncertain {
        redeem_request_id: String,
        error: ReadError,
    },
}

/// Send the claim a [`plan`] described.
pub async fn claim(
    client: &Client,
    plan: &ResetPlan,
    journal: &Journal,
    now: i64,
) -> Result<Claim> {
    let account = &plan.account;
    let (pending, fresh) = journal
        .begin(
            Service::Codex,
            &account.name,
            plan.credit_id.as_deref(),
            now,
        )
        .map_err(|error| match error.downcast::<Conflict>() {
            Ok(conflict) => anyhow::Error::new(Refusal::PendingForOtherCredit {
                pending: conflict.pending.item,
                requested: conflict.requested,
            }),
            Err(error) => error,
        })?;
    let mut body = json!({ "redeem_request_id": pending.request_id });
    if let Some(credit_id) = &pending.item {
        body["credit_id"] = json!(credit_id);
    }
    let answer = match CodexHome::new(account.home()).read() {
        Err(error) => Err(ReadError::Credential {
            message: format!("{error:#}"),
        }),
        Ok(auth) => probe::call(
            client,
            &account.name,
            &auth,
            reqwest::Method::POST,
            &client.consume_url(),
            Some(&body),
        )
        .await
        .and_then(|text| {
            serde_json::from_str::<Consumed>(&text).map_err(|error| ReadError::Decode {
                message: format!("unreadable reset answer: {error}"),
            })
        }),
    };
    match answer {
        Ok(consumed) => {
            journal.finish(Service::Codex, &account.name, &pending.request_id)?;
            Ok(Claim::Answered {
                redeem_request_id: pending.request_id,
                consumed,
                retried: !fresh,
            })
        }
        Err(error) => Ok(Claim::Uncertain {
            redeem_request_id: pending.request_id,
            error,
        }),
    }
}

/// `Full reset (available), expires 2026-11-30 19:50Z, id RateLimitResetCredit_…`.
pub fn describe(credit: &ResetCredit) -> String {
    let mut text = format!(
        "{} ({})",
        credit.title.as_deref().unwrap_or("reset credit"),
        credit.status.as_str()
    );
    if let Some(expires) = credit
        .expires_at
        .and_then(|at| DateTime::<Utc>::from_timestamp(at, 0))
    {
        text.push_str(&format!(", expires {}", expires.format("%Y-%m-%d %H:%MZ")));
    }
    if credit.supported_by_plan == Some(false) {
        text.push_str(", not supported by the plan");
    }
    text.push_str(&format!(", id {}", credit.id));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::probe::tests::{
        Backend, limited_body, ok, profile, status, weekly_only_body,
    };
    use axum::http::StatusCode;

    fn journal(dir: &std::path::Path) -> Journal {
        Journal::at(dir.join("journal"))
    }

    /// The list body as the backend sent it on 2026-11-11, ids shortened.
    pub(crate) fn credit_list_body() -> Value {
        json!({
            "credits": [
                {"id": "RateLimitResetCredit_a", "reset_type": "codex_rate_limits",
                 "is_supported_by_plan": true, "status": "available",
                 "granted_at": "2026-10-31T19:50:50.360127Z", "expires_at": "2026-11-30T19:50:50.360127Z",
                 "redeem_started_at": null, "redeemed_at": null,
                 "title": "Full reset", "description": "One free rate limit reset."},
                {"id": "RateLimitResetCredit_b", "reset_type": "codex_rate_limits",
                 "is_supported_by_plan": true, "status": "redeemed",
                 "granted_at": "2026-11-01T22:26:26.750129Z", "expires_at": null,
                 "title": "Full reset", "description": null}
            ],
            "available_count": 1,
            "total_earned_count": 0,
            "immediate_reset_purchase_eligible": false,
            "history_enabled": true
        })
    }

    #[test]
    fn the_list_decodes_with_timestamps_and_unknown_statuses_kept() {
        let list = decode_credit_list(&credit_list_body().to_string()).unwrap();
        assert_eq!(list.available, 1);
        assert_eq!(list.credits.len(), 2);
        let first = &list.credits[0];
        assert_eq!(first.status, CreditStatus::Available);
        assert_eq!(first.granted_at, Some(1_793_476_250));
        assert_eq!(first.expires_at, Some(1_796_068_250));
        assert_eq!(first.supported_by_plan, Some(true));
        assert_eq!(list.credits[1].status, CreditStatus::Redeemed);
        assert_eq!(list.credits[1].expires_at, None);
        assert_eq!(
            describe(first),
            "Full reset (available), expires 2026-11-30 19:50Z, id RateLimitResetCredit_a"
        );
        let odd = decode_credit_list(r#"{"credits":[{"id":"x","status":"frozen"}]}"#).unwrap();
        assert_eq!(odd.credits[0].status, CreditStatus::Other("frozen".into()));
        assert_eq!(
            odd.available, 0,
            "counted from the list when the total is missing"
        );
        assert!(decode_credit_list("<html>").is_err());
    }

    #[test]
    fn consume_codes_use_the_backend_spellings() {
        for (text, code) in [
            ("reset", ConsumeCode::Reset),
            ("nothing_to_reset", ConsumeCode::NothingToReset),
            ("no_credit", ConsumeCode::NoCredit),
            ("already_redeemed", ConsumeCode::AlreadyRedeemed),
        ] {
            let consumed: Consumed =
                serde_json::from_str(&format!(r#"{{"code":"{text}"}}"#)).unwrap();
            assert_eq!(
                consumed,
                Consumed {
                    code,
                    windows_reset: 0
                }
            );
        }
    }

    fn consume_answer(code: &str) -> (StatusCode, String) {
        (
            StatusCode::OK,
            json!({"code": code, "windows_reset": 2}).to_string(),
        )
    }

    #[tokio::test]
    async fn an_unlimited_account_or_one_without_applicable_credits_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let journal = journal(dir.path());

        let backend = Backend::default()
            .usage_answers(vec![ok(weekly_only_body(42.0))])
            .credit_answers(vec![(StatusCode::OK, credit_list_body().to_string())]);
        let client = backend.client().await;
        let error = plan(&client, &account, None, &journal).await.unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<Refusal>(),
                Some(Refusal::NotLimited { .. })
            ),
            "{error}"
        );

        let mut body = limited_body();
        body["rate_limit_reset_credits"]["applicable_available_count"] = json!(0);
        let backend = Backend::default()
            .usage_answers(vec![ok(body)])
            .credit_answers(vec![(StatusCode::INTERNAL_SERVER_ERROR, "{}".into())]);
        let client = backend.client().await;
        let error = plan(&client, &account, None, &journal).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<Refusal>(),
            Some(&Refusal::NoApplicableCredit {
                available: Some(2),
                applicable: Some(0)
            })
        );

        let backend = Backend::default()
            .usage_answers(vec![ok(limited_body())])
            .credit_answers(vec![(StatusCode::OK, credit_list_body().to_string())]);
        let client = backend.client().await;
        for (id, expected) in [
            ("nope", "no reset credit nope on this account"),
            (
                "RateLimitResetCredit_b",
                "reset credit RateLimitResetCredit_b is redeemed",
            ),
        ] {
            let error = plan(&client, &account, Some(id), &journal)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
        assert!(backend.requests("/credits/consume").is_empty());

        // An account whose usage cannot be read is not judged limited.
        let backend = Backend::default()
            .usage_answers(vec![status(StatusCode::UNAUTHORIZED, "{}")])
            .credit_answers(vec![(StatusCode::OK, credit_list_body().to_string())]);
        let client = backend.client().await;
        let error = plan(&client, &account, None, &journal).await.unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<Refusal>(),
                Some(Refusal::Unreadable(ReadError::Http { status: 401, .. }))
            ),
            "{error}"
        );
        assert_eq!(
            journal.pending(Service::Codex, "one").unwrap(),
            None,
            "a refusal mints nothing"
        );
    }

    #[tokio::test]
    async fn a_claim_sends_one_id_and_clears_it_on_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let journal = journal(dir.path());
        let backend = Backend::default()
            .usage_answers(vec![ok(limited_body())])
            .credit_answers(vec![(StatusCode::OK, credit_list_body().to_string())])
            .consume_answers(vec![consume_answer("reset")]);
        let client = backend.client().await;
        let plan = plan(&client, &account, Some("RateLimitResetCredit_a"), &journal)
            .await
            .unwrap();
        assert_eq!(plan.chosen().unwrap().title.as_deref(), Some("Full reset"));
        assert!(plan.pending.is_none());
        let outcome = claim(&client, &plan, &journal, 100).await.unwrap();
        let Claim::Answered {
            redeem_request_id,
            consumed,
            retried,
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(consumed.code, ConsumeCode::Reset);
        assert_eq!(consumed.windows_reset, 2);
        assert!(!retried);
        let sent = backend.requests("/credits/consume");
        assert_eq!(sent.len(), 1);
        let body: Value = serde_json::from_str(&sent[0].body).unwrap();
        assert_eq!(
            body,
            json!({"redeem_request_id": redeem_request_id, "credit_id": "RateLimitResetCredit_a"})
        );
        let header = |name: &str| {
            sent[0]
                .headers
                .get(name)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(header("authorization"), "Bearer access-one");
        assert_eq!(header("chatgpt-account-id"), "acct-one");
        assert_eq!(journal.pending(Service::Codex, "one").unwrap(), None);
    }

    #[tokio::test]
    async fn an_unanswered_claim_is_retried_with_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let journal = journal(dir.path());
        let backend = Backend::default()
            // After the first attempt went through, the account is no longer
            // limited and no credit applies; the retry must still be allowed.
            .usage_answers(vec![ok(limited_body()), ok(weekly_only_body(0.0))])
            .credit_answers(vec![(StatusCode::INTERNAL_SERVER_ERROR, "{}".into())])
            .consume_answers(vec![
                (StatusCode::BAD_GATEWAY, "upstream".into()),
                consume_answer("already_redeemed"),
            ]);
        let client = backend.client().await;

        let first_plan = plan(&client, &account, None, &journal).await.unwrap();
        assert!(
            first_plan.credits.is_err(),
            "the list failing does not block a claim"
        );
        let first = claim(&client, &first_plan, &journal, 100).await.unwrap();
        let Claim::Uncertain {
            redeem_request_id: first_id,
            error,
        } = first
        else {
            panic!("{first:?}");
        };
        assert_eq!(
            error,
            ReadError::Http {
                status: 502,
                body: "upstream".into()
            }
        );
        assert_eq!(
            journal
                .pending(Service::Codex, "one")
                .unwrap()
                .unwrap()
                .request_id,
            first_id
        );

        let retry_plan = plan(&client, &account, None, &journal).await.unwrap();
        assert_eq!(retry_plan.pending.as_ref().unwrap().request_id, first_id);
        // Naming another credit while that claim is unanswered is refused
        // before any request.
        let other = plan(&client, &account, Some("RateLimitResetCredit_a"), &journal)
            .await
            .unwrap_err();
        assert!(matches!(
            other.downcast_ref::<Refusal>(),
            Some(Refusal::PendingForOtherCredit { pending: None, .. })
        ));
        let mut stale = retry_plan.clone();
        stale.credit_id = Some("RateLimitResetCredit_a".into());
        let other = claim(&client, &stale, &journal, 150).await.unwrap_err();
        assert!(matches!(
            other.downcast_ref::<Refusal>(),
            Some(Refusal::PendingForOtherCredit { .. })
        ));
        assert_eq!(backend.requests("/credits/consume").len(), 1);
        let retry = claim(&client, &retry_plan, &journal, 200).await.unwrap();
        assert_eq!(
            retry,
            Claim::Answered {
                redeem_request_id: first_id.clone(),
                consumed: Consumed {
                    code: ConsumeCode::AlreadyRedeemed,
                    windows_reset: 2
                },
                retried: true,
            }
        );
        let ids: Vec<String> = backend
            .requests("/credits/consume")
            .iter()
            .map(|seen| {
                serde_json::from_str::<Value>(&seen.body).unwrap()["redeem_request_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(ids, vec![first_id.clone(), first_id]);
        assert_eq!(journal.pending(Service::Codex, "one").unwrap(), None);
    }

    #[tokio::test]
    async fn the_first_expiry_is_put_beside_the_count_only_where_credits_are_held() {
        let dir = tempfile::tempdir().unwrap();
        let holder = profile(dir.path(), "holder", "r1");
        let none = profile(dir.path(), "none", "r2");
        let mut empty = weekly_only_body(5.0);
        empty["rate_limit_reset_credits"]["available_count"] = json!(0);
        let mut list = credit_list_body();
        // A redeemed credit's date is not an expiry of anything on offer,
        // and of two available credits the earlier date counts.
        list["credits"][1]["expires_at"] = json!("2026-11-12T00:00:00Z");
        list["credits"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "c", "status": "available", "expires_at": "2026-11-20T00:00:00Z"}));
        let backend = Backend::default()
            .usage_answers(vec![
                ok(weekly_only_body(5.0)),
                ok(empty),
                ok(weekly_only_body(5.0)),
            ])
            .credit_answers(vec![
                (StatusCode::OK, list.to_string()),
                (StatusCode::INTERNAL_SERVER_ERROR, "oops".into()),
            ]);
        let client = backend.client().await;
        let accounts = [holder, none];
        let mut probes = probe::probe_all(&client, &accounts).await;
        let expiry = |probes: &[Reading], index: usize| {
            probes[index]
                .codex()
                .and_then(|codex| codex.reset_credits)
                .and_then(|count| count.expires_at)
        };
        with_expiry(&client, &accounts, &mut probes).await;
        assert_eq!(backend.requests("/credits").len(), 1, "only the holder");
        let nov_20 = DateTime::parse_from_rfc3339("2026-11-20T00:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(expiry(&probes, 0), Some(nov_20));
        assert_eq!(expiry(&probes, 1), None);
        // A list that cannot be read leaves the count as it was.
        let mut again = probe::probe_all(&client, &accounts[..1]).await;
        with_expiry(&client, &accounts, &mut again).await;
        assert_eq!(expiry(&again, 0), None);
        assert_eq!(
            again[0].codex().unwrap().reset_credits.unwrap().available,
            2
        );
    }

    #[tokio::test]
    async fn an_unreadable_answer_keeps_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let journal = journal(dir.path());
        let backend = Backend::default()
            .usage_answers(vec![ok(limited_body())])
            .credit_answers(vec![(StatusCode::OK, credit_list_body().to_string())])
            .consume_answers(vec![(StatusCode::OK, "<html>".into())]);
        let client = backend.client().await;
        let plan = plan(&client, &account, None, &journal).await.unwrap();
        let outcome = claim(&client, &plan, &journal, 1).await.unwrap();
        assert!(matches!(
            outcome,
            Claim::Uncertain {
                error: ReadError::Decode { .. },
                ..
            }
        ));
        assert!(journal.pending(Service::Codex, "one").unwrap().is_some());
    }

    #[tokio::test]
    async fn a_claim_left_pending_by_an_earlier_build_is_honoured() {
        let dir = tempfile::tempdir().unwrap();
        let account = profile(dir.path(), "one", "r1");
        let journal_dir = dir.path().join("journal");
        std::fs::create_dir_all(&journal_dir).unwrap();
        crate::claims::tests::legacy_codex_fixture(&journal_dir);
        let journal = journal(dir.path());
        let backend = Backend::default()
            .usage_answers(vec![ok(weekly_only_body(0.0))])
            .credit_answers(vec![(StatusCode::OK, credit_list_body().to_string())])
            .consume_answers(vec![consume_answer("already_redeemed")]);
        let client = backend.client().await;

        // Another credit is refused while the old claim is unanswered.
        let error = plan(&client, &account, Some("RateLimitResetCredit_b"), &journal)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Refusal>(),
            Some(&Refusal::PendingForOtherCredit {
                pending: Some("RateLimitResetCredit_a".into()),
                requested: "RateLimitResetCredit_b".into(),
            })
        );
        assert!(backend.requests("/credits/consume").is_empty());

        // The retry resends the id and the credit the old file held.
        let retry = plan(&client, &account, None, &journal).await.unwrap();
        assert_eq!(retry.credit_id.as_deref(), Some("RateLimitResetCredit_a"));
        let outcome = claim(&client, &retry, &journal, 1).await.unwrap();
        assert!(matches!(outcome, Claim::Answered { retried: true, .. }));
        let sent = backend.requests("/credits/consume");
        assert_eq!(
            serde_json::from_str::<Value>(&sent[0].body).unwrap(),
            json!({"redeem_request_id": "0b0e3c1a-5a52-4c8e-9d0a-7f3e2b1c4d5e",
                "credit_id": "RateLimitResetCredit_a"})
        );
        assert_eq!(journal.pending(Service::Codex, "one").unwrap(), None);
        // The other account's old claim is still there.
        assert!(journal.pending(Service::Codex, "two").unwrap().is_some());
    }

    #[test]
    fn the_list_reports_keys_statuses_and_reset_types_it_does_not_know() {
        let list = decode_credit_list(&credit_list_body().to_string()).unwrap();
        assert_eq!(list.unrecognized, Vec::<String>::new());

        let mut body = credit_list_body();
        body["next_page"] = json!(null);
        body["credits"][0]["source"] = json!("promo");
        body["credits"][0]["status"] = json!("expired");
        body["credits"][1]["reset_type"] = json!("codex_weekly_only");
        let list = decode_credit_list(&body.to_string()).unwrap();
        assert_eq!(
            list.unrecognized,
            [
                "reset-credits.credits[].reset_type=codex_weekly_only",
                "reset-credits.credits[].source",
                "reset-credits.credits[].status=expired",
                "reset-credits.next_page",
            ]
        );
        assert_eq!(
            list.credits[0].status,
            CreditStatus::Other("expired".into()),
            "and the list still reads"
        );
    }
}

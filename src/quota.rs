//! What is known about an account's room: the windows a probe reports, the
//! per-model weekly buckets, and how a model name maps onto those buckets.

use serde::{Deserialize, Serialize};
use std::fmt;

/// One rate-limit window. `used` is a fraction, `resets_at` epoch seconds
/// (0 when the server sent no reset).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    #[serde(rename = "utilization")]
    pub used: f64,
    #[serde(rename = "reset", default)]
    pub resets_at: i64,
}

impl Window {
    pub fn new(used: f64, resets_at: i64) -> Self {
        Self { used, resets_at }
    }

    /// Fraction left, unclamped.
    pub fn remaining(&self) -> f64 {
        1.0 - self.used
    }

    /// Whether the window has at least `floor` of its capacity left.
    /// Compared on the used side: `1.0 - 0.95` can exceed `0.05` by a rounding
    /// error and make the exact boundary look viable.
    pub fn above(&self, floor: f64) -> bool {
        self.used < 1.0 - floor
    }
}

/// How much of a window is left, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Low,
    Critical,
}

impl Level {
    pub fn of(remaining: f64) -> Self {
        if remaining > 0.50 {
            Self::Ok
        } else if remaining > 0.20 {
            Self::Low
        } else {
            Self::Critical
        }
    }
}

/// A model family with its own weekly limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Opus,
    Sonnet,
    Fable,
}

impl Family {
    pub const ALL: [Family; 3] = [Family::Opus, Family::Sonnet, Family::Fable];

    pub fn name(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Sonnet => "sonnet",
            Self::Fable => "fable",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Sonnet => "Sonnet",
            Self::Fable => "Fable",
        }
    }

    /// The family a model id or display name belongs to.
    pub fn of(model: &str) -> Option<Self> {
        let lower = model.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|family| lower.contains(family.name()))
    }
}

/// The canonical spelling of a model: ASCII alphanumerics, lower case, without
/// a leading `claude`. `claude-opus-4-8`, `Opus 4.8` and `opus48` are all
/// `opus48`; `claude-fable-5-1[1m]` is `fable51`; a family alone (`fable`)
/// names its family-wide bucket.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelKey(String);

impl ModelKey {
    pub fn new(model: &str) -> Self {
        let base = model.split('[').next().unwrap_or(model);
        let key: String = base
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        Self(match key.strip_prefix("claude") {
            Some(rest) if !rest.is_empty() => rest.to_owned(),
            _ => key,
        })
    }

    pub fn family_wide(family: Family) -> Self {
        Self(family.name().to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn family(&self) -> Option<Family> {
        Family::of(&self.0)
    }

    /// The version digits after the family name: `48` for Opus 4.8, `5` for
    /// a major-only name, `None` for a family-wide key. Claude Code's bare
    /// `opus` alias selects Opus 5, so it reads as version `5` rather than
    /// as the whole family: an exhausted Opus 4.8 bucket must not poison it.
    pub fn version(&self) -> Option<&str> {
        let family = self.family()?;
        let start = self.0.find(family.name())? + family.name().len();
        let digits = &self.0[start..];
        let digits = digits.trim_end_matches(|c: char| !c.is_ascii_digit());
        let digits: &str = digits
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("");
        if digits.is_empty() {
            (family == Family::Opus && self.0 == "opus").then_some("5")
        } else {
            Some(digits)
        }
    }

    /// Whether a bucket keyed `bucket` limits sessions on this model.
    ///
    /// Same family and: the bucket is family-wide; or the bucket names a
    /// major only (`fable5`) and this model is one of its point releases; or
    /// both name the same version. Keys outside any family match by equality.
    pub fn is_limited_by(&self, bucket: &ModelKey) -> bool {
        match (self.family(), bucket.family()) {
            (Some(mine), Some(theirs)) if mine == theirs => {
                match (self.version(), bucket.version()) {
                    (_, None) => true,
                    (Some(mine), Some(theirs)) if theirs.len() == 1 => mine.starts_with(theirs),
                    (mine, theirs) => mine == theirs,
                }
            }
            (Some(_), Some(_)) => false,
            _ => !self.0.is_empty() && self.0 == bucket.0,
        }
    }
}

impl fmt::Display for ModelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a bucket's reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// `/api/oauth/usage`, which needs a full-scope login.
    Profile,
    /// Rate-limit headers of a probe made with that model.
    Headers,
    /// A rejection seen in a Claude Code transcript.
    #[serde(rename = "observed-rejection")]
    Observed,
    /// An earlier reading carried forward until its window resets.
    Carried,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Headers => "rate-limit headers",
            Self::Observed => "observed Claude rejection",
            Self::Carried => "carried sample",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Headers => "headers",
            Self::Observed => "observed!",
            Self::Carried => "carried",
        }
    }
}

/// One per-model weekly limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub key: ModelKey,
    pub label: String,
    pub window: Window,
    pub source: Source,
}

/// What the probe's `unified-status` header said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allowed,
    AllowedWarning,
    Rejected,
    Unknown,
}

impl Verdict {
    pub fn parse(status: &str) -> Self {
        match status {
            "allowed" => Self::Allowed,
            "allowed_warning" => Self::AllowedWarning,
            "rejected" => Self::Rejected,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::AllowedWarning => "allowed_warning",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
        }
    }

    pub fn allows(self) -> bool {
        matches!(self, Self::Allowed | Self::AllowedWarning)
    }
}

/// Extra usage beyond the subscription, when the account has it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Overage {
    pub status: Option<String>,
    pub window: Option<Window>,
    pub disabled_reason: Option<String>,
    pub in_use: bool,
}

/// Why a probe produced no quota.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeError {
    /// The account offers nothing to probe with.
    NoCredential,
    /// The request never completed.
    Network { message: String },
    /// The API answered without quota headers.
    Http { status: u16, body: String },
}

impl ProbeError {
    /// 401/403 or no credential: the account will stay refused until its
    /// credential changes, unlike a timeout or a 5xx.
    pub fn is_definitive(&self) -> bool {
        match self {
            Self::NoCredential => true,
            Self::Http { status, .. } => matches!(status, 401 | 403),
            Self::Network { .. } => false,
        }
    }

    /// A few words that group accounts by cause in a summary line.
    pub fn reason(&self) -> String {
        match self {
            Self::NoCredential => "no usable credential".into(),
            Self::Network { .. } => "network".into(),
            Self::Http { status, body } => {
                let lower = body.to_ascii_lowercase();
                let cause = if lower.contains("revoked") {
                    Some("revoked")
                } else if lower.contains("has expired") {
                    Some("expired")
                } else if lower.contains("not allowed for this organization") {
                    Some("org disallows OAuth")
                } else if lower.contains("overloaded") {
                    Some("overloaded")
                } else {
                    None
                };
                match cause {
                    Some(cause) => format!("{status} {cause}"),
                    None => format!("HTTP {status}"),
                }
            }
        }
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCredential => f.write_str("no usable credential"),
            Self::Network { message } => write!(f, "network: {message}"),
            Self::Http { status, body } => write!(f, "HTTP {status}: {body}"),
        }
    }
}

/// Everything one probe learned about one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub account: String,
    /// Epoch seconds.
    pub at: i64,
    pub verdict: Verdict,
    /// The limit the server named as binding (`session`, `weekly`, ...).
    #[serde(default)]
    pub claim: Option<String>,
    /// The 5-hour window.
    #[serde(default)]
    pub session: Option<Window>,
    /// The 7-day window.
    #[serde(default)]
    pub weekly: Option<Window>,
    /// Per-model weekly limits, including the family-wide Fable limit.
    #[serde(default)]
    pub buckets: Vec<Bucket>,
    #[serde(default)]
    pub overage: Option<Overage>,
    #[serde(default)]
    pub error: Option<ProbeError>,
    /// Why the profile endpoint added nothing, when it was asked.
    #[serde(default)]
    pub profile_error: Option<String>,
}

impl Reading {
    /// A reading that knows nothing but why.
    pub fn failed(account: &str, at: i64, error: ProbeError) -> Self {
        Self {
            account: account.to_owned(),
            at,
            verdict: Verdict::Unknown,
            claim: None,
            session: None,
            weekly: None,
            buckets: Vec::new(),
            overage: None,
            error: Some(error),
            profile_error: None,
        }
    }

    /// Both shared windows, when the probe reported them.
    pub fn windows(&self) -> Option<(Window, Window)> {
        Some((self.session?, self.weekly?))
    }

    /// Fractions left in the session and weekly windows.
    pub fn remaining(&self) -> Option<(f64, f64)> {
        self.windows()
            .map(|(session, weekly)| (session.remaining(), weekly.remaining()))
    }

    /// Enough to decide on: an error-free answer with both windows. A
    /// `Rejected` verdict with windows is a reading too.
    pub fn has_quota(&self) -> bool {
        self.error.is_none() && self.windows().is_some()
    }

    /// Refused in a way that will not change until the credential does.
    pub fn is_refused(&self) -> bool {
        self.error.as_ref().is_some_and(ProbeError::is_definitive)
    }

    pub fn bucket(&self, key: &ModelKey) -> Option<&Bucket> {
        self.buckets.iter().find(|bucket| &bucket.key == key)
    }

    /// Insert or replace the bucket with this key.
    pub fn set_bucket(&mut self, bucket: Bucket) {
        match self
            .buckets
            .iter_mut()
            .find(|existing| existing.key == bucket.key)
        {
            Some(existing) => *existing = bucket,
            None => self.buckets.push(bucket),
        }
    }

    /// The weekly windows that limit sessions on `model`.
    pub fn windows_limiting<'a>(
        &'a self,
        model: Option<&'a ModelKey>,
    ) -> impl Iterator<Item = &'a Window> + 'a {
        self.buckets
            .iter()
            .filter(move |bucket| model.is_some_and(|model| model.is_limited_by(&bucket.key)))
            .map(|bucket| &bucket.window)
    }

    /// Whether the account can take sessions on `model`: allowed, with the
    /// session window above `session_floor` and the weekly window and every
    /// limiting model bucket above `weekly_floor`.
    pub fn above(&self, session_floor: f64, weekly_floor: f64, model: Option<&ModelKey>) -> bool {
        if self.error.is_some() || !self.verdict.allows() {
            return false;
        }
        let Some((session, weekly)) = self.windows() else {
            return false;
        };
        session.above(session_floor)
            && weekly.above(weekly_floor)
            && self
                .windows_limiting(model)
                .all(|window| window.above(weekly_floor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> ModelKey {
        ModelKey::new(s)
    }

    #[test]
    fn model_keys_agree_between_ids_and_display_names() {
        assert_eq!(key("claude-opus-4-8"), key("Opus 4.8"));
        assert_eq!(key("claude-fable-5-1[1m]").as_str(), "fable51");
        assert_eq!(key("Fable 5").as_str(), "fable5");
        assert_eq!(key("claude").as_str(), "claude");
        assert_eq!(key("Sonnet 4.5").version(), Some("45"));
        assert_eq!(key("fable").version(), None);
        assert_eq!(key("opus").version(), Some("5"));
    }

    #[test]
    fn a_model_is_limited_by_its_own_bucket_and_its_family_wide_one() {
        let fable51 = key("claude-fable-5-1[1m]");
        assert!(fable51.is_limited_by(&key("fable")));
        assert!(fable51.is_limited_by(&key("Fable 5")));
        assert!(fable51.is_limited_by(&key("fable51")));
        assert!(!fable51.is_limited_by(&key("fable52")));
        assert!(!fable51.is_limited_by(&key("Opus 4.8")));
    }

    #[test]
    fn the_opus_alias_obeys_opus_5_not_opus_4_8() {
        let alias = key("opus");
        assert!(alias.is_limited_by(&key("Opus 5")));
        assert!(alias.is_limited_by(&key("opus")));
        assert!(!alias.is_limited_by(&key("Opus 4.8")));
        assert!(key("claude-opus-4-8").is_limited_by(&key("Opus 4.8")));
        assert!(!key("claude-opus-4-8").is_limited_by(&key("Opus 5")));
    }

    #[test]
    fn keys_outside_any_family_match_by_equality() {
        assert!(key("haiku45").is_limited_by(&key("Haiku 4.5")));
        assert!(!key("haiku45").is_limited_by(&key("opus")));
        assert!(!key("").is_limited_by(&key("")));
    }

    fn reading(session_used: f64, weekly_used: f64) -> Reading {
        Reading {
            account: "a".into(),
            at: 0,
            verdict: Verdict::Allowed,
            claim: None,
            session: Some(Window::new(session_used, 100)),
            weekly: Some(Window::new(weekly_used, 200)),
            buckets: Vec::new(),
            overage: None,
            error: None,
            profile_error: None,
        }
    }

    #[test]
    fn the_exact_floor_is_not_above_it() {
        assert!(reading(0.94, 0.0).above(0.05, 0.05, None));
        assert!(!reading(0.95, 0.0).above(0.05, 0.05, None));
        assert!(!reading(0.0, 0.95).above(0.05, 0.05, None));
    }

    #[test]
    fn a_limiting_bucket_is_a_weekly_floor_and_an_unrelated_one_is_not() {
        let mut r = reading(0.1, 0.1);
        r.buckets.push(Bucket {
            key: key("Opus 4.8"),
            label: "Opus 4.8".into(),
            window: Window::new(0.99, 0),
            source: Source::Profile,
        });
        assert!(!r.above(0.05, 0.05, Some(&key("claude-opus-4-8"))));
        assert!(r.above(0.05, 0.05, Some(&key("claude-opus-5"))));
        assert!(r.above(0.05, 0.05, Some(&key("claude-sonnet-4-5"))));
        assert!(r.above(0.05, 0.05, None));
    }

    #[test]
    fn errors_and_rejections_are_never_above_a_floor() {
        let mut rejected = reading(0.0, 0.0);
        rejected.verdict = Verdict::Rejected;
        assert!(!rejected.above(0.0, 0.0, None));
        assert!(
            rejected.has_quota(),
            "a rejection with windows is still a reading"
        );
        let failed = Reading::failed(
            "a",
            0,
            ProbeError::Http {
                status: 500,
                body: String::new(),
            },
        );
        assert!(!failed.above(0.0, 0.0, None));
        assert!(!failed.has_quota());
        assert!(!failed.is_refused());
        assert!(Reading::failed("a", 0, ProbeError::NoCredential).is_refused());
    }

    #[test]
    fn probe_errors_group_by_cause() {
        let http = |status, body: &str| ProbeError::Http {
            status,
            body: body.into(),
        };
        assert_eq!(
            http(401, "OAuth access token has been revoked.").reason(),
            "401 revoked"
        );
        assert_eq!(http(529, "overloaded_error").reason(), "529 overloaded");
        assert_eq!(http(500, "boom").reason(), "HTTP 500");
        assert_eq!(
            ProbeError::Network {
                message: "timed out".into()
            }
            .reason(),
            "network"
        );
    }

    #[test]
    fn bucket_serde_keeps_the_stored_spelling() {
        let bucket = Bucket {
            key: key("Opus 4.8"),
            label: "Opus 4.8".into(),
            window: Window::new(0.5, 7),
            source: Source::Observed,
        };
        let json = serde_json::to_string(&bucket).unwrap();
        assert_eq!(
            json,
            r#"{"key":"opus48","label":"Opus 4.8","window":{"utilization":0.5,"reset":7},"source":"observed-rejection"}"#
        );
        assert_eq!(serde_json::from_str::<Bucket>(&json).unwrap(), bucket);
    }
}

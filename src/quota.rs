//! What is known about an account's room, for every service: a [`Reading`]
//! is one read of one account, and its [`Limit`]s are the windows that read
//! reported, each named by its length ([`Span`]) and what it covers
//! ([`Scope`]). How a model name maps onto a model's limit is here too.

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
    Mythos,
}

impl Family {
    pub const ALL: [Family; 4] = [Family::Opus, Family::Sonnet, Family::Fable, Family::Mythos];

    pub fn name(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Sonnet => "sonnet",
            Self::Fable => "fable",
            Self::Mythos => "mythos",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::Sonnet => "Sonnet",
            Self::Fable => "Fable",
            Self::Mythos => "Mythos",
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
/// names its family-wide limit.
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
    /// as the whole family: an exhausted Opus 4.8 limit must not poison it.
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

    /// Whether a limit scoped to `bucket` limits sessions on this model.
    ///
    /// Same family and: the limit is family-wide; or it names a major only
    /// (`fable5`) and this model is one of its point releases; or both name
    /// the same version. Keys outside any family match by equality.
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

/// A subscription service tokeman reads quota from.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    #[value(skip)]
    Claude,
    #[value(skip)]
    Codex,
    Antigravity,
    Zai,
    Kimi,
}

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Zai => "zai",
            Self::Kimi => "kimi",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
            Self::Zai => "GLM (z.ai)",
            Self::Kimi => "Kimi Code",
        }
    }
}

/// How long a window lasts, which is what names it. A server's position or
/// field name for a window says nothing: Codex reports a weekly-only
/// account's one window as `primary_window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Span {
    FiveHour,
    Weekly,
    /// 28 to 31 days.
    Monthly,
    /// Any other length, in seconds.
    Other(i64),
    /// The server did not say.
    Unknown,
}

impl Span {
    pub const FIVE_HOUR_SECS: i64 = 5 * 3_600;
    pub const DAY_SECS: i64 = 86_400;
    pub const WEEK_SECS: i64 = 7 * Self::DAY_SECS;

    pub fn from_seconds(seconds: Option<i64>) -> Self {
        match seconds {
            Some(Self::FIVE_HOUR_SECS) => Self::FiveHour,
            Some(Self::WEEK_SECS) => Self::Weekly,
            Some(seconds) if (28 * Self::DAY_SECS..=31 * Self::DAY_SECS).contains(&seconds) => {
                Self::Monthly
            }
            Some(seconds) if seconds > 0 => Self::Other(seconds),
            _ => Self::Unknown,
        }
    }

    /// The length in seconds; a month counts 30 days.
    pub fn seconds(&self) -> Option<i64> {
        match *self {
            Self::FiveHour => Some(Self::FIVE_HOUR_SECS),
            Self::Weekly => Some(Self::WEEK_SECS),
            Self::Monthly => Some(30 * Self::DAY_SECS),
            Self::Other(seconds) => Some(seconds),
            Self::Unknown => None,
        }
    }

    /// `5h`, `weekly`, `monthly`, `daily`, or the span in its largest whole
    /// unit (`2d`, `90m`).
    pub fn label(&self) -> String {
        match *self {
            Self::FiveHour => "5h".to_owned(),
            Self::Weekly => "weekly".to_owned(),
            Self::Monthly => "monthly".to_owned(),
            Self::Other(Self::DAY_SECS) => "daily".to_owned(),
            Self::Other(seconds) => {
                let unit = [(Self::DAY_SECS, "d"), (3_600, "h"), (60, "m")]
                    .into_iter()
                    .find(|(size, _)| seconds % size == 0);
                match unit {
                    Some((size, suffix)) => format!("{}{suffix}", seconds / size),
                    None => format!("{seconds}s"),
                }
            }
            Self::Unknown => "window".to_owned(),
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// What a limit covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The whole account.
    All,
    /// One model, or a family of them: Claude's per-model weekly limits and
    /// Codex's limits that stand in for a model's ordinary one.
    Model(ModelKey),
    /// A named part of the account's use, as the server groups it:
    /// Antigravity's model groups, a Codex metered feature, z.ai's MCP tools.
    Group(String),
}

/// Where a limit's reading came from.
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
    /// A service's usage endpoint, asked directly.
    Poll,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Headers => "rate-limit headers",
            Self::Observed => "observed Claude rejection",
            Self::Carried => "carried sample",
            Self::Poll => "usage endpoint",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Headers => "headers",
            Self::Observed => "observed!",
            Self::Carried => "carried",
            Self::Poll => "usage",
        }
    }
}

/// An absolute amount behind a window's fraction, when the service reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Amount {
    pub used: f64,
    pub limit: f64,
    /// "credits", "requests", "tokens", ...
    pub unit: String,
}

/// A money balance beside the limits (Kimi's booster wallet).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub label: String,
    /// Minor units (cents).
    pub left_minor: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_minor: Option<i64>,
    pub currency: String,
}

/// One limit of one account: a window of some length over some part of the
/// account's use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    pub span: Span,
    pub scope: Scope,
    /// The server's name for the limit, when it has one ("Fable", "5h",
    /// "codex_spark").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Fraction used (may exceed 1.0) and the reset (0 when none is pending).
    pub window: Window,
    pub source: Source,
    /// The limit's size as a fraction of the weekly limit, when the server
    /// says (`weekly_scoped_shares`): Fable's is 0.5. Usage under it also
    /// counts toward the weekly limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<f64>,
    /// What the server said about this window apart from its use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<WindowStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
}

impl Limit {
    pub fn new(span: Span, scope: Scope, window: Window, source: Source) -> Self {
        Self {
            span,
            scope,
            label: String::new(),
            window,
            source,
            share: None,
            status: None,
            amount: None,
        }
    }

    /// A model's weekly limit.
    pub fn model(key: ModelKey, label: &str, window: Window, source: Source) -> Self {
        Self {
            label: label.to_owned(),
            ..Self::new(Span::Weekly, Scope::Model(key), window, source)
        }
    }

    /// Whether this is an account-wide limit.
    pub fn is_shared(&self) -> bool {
        self.scope == Scope::All
    }

    /// The model this limit is scoped to.
    pub fn key(&self) -> Option<&ModelKey> {
        match &self.scope {
            Scope::Model(key) => Some(key),
            _ => None,
        }
    }

    /// The group this limit is scoped to.
    pub fn group(&self) -> Option<&str> {
        match &self.scope {
            Scope::Group(group) => Some(group),
            _ => None,
        }
    }

    /// The label, or the span's name when the server gave none.
    pub fn name(&self) -> String {
        if self.label.is_empty() {
            self.span.label()
        } else {
            self.label.clone()
        }
    }

    /// Whether the server's own status for this window is a rejection.
    pub fn is_rejected(&self) -> bool {
        self.status
            .is_some_and(|status| status.verdict == Verdict::Rejected)
    }

    /// Loose match of a scoped limit on its group, model or label, any case,
    /// so a caller can say "spark" or "gemini". Never an account-wide limit.
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        let scope = match &self.scope {
            Scope::All => return false,
            Scope::Model(key) => key.as_str(),
            Scope::Group(group) => group,
        };
        scope.to_lowercase().contains(&needle) || self.label.to_lowercase().contains(&needle)
    }

    /// Room left under this limit as a fraction of the weekly limit: its own
    /// room scaled by its share, never more than the weekly limit has left.
    /// Without a share, its own room.
    pub fn room(&self, weekly: Option<&Window>) -> f64 {
        let own = self.window.remaining();
        match (self.share, weekly) {
            (Some(share), Some(weekly)) => (own * share).min(weekly.remaining()),
            (Some(share), None) => own * share,
            (None, _) => own,
        }
    }
}

/// What the probe's `unified-status` header said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allowed,
    AllowedWarning,
    Rejected,
    #[default]
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

/// What a window's own status said. For Claude these are the `-status` and
/// `-surpassed-threshold` headers: the overall `unified-status` describes
/// the requested model only, these describe each window whatever the model.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowStatus {
    pub verdict: Verdict,
    /// The warning threshold the window has crossed (0.25 .. 1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surpassed_threshold: Option<f64>,
}

/// The experiment arm of the low-priority lane offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlowOffer {
    Treatment,
    Control,
    #[serde(other)]
    Unrecognized,
}

/// The low-priority lane's state (`unified-slow-status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlowStatus {
    Active,
    NotNeeded,
    SlotBusy,
    WeeklyLimit,
    BudgetExhausted,
    Ineligible,
    Off,
    #[serde(other)]
    Unrecognized,
}

/// Parse a header value into a snake_case enum with an `Unrecognized` arm.
pub fn parse_wire<T: serde::de::DeserializeOwned>(value: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).ok()
}

/// The low-priority lane past the 5h wall: requests sent with
/// `anthropic-usage-limit: slow` are served when capacity allows, charged to
/// the weekly limit. Never room: an account on it is walled.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SlowLane {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer: Option<SlowOffer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SlowStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wait_secs: Option<f64>,
    /// The lane's own budget, as a fraction used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Window>,
}

/// A wrap-up allowance past a limit: 2xx responses keep arriving while a
/// window reads at or over 1.0. Never room.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Grace {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_used: Option<f64>,
}

/// Everything else the rate-limit headers said, for display and history.
/// None of it takes part in viability; a window's own status is on its
/// [`Limit`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Signals {
    /// Every `unified-claim`: the windows the server counts as exceeded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<String>,
    /// `unified-fallback: available`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_percentage: Option<f64>,
    /// What the account could buy (`overage`, `upgrade_plan`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upgrade_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow: Option<SlowLane>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace: Option<Grace>,
}

impl Signals {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Whether the account has a 5-hour limit at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionLimit {
    /// Nothing says either way: headers that lack the 5h window prove nothing.
    #[default]
    Unknown,
    /// The headers or the profile's `limits[]` reported a session window.
    Present,
    /// The profile's `limits[]` has no `session` row: a weekly-only account
    /// (verified Ultra).
    Absent,
}

impl SessionLimit {
    pub fn is_unknown(&self) -> bool {
        *self == Self::Unknown
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

/// Why a read produced no quota.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadError {
    /// The account offers nothing to read with.
    NoCredential,
    /// The credential could not be read: a missing or unreadable auth or key
    /// file, or one without a token.
    Credential { message: String },
    /// The request never completed.
    Network { message: String },
    /// Every attempt was answered by the edge, not the API.
    Edge { status: u16, attempts: u32 },
    /// The API answered without quota.
    Http { status: u16, body: String },
    /// The answer was not in a shape this build reads.
    Decode { message: String },
    /// The service refused in its own words.
    Other { message: String },
}

impl ReadError {
    /// 401/403 or no credential: the account will stay refused until its
    /// credential changes, unlike a timeout or a 5xx.
    pub fn is_definitive(&self) -> bool {
        match self {
            Self::NoCredential | Self::Credential { .. } => true,
            Self::Http { status, .. } => matches!(status, 401 | 403),
            _ => false,
        }
    }

    /// The API rejected the token: a refresh may help.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Http { status: 401, .. })
    }

    /// A few words that group accounts by cause in a summary line.
    pub fn reason(&self) -> String {
        match self {
            Self::NoCredential | Self::Credential { .. } => "no usable credential".into(),
            Self::Network { .. } => "network".into(),
            Self::Edge { .. } => "edge challenge".into(),
            Self::Decode { .. } => "unreadable answer".into(),
            Self::Other { .. } => "refused".into(),
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

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCredential => f.write_str("no usable credential"),
            Self::Network { message } => write!(f, "network: {message}"),
            Self::Edge { status, attempts } => write!(
                f,
                "HTTP {status}: Cloudflare challenge after {attempts} attempts, not an auth failure"
            ),
            Self::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            Self::Credential { message } | Self::Decode { message } | Self::Other { message } => {
                f.write_str(message)
            }
        }
    }
}

/// What only Claude's rate-limit headers and profile endpoint say.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ClaudeDetail {
    /// For the requested model only.
    #[serde(default)]
    pub verdict: Verdict,
    /// The limit the server named as binding (`five_hour`, `seven_day`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
    #[serde(default, skip_serializing_if = "SessionLimit::is_unknown")]
    pub session_limit: SessionLimit,
    #[serde(default, skip_serializing_if = "Signals::is_empty")]
    pub signals: Signals,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage: Option<Overage>,
    /// Why the profile endpoint added nothing, when it was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_error: Option<String>,
}

/// Codex's credit state, as the usage body sends it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Credits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub overage_limit_reached: Option<bool>,
    /// The backend sends `null`, `"0"` or a number here; kept as text.
    pub balance: Option<String>,
}

/// Whether a Codex account may use one model right now (`model_usage`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub slug: String,
    pub available: Option<bool>,
    /// Epoch seconds when the model becomes available again.
    pub available_at: Option<i64>,
    /// Buying credits would make the model available.
    pub credits_would_enable: Option<bool>,
}

impl ModelUsage {
    pub fn is_unavailable(&self) -> bool {
        self.available == Some(false)
    }
}

/// A Codex workspace's cap on credit spending (`spend_control`). The amounts
/// are decimal strings, kept as sent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpendControl {
    pub reached: Option<bool>,
    pub limit: Option<SpendLimit>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpendLimit {
    pub limit: Option<String>,
    pub used: Option<String>,
    pub remaining: Option<String>,
    pub used_percent: Option<f64>,
    /// Epoch seconds.
    pub reset_at: Option<i64>,
    pub source: Option<String>,
}

/// Reset credits a Codex account holds, as the usage body counts them.
/// `applicable` is how many would clear a limit the account is at now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResetCreditCount {
    pub available: i64,
    pub applicable: Option<i64>,
}

/// What only Codex's usage endpoint says.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CodexDetail {
    /// From the usage body, else from the stored id token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Whether the backend accepts work on the account-wide limit. `None`
    /// when it sent no limit block, which enforces nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<bool>,
    /// The backend's name for the limit it is enforcing, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
    /// Per-model availability, by slug.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_control: Option<SpendControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<ResetCreditCount>,
}

/// What one service says beyond limits and balances.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Detail {
    #[default]
    None,
    Claude(ClaudeDetail),
    Codex(CodexDetail),
}

impl Detail {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Everything one read learned about one account of one service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub service: Service,
    pub account: String,
    /// Epoch seconds.
    pub at: i64,
    /// The plan as the service names it ("max", "plus", "Vivace").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Every limit the read reported. A server omits a window with no use,
    /// and an omitted window limits nothing.
    #[serde(default)]
    pub limits: Vec<Limit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub balances: Vec<Balance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReadError>,
    #[serde(default, skip_serializing_if = "Detail::is_none")]
    pub detail: Detail,
}

impl Reading {
    /// A reading of `account` that knows nothing yet.
    pub fn empty(service: Service, account: &str, at: i64) -> Self {
        Self {
            service,
            account: account.to_owned(),
            at,
            plan: None,
            limits: Vec::new(),
            balances: Vec::new(),
            error: None,
            detail: match service {
                Service::Claude => Detail::Claude(ClaudeDetail::default()),
                Service::Codex => Detail::Codex(CodexDetail::default()),
                Service::Antigravity | Service::Zai | Service::Kimi => Detail::None,
            },
        }
    }

    /// A reading that knows nothing but why.
    pub fn failed(service: Service, account: &str, at: i64, error: ReadError) -> Self {
        Self {
            error: Some(error),
            ..Self::empty(service, account, at)
        }
    }

    pub fn claude(&self) -> Option<&ClaudeDetail> {
        match &self.detail {
            Detail::Claude(detail) => Some(detail),
            _ => None,
        }
    }

    /// Claude's detail, to fill in; a reading without one gains an empty one.
    pub fn claude_mut(&mut self) -> &mut ClaudeDetail {
        if !matches!(self.detail, Detail::Claude(_)) {
            self.detail = Detail::Claude(ClaudeDetail::default());
        }
        match &mut self.detail {
            Detail::Claude(detail) => detail,
            _ => unreachable!("the detail was just set"),
        }
    }

    pub fn codex(&self) -> Option<&CodexDetail> {
        match &self.detail {
            Detail::Codex(detail) => Some(detail),
            _ => None,
        }
    }

    /// Claude's verdict for the requested model; `Unknown` elsewhere.
    pub fn verdict(&self) -> Verdict {
        self.claude()
            .map_or(Verdict::Unknown, |claude| claude.verdict)
    }

    /// Whether the account has a 5-hour limit at all, as far as Claude says.
    pub fn session_limit(&self) -> SessionLimit {
        self.claude()
            .map_or(SessionLimit::Unknown, |claude| claude.session_limit)
    }

    /// The limit of this length and scope.
    pub fn limit(&self, span: Span, scope: &Scope) -> Option<&Limit> {
        self.limits
            .iter()
            .find(|limit| limit.span == span && &limit.scope == scope)
    }

    /// The window of this length and scope.
    pub fn window(&self, span: Span, scope: &Scope) -> Option<Window> {
        self.limit(span, scope).map(|limit| limit.window)
    }

    /// The account-wide 5-hour window; `None` when the read did not report it.
    pub fn session(&self) -> Option<Window> {
        self.window(Span::FiveHour, &Scope::All)
    }

    /// The account-wide weekly window, likewise.
    pub fn weekly(&self) -> Option<Window> {
        self.window(Span::Weekly, &Scope::All)
    }

    /// Fraction left in the session window, when the read reported one.
    pub fn session_remaining(&self) -> Option<f64> {
        self.session().map(|window| window.remaining())
    }

    pub fn weekly_remaining(&self) -> Option<f64> {
        self.weekly().map(|window| window.remaining())
    }

    /// The account-wide limits.
    pub fn shared(&self) -> impl Iterator<Item = &Limit> {
        self.limits.iter().filter(|limit| limit.is_shared())
    }

    /// The limits scoped to a model or a group, gathered by scope in the
    /// order the read reported them.
    pub fn scoped(&self) -> Vec<(&Scope, Vec<&Limit>)> {
        let mut groups: Vec<(&Scope, Vec<&Limit>)> = Vec::new();
        for limit in self.limits.iter().filter(|limit| !limit.is_shared()) {
            match groups.iter_mut().find(|(scope, _)| *scope == &limit.scope) {
                Some((_, limits)) => limits.push(limit),
                None => groups.push((&limit.scope, vec![limit])),
            }
        }
        groups
    }

    /// The limits scoped to a model.
    pub fn models(&self) -> impl Iterator<Item = &Limit> {
        self.limits.iter().filter(|limit| limit.key().is_some())
    }

    /// The limit scoped to exactly this model key.
    pub fn model_limit(&self, key: &ModelKey) -> Option<&Limit> {
        self.models().find(|limit| limit.key() == Some(key))
    }

    /// Insert the limit, or replace the one of the same length and scope.
    pub fn set_limit(&mut self, limit: Limit) {
        match self
            .limits
            .iter_mut()
            .find(|existing| existing.span == limit.span && existing.scope == limit.scope)
        {
            Some(existing) => *existing = limit,
            None => self.limits.push(limit),
        }
    }

    /// Set the window of the limit of this length and scope, keeping what
    /// else is known about it; a new limit carries nothing else.
    pub fn set_window(&mut self, span: Span, scope: Scope, window: Window, source: Source) {
        match self
            .limits
            .iter_mut()
            .find(|existing| existing.span == span && existing.scope == scope)
        {
            Some(existing) => {
                existing.window = window;
                existing.source = source;
            }
            None => self.limits.push(Limit::new(span, scope, window, source)),
        }
    }

    /// Forget the limit of this length and scope.
    pub fn remove_limit(&mut self, span: Span, scope: &Scope) {
        self.limits
            .retain(|limit| !(limit.span == span && &limit.scope == scope));
    }

    /// Enough to decide on: an error-free answer with at least one
    /// account-wide window. A server omits a window that has no usage (since
    /// 2026-10 a healthy Claude account often reports only its weekly
    /// window), and an omitted window limits nothing. A rejection with a
    /// window is a reading.
    pub fn has_quota(&self) -> bool {
        self.error.is_none() && self.shared().next().is_some()
    }

    /// Refused in a way that will not change until the credential does.
    pub fn is_refused(&self) -> bool {
        self.error.as_ref().is_some_and(ReadError::is_definitive)
    }

    /// Whether the service says it accepts work on the account: Claude's
    /// verdict for the requested model, Codex's `allowed`. A service that
    /// says neither refuses nothing.
    pub fn allows(&self) -> bool {
        match &self.detail {
            Detail::Claude(claude) => claude.verdict.allows(),
            Detail::Codex(codex) => codex.allowed != Some(false),
            Detail::None => true,
        }
    }

    /// The model-scoped limits that limit sessions on `model`.
    pub fn limits_for<'a>(
        &'a self,
        model: Option<&'a ModelKey>,
    ) -> impl Iterator<Item = &'a Limit> + 'a {
        self.limits.iter().filter(move |limit| {
            limit
                .key()
                .zip(model)
                .is_some_and(|(key, model)| model.is_limited_by(key))
        })
    }

    /// Whether a model's limit leaves more than `floor`. One that is a share
    /// of the weekly is judged on its room in weekly terms.
    pub fn limit_above(&self, limit: &Limit, floor: f64) -> bool {
        match limit.share {
            None => limit.window.above(floor),
            Some(_) => limit.room(self.weekly().as_ref()) > floor,
        }
    }

    /// Room left for a session on `model`, as a fraction of the weekly
    /// limit: the weekly window's, or less when a limit on the model has
    /// less (a shared one counts at its share of the weekly). `None` when
    /// nothing weekly is reported.
    pub fn model_room(&self, model: Option<&ModelKey>) -> Option<f64> {
        let weekly = self.weekly();
        self.limits_for(model)
            .map(|limit| limit.room(weekly.as_ref()))
            .chain(weekly.map(|window| window.remaining()))
            .reduce(f64::min)
    }

    /// Whether the account can take sessions on `model`: accepted by the
    /// service, no account-wide window rejected by its own status, every
    /// account-wide window above its floor (the session floor for a 5-hour
    /// window, the weekly floor for any other) and every limit on the model
    /// above the weekly floor.
    pub fn above(&self, session_floor: f64, weekly_floor: f64, model: Option<&ModelKey>) -> bool {
        let floor = |span: Span| match span {
            Span::FiveHour => session_floor,
            _ => weekly_floor,
        };
        self.has_quota()
            && self.allows()
            && self
                .shared()
                .all(|limit| !limit.is_rejected() && limit.window.above(floor(limit.span)))
            && self
                .limits_for(model)
                .all(|limit| self.limit_above(limit, weekly_floor))
    }

    /// The limit with the least left: among the account-wide ones when there
    /// are any, else among all of them.
    pub fn tightest(&self) -> Option<&Limit> {
        let most_used = |a: &&Limit, b: &&Limit| a.window.used.total_cmp(&b.window.used);
        self.shared()
            .max_by(most_used)
            .or_else(|| self.limits.iter().max_by(most_used))
    }

    /// The reading as work under its scoped limits sees it, for ranking
    /// accounts on a part of their quota. With `needle`, the scoped limits
    /// matching it ([`Limit::matches`]) become the account-wide ones and
    /// every other limit is left out: such a limit is metered apart. With
    /// `None`, every group's limit joins the account-wide ones, since work
    /// may land in any group. `None` when no limit would be left.
    pub fn scoped_to(&self, needle: Option<&str>) -> Option<Reading> {
        let limits: Vec<Limit> = self
            .limits
            .iter()
            .filter(|limit| match needle {
                Some(needle) => limit.matches(needle),
                None => limit.key().is_none(),
            })
            .map(|limit| Limit {
                scope: Scope::All,
                ..limit.clone()
            })
            .collect();
        (!limits.is_empty()).then(|| Reading {
            limits,
            ..self.clone()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::model_limit;

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
        crate::testing::reading("a", session_used, weekly_used)
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
        r.limits
            .push(model_limit("Opus 4.8", "Opus 4.8", 0.99, Source::Profile));
        assert!(!r.above(0.05, 0.05, Some(&key("claude-opus-4-8"))));
        assert!(r.above(0.05, 0.05, Some(&key("claude-opus-5"))));
        assert!(r.above(0.05, 0.05, Some(&key("claude-sonnet-4-5"))));
        assert!(r.above(0.05, 0.05, None));
    }

    #[test]
    fn a_window_the_api_omits_limits_nothing() {
        let mut weekly_only = reading(0.0, 0.74);
        weekly_only.set_session(None);
        assert!(weekly_only.has_quota());
        assert_eq!(weekly_only.session_remaining(), None);
        assert!(weekly_only.above(0.99, 0.05, None));
        assert!(!weekly_only.above(0.0, 0.30, None));
        let mut nothing = reading(0.0, 0.0);
        nothing.set_session(None);
        nothing.set_weekly(None);
        assert!(!nothing.has_quota());
        assert!(!nothing.above(0.0, 0.0, None));
        // A model's limit alone is not a reading of the account.
        nothing
            .limits
            .push(model_limit("fable", "Fable", 0.1, Source::Observed));
        assert!(!nothing.has_quota());
    }

    #[test]
    fn errors_and_rejections_are_never_above_a_floor() {
        let mut rejected = reading(0.0, 0.0);
        rejected.claude_mut().verdict = Verdict::Rejected;
        assert!(!rejected.above(0.0, 0.0, None));
        assert!(
            rejected.has_quota(),
            "a rejection with windows is still a reading"
        );
        let failed = Reading::failed(
            Service::Claude,
            "a",
            0,
            ReadError::Http {
                status: 500,
                body: String::new(),
            },
        );
        assert!(!failed.above(0.0, 0.0, None));
        assert!(!failed.has_quota());
        assert!(!failed.is_refused());
        assert!(Reading::failed(Service::Claude, "a", 0, ReadError::NoCredential).is_refused());
    }

    #[test]
    fn a_shared_window_rejected_by_its_own_status_is_not_above_any_floor() {
        let mut r = reading(0.1, 0.5);
        assert!(r.above(0.05, 0.05, None));
        r.set_status(
            Span::Weekly,
            &Scope::All,
            WindowStatus {
                verdict: Verdict::AllowedWarning,
                surpassed_threshold: Some(0.5),
            },
        );
        assert!(r.above(0.05, 0.05, None));
        r.set_status(
            Span::Weekly,
            &Scope::All,
            WindowStatus {
                verdict: Verdict::Rejected,
                surpassed_threshold: Some(1.0),
            },
        );
        assert!(!r.above(0.0, 0.0, None));
        // A model's own window rejected says nothing about other models.
        let mut fable_walled = reading(0.1, 0.5);
        let fable = Scope::Model(ModelKey::family_wide(Family::Fable));
        fable_walled
            .limits
            .push(model_limit("fable", "Fable", 0.5, Source::Headers));
        fable_walled.set_status(
            Span::Weekly,
            &fable,
            WindowStatus {
                verdict: Verdict::Rejected,
                surpassed_threshold: None,
            },
        );
        assert!(fable_walled.above(0.05, 0.05, Some(&key("claude-opus-5-5"))));
    }

    #[test]
    fn fable_room_is_its_share_of_the_weekly_and_never_more_than_the_weekly_left() {
        let fable = key("claude-fable-5-2");
        let mut r = reading(0.0, 0.40);
        let mut limit = model_limit("fable", "Fable", 0.37, Source::Profile);
        limit.share = Some(0.5);
        r.limits.push(limit);
        // 63% of a half-weekly allowance is 31.5% of the weekly; 60% is left.
        assert!((r.model_room(Some(&fable)).unwrap() - 0.315).abs() < 1e-9);
        // When the weekly has less left than that, the weekly binds.
        r.set_weekly(Some(Window::new(0.80, 0)));
        assert!((r.model_room(Some(&fable)).unwrap() - 0.20).abs() < 1e-9);
        // Other models see only the weekly; no weekly and no bucket is unknown.
        assert!((r.model_room(Some(&key("claude-opus-5-5"))).unwrap() - 0.20).abs() < 1e-9);
        r.set_weekly(None);
        assert!((r.model_room(Some(&fable)).unwrap() - 0.315).abs() < 1e-9);
        assert_eq!(r.model_room(None), None);
        // A bucket without a share counts its own room.
        let plain = crate::testing::reading_with_model("p", 0.0, 0.1, "opus48", "Opus 4.8", 0.7);
        assert!((plain.model_room(Some(&key("claude-opus-4-8"))).unwrap() - 0.3).abs() < 1e-9);
    }

    #[test]
    fn a_shared_bucket_is_viable_on_its_room_in_weekly_terms() {
        let fable = key("claude-fable-5-2");
        let mut r = reading(0.0, 0.2);
        let mut limit = model_limit("fable", "Fable", 0.92, Source::Profile);
        r.limits.push(limit.clone());
        // Unshared: 8% of its own limit left clears a 5% floor.
        assert!(r.above(0.05, 0.05, Some(&fable)));
        // Half the weekly: 4% of the weekly left does not.
        limit.share = Some(0.5);
        r.set_limit(limit);
        assert_eq!(r.models().count(), 1);
        assert!(!r.above(0.05, 0.05, Some(&fable)));
        assert!(r.above(0.05, 0.03, Some(&fable)));
        assert!(r.above(0.05, 0.05, Some(&key("claude-opus-5-5"))));
    }

    #[test]
    fn mythos_is_a_family() {
        assert_eq!(key("claude-mythos-5").family(), Some(Family::Mythos));
        assert!(key("claude-mythos-5").is_limited_by(&ModelKey::family_wide(Family::Mythos)));
        assert!(!key("claude-mythos-5").is_limited_by(&key("fable")));
    }

    #[test]
    fn wire_enums_parse_known_values_and_keep_the_unknown_ones() {
        assert_eq!(
            parse_wire::<SlowStatus>("not_needed"),
            Some(SlowStatus::NotNeeded)
        );
        assert_eq!(
            parse_wire::<SlowStatus>("budget_exhausted"),
            Some(SlowStatus::BudgetExhausted)
        );
        assert_eq!(
            parse_wire::<SlowStatus>("brand_new"),
            Some(SlowStatus::Unrecognized)
        );
        assert_eq!(
            parse_wire::<SlowOffer>("treatment"),
            Some(SlowOffer::Treatment)
        );
    }

    #[test]
    fn readings_without_the_optional_fields_still_parse_and_do_not_grow_on_write() {
        let bare = r#"{"service":"claude","account":"a","at":1,"detail":{"claude":{"verdict":"allowed"}}}"#;
        let parsed: Reading = serde_json::from_str(bare).unwrap();
        assert_eq!(parsed.session_limit(), SessionLimit::Unknown);
        assert_eq!(parsed.verdict(), Verdict::Allowed);
        assert!(parsed.claude().unwrap().signals.is_empty());
        assert!(parsed.limits.is_empty());
        let written = serde_json::to_string(&parsed).unwrap();
        assert!(!written.contains("signals") && !written.contains("session_limit"));
        assert!(!written.contains("plan") && !written.contains("balances"));
        let none: Reading =
            serde_json::from_str(r#"{"service":"kimi","account":"k","at":2}"#).unwrap();
        assert_eq!(none.detail, Detail::None);
        assert!(!serde_json::to_string(&none).unwrap().contains("detail"));
    }

    #[test]
    fn read_errors_group_by_cause() {
        let http = |status, body: &str| ReadError::Http {
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
            ReadError::Network {
                message: "timed out".into()
            }
            .reason(),
            "network"
        );
        assert!(http(401, "").is_unauthorized() && http(401, "").is_definitive());
        assert!(http(403, "").is_definitive() && !http(403, "").is_unauthorized());
        let edge = ReadError::Edge {
            status: 403,
            attempts: 4,
        };
        assert!(!edge.is_definitive() && !edge.is_unauthorized());
        assert_eq!(
            serde_json::to_string(&edge).unwrap(),
            r#"{"kind":"edge","status":403,"attempts":4}"#
        );
    }

    #[test]
    fn limit_serde_keeps_the_stored_spelling() {
        let limit = Limit::model(
            key("Opus 4.8"),
            "Opus 4.8",
            Window::new(0.5, 7),
            Source::Observed,
        );
        let json = serde_json::to_string(&limit).unwrap();
        assert_eq!(
            json,
            r#"{"span":"weekly","scope":{"model":"opus48"},"label":"Opus 4.8","window":{"utilization":0.5,"reset":7},"source":"observed-rejection"}"#
        );
        assert_eq!(serde_json::from_str::<Limit>(&json).unwrap(), limit);
        let shared = Limit::new(
            Span::Other(86_400),
            Scope::All,
            Window::new(0.1, 0),
            Source::Poll,
        );
        assert_eq!(
            serde_json::to_string(&shared).unwrap(),
            r#"{"span":{"other":86400},"scope":"all","window":{"utilization":0.1,"reset":0},"source":"poll"}"#
        );
        let group = Scope::Group("Gemini Models".into());
        assert_eq!(
            serde_json::to_string(&group).unwrap(),
            r#"{"group":"Gemini Models"}"#
        );
    }

    #[test]
    fn spans_are_named_by_length() {
        assert_eq!(Span::from_seconds(Some(18_000)), Span::FiveHour);
        assert_eq!(Span::from_seconds(Some(604_800)), Span::Weekly);
        assert_eq!(Span::from_seconds(Some(30 * 86_400)), Span::Monthly);
        assert_eq!(Span::from_seconds(Some(28 * 86_400)), Span::Monthly);
        assert_eq!(Span::from_seconds(None), Span::Unknown);
        assert_eq!(Span::from_seconds(Some(0)), Span::Unknown);
        assert_eq!(Span::FiveHour.label(), "5h");
        assert_eq!(Span::Weekly.label(), "weekly");
        assert_eq!(Span::Monthly.label(), "monthly");
        assert_eq!(Span::Other(86_400).label(), "daily");
        assert_eq!(Span::Other(2 * 86_400).label(), "2d");
        assert_eq!(Span::Other(5_400).label(), "90m");
        assert_eq!(Span::Other(3_600).label(), "1h");
        assert_eq!(Span::Other(61).label(), "61s");
        assert_eq!(Span::Unknown.to_string(), "window");
        assert_eq!(Span::Monthly.seconds(), Some(30 * 86_400));
        assert_eq!(Span::Unknown.seconds(), None);
    }

    /// A Claude reading as the probe and the profile endpoint build it.
    fn claude_shaped() -> Reading {
        let mut r = reading(0.30, 0.40);
        let mut fable = model_limit("fable", "Fable", 0.50, Source::Profile);
        fable.share = Some(0.5);
        r.limits.push(fable);
        r.limits
            .push(model_limit("Opus 4.8", "Opus 4.8", 0.97, Source::Profile));
        r
    }

    /// A Codex reading: weekly first, a 5h window, a daily one, and a
    /// feature limit that is exhausted.
    fn codex_shaped(allowed: bool) -> Reading {
        let mut r = Reading::empty(Service::Codex, "work", 100);
        let shared = |span, used, reset| {
            Limit::new(span, Scope::All, Window::new(used, reset), Source::Poll)
        };
        r.limits = vec![
            shared(Span::Weekly, 0.60, 9_000),
            shared(Span::FiveHour, 0.20, 500),
            shared(Span::Other(86_400), 0.10, 700),
            Limit {
                label: "codex_spark".into(),
                status: Some(WindowStatus {
                    verdict: Verdict::Rejected,
                    surpassed_threshold: None,
                }),
                ..Limit::new(
                    Span::Unknown,
                    Scope::Group("gpt-5.3-codex-spark".into()),
                    Window::new(1.0, 0),
                    Source::Poll,
                )
            },
        ];
        r.detail = Detail::Codex(CodexDetail {
            allowed: Some(allowed),
            ..CodexDetail::default()
        });
        r
    }

    /// An Antigravity reading: two groups, each with a 5h and a weekly limit.
    fn antigravity_shaped() -> Reading {
        let mut r = Reading::empty(Service::Antigravity, "a@b.c", 100);
        let grouped = |group: &str, span: Span, used| Limit {
            label: span.label(),
            ..Limit::new(
                span,
                Scope::Group(group.into()),
                Window::new(used, 0),
                Source::Poll,
            )
        };
        r.limits = vec![
            grouped("Gemini Models", Span::FiveHour, 0.9),
            grouped("Gemini Models", Span::Weekly, 0.3),
            grouped("Claude and GPT models", Span::FiveHour, 0.1),
            grouped("Claude and GPT models", Span::Weekly, 0.2),
        ];
        r
    }

    #[test]
    fn the_accessors_read_a_claude_reading() {
        let r = claude_shaped();
        assert_eq!(r.session(), Some(Window::new(0.30, 0)));
        assert_eq!(r.weekly(), Some(Window::new(0.40, 0)));
        assert_eq!(r.window(Span::Monthly, &Scope::All), None);
        let fable = key("claude-fable-5-2");
        let labels = |model: &ModelKey| -> Vec<String> {
            r.limits_for(Some(model))
                .map(|limit| limit.label.clone())
                .collect()
        };
        assert_eq!(labels(&fable), ["Fable"]);
        assert_eq!(labels(&key("claude-opus-4-8")), ["Opus 4.8"]);
        assert!(labels(&key("claude-sonnet-4-5")).is_empty());
        assert_eq!(r.limits_for(None).count(), 0);
        // Half of a half-weekly allowance, under the 60% the weekly has left.
        assert!((r.model_room(Some(&fable)).unwrap() - 0.25).abs() < 1e-9);
        assert!((r.model_room(None).unwrap() - 0.60).abs() < 1e-9);
        assert!(r.above(0.05, 0.05, Some(&fable)));
        assert!(!r.above(0.05, 0.05, Some(&key("claude-opus-4-8"))));
        assert!(!r.above(0.75, 0.05, None), "the session floor binds the 5h");
        assert!(
            !r.above(0.05, 0.65, None),
            "the weekly floor binds the weekly"
        );
        assert_eq!(r.tightest().unwrap().span, Span::Weekly);
        assert_eq!(r.scoped().len(), 2);
        assert_eq!(
            r.model_limit(&key("fable")).map(|limit| limit.share),
            Some(Some(0.5))
        );
    }

    #[test]
    fn the_accessors_read_a_codex_shaped_reading() {
        let r = codex_shaped(true);
        assert_eq!(r.session(), Some(Window::new(0.20, 500)));
        assert_eq!(r.weekly(), Some(Window::new(0.60, 9_000)));
        assert_eq!(
            r.window(Span::Other(86_400), &Scope::All),
            Some(Window::new(0.10, 700))
        );
        assert!(r.has_quota() && r.allows());
        assert_eq!(r.limits_for(Some(&key("gpt-5.3-codex-spark"))).count(), 0);
        assert!((r.model_room(None).unwrap() - 0.40).abs() < 1e-9);
        // The exhausted feature limit is metered apart: it gates nothing here.
        assert!(r.above(0.0, 0.0, None));
        assert!(!r.above(0.85, 0.05, None), "the 5h window has 80% left");
        assert!(r.above(0.05, 0.39, None));
        assert!(!r.above(0.05, 0.41, None));
        // A window of another length is held to the weekly floor.
        assert!(r.above(0.05, 0.39, None) && !r.above(0.95, 0.0, None));
        let mut daily_tight = r.clone();
        daily_tight.set_window(
            Span::Other(86_400),
            Scope::All,
            Window::new(0.97, 700),
            Source::Poll,
        );
        assert!(daily_tight.above(0.05, 0.02, None));
        assert!(!daily_tight.above(0.0, 0.05, None));
        assert_eq!(r.tightest().unwrap().span, Span::Weekly);
        assert!(!codex_shaped(false).above(0.0, 0.0, None));
        // Asked for the feature, only its own limit counts, and it is spent.
        let spark = r.scoped_to(Some("SPARK")).unwrap();
        assert_eq!(spark.limits.len(), 1);
        assert!(spark.has_quota());
        assert!(!spark.above(0.0, 0.0, None));
        assert_eq!(r.scoped_to(Some("nothing-like-it")), None);
        // No limit block at all: allowed, but nothing to rank on.
        let mut bare = Reading::empty(Service::Codex, "bare", 0);
        assert!(bare.allows() && !bare.has_quota());
        bare.error = Some(ReadError::Http {
            status: 401,
            body: String::new(),
        });
        assert!(bare.is_refused());
    }

    #[test]
    fn the_accessors_read_an_antigravity_shaped_reading() {
        let r = antigravity_shaped();
        assert_eq!(r.session(), None);
        assert_eq!(r.weekly(), None);
        assert!(!r.has_quota(), "no limit is account-wide");
        assert!(!r.above(0.0, 0.0, None));
        assert_eq!(r.model_room(None), None);
        assert_eq!(r.limits_for(Some(&key("gemini"))).count(), 0);
        assert_eq!(r.tightest().unwrap().window.used, 0.9);
        let groups: Vec<_> = r
            .scoped()
            .into_iter()
            .map(|(scope, limits)| (scope.clone(), limits.len()))
            .collect();
        assert_eq!(
            groups,
            [
                (Scope::Group("Gemini Models".into()), 2),
                (Scope::Group("Claude and GPT models".into()), 2)
            ]
        );
        // Work may land in any group: every group's limit binds.
        let any = r.scoped_to(None).unwrap();
        assert!(any.has_quota());
        assert_eq!(any.shared().count(), 4);
        assert!(any.above(0.05, 0.05, None));
        assert!(!any.above(0.15, 0.05, None), "Gemini's 5h has 10% left");
        // One group: only its limits.
        let claude = r.scoped_to(Some("claude")).unwrap();
        assert_eq!(claude.session(), Some(Window::new(0.1, 0)));
        assert_eq!(claude.weekly(), Some(Window::new(0.2, 0)));
        assert!(claude.above(0.15, 0.05, None));
        assert_eq!(r.scoped_to(Some("no such group")), None);
        let failed = Reading::failed(
            Service::Antigravity,
            "down",
            0,
            ReadError::Other {
                message: "refused".into(),
            },
        );
        assert_eq!(failed.scoped_to(None), None);
    }

    #[test]
    fn setting_a_window_keeps_what_else_is_known_about_the_limit() {
        let mut r = claude_shaped();
        let status = WindowStatus {
            verdict: Verdict::AllowedWarning,
            surpassed_threshold: Some(0.75),
        };
        r.set_status(Span::Weekly, &Scope::All, status);
        r.set_window(
            Span::Weekly,
            Scope::All,
            Window::new(1.0, 9),
            Source::Observed,
        );
        let weekly = r.limit(Span::Weekly, &Scope::All).unwrap();
        assert_eq!(weekly.window, Window::new(1.0, 9));
        assert_eq!(weekly.source, Source::Observed);
        assert_eq!(weekly.status, Some(status));
        r.remove_limit(Span::FiveHour, &Scope::All);
        assert_eq!(r.session(), None);
        assert_eq!(r.limits.len(), 3);
    }
}

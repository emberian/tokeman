//! Terminal text: gauges, reset times, labels, and the printed reports.

use chrono::{DateTime, Local, Utc};

use crate::account::Account;
use crate::anthropic::resets;
use crate::attention::Attention;
use crate::claude::sessions;
use crate::codex::probe::State;
use crate::codex::resets as codex_resets;
use crate::commands::codex::{self, Listed};
use crate::daemon::notify::{Channel, Delivery};
use crate::forecast::{self, Forecast, Outlook};
use crate::paths::tilde;
use crate::quota::{
    Balance, Detail, Level, Limit, ModelKey, Reading, ResetCreditCount, Scope, Service,
    SessionLimit, Span, Verdict, Window,
};
use crate::store::Stats;
use crate::ui::overview::{AccountView, ForecastView, Monitor, Origin, Overview};
use crate::util::one_line;

const BAR_WIDTH: usize = 40;
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[90m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// A progress line, on stderr. `main` installs this as the library's notice sink.
pub fn notice(message: &str) {
    eprintln!("{message}");
}

pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    s.char_indices()
        .nth(max_chars)
        .map_or(s, |(idx, _)| &s[..idx])
}

/// Long and short names for the limit the server named as binding.
pub fn claim_label(claim: &str) -> Option<(&'static str, &'static str)> {
    Some(match claim {
        "five_hour" => ("session", "session"),
        "seven_day" => ("weekly", "weekly"),
        "seven_day_opus" => ("Opus weekly", "Opus"),
        "seven_day_sonnet" => ("Sonnet weekly", "Sonnet"),
        "overage" => ("extra usage", "extra"),
        _ => return None,
    })
}

fn until(ts: i64) -> Result<(DateTime<Utc>, chrono::Duration), &'static str> {
    if ts == 0 {
        return Err("--");
    }
    let reset = DateTime::from_timestamp(ts, 0).unwrap_or_default();
    let diff = reset - Utc::now();
    if diff.num_seconds() < 0 {
        return Err("now");
    }
    Ok((reset, diff))
}

/// `4h29m (4:00PM)`, `Tue 12:00PM`, `12m`, `--` for no reset, `now` if past.
pub fn format_reset(ts: i64) -> String {
    let (reset, diff) = match until(ts) {
        Ok(until) => until,
        Err(text) => return text.into(),
    };
    let local = reset.with_timezone(&Local);
    let hours = diff.num_hours();
    let mins = diff.num_minutes() % 60;
    if hours > 24 {
        local.format("%a %-I:%M%p").to_string()
    } else if hours > 0 {
        format!("{hours}h{mins}m ({})", local.format("%-I:%M%p"))
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{}s", diff.num_seconds())
    }
}

pub fn format_reset_compact(ts: i64) -> String {
    let (reset, diff) = match until(ts) {
        Ok(until) => until,
        Err(text) => return text.into(),
    };
    let hours = diff.num_hours();
    let mins = diff.num_minutes() % 60;
    if hours > 24 {
        reset.with_timezone(&Local).format("%a %-I%p").to_string()
    } else if hours > 0 {
        format!("{hours}h{mins}m")
    } else {
        format!("{mins}m")
    }
}

/// Green, yellow, red: the terminal palette for [`super::level_color`].
const LEVELS: [&str; 3] = [GREEN, YELLOW, RED];

/// A forty-cell bar of the fraction left, coloured by level.
pub fn bar(used: f64) -> String {
    let remaining = (1.0 - used).clamp(0.0, 1.0);
    let filled = ((remaining * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
    let empty = BAR_WIDTH - filled;
    format!(
        "{}{}{RESET}{} {:>3}% left",
        super::level_color(Level::of(remaining), LEVELS),
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
        (remaining * 100.0).round() as u8,
    )
}

/// A word for the reading's state and the level to colour it with: Claude's
/// verdict, Codex's `ok` or `limited (...)`, `error` for a failed read of any
/// service. `None` when a service that judges nothing answered.
pub fn badge(reading: &Reading) -> Option<(String, Option<Level>)> {
    let (word, level) = match &reading.detail {
        Detail::Claude(claude) => {
            if reading.shared().next().is_none() {
                if reading.error.is_some() {
                    ("error", Some(Level::Critical))
                } else {
                    ("no quota", None)
                }
            } else {
                match claude.verdict {
                    Verdict::Allowed => ("allowed", Some(Level::Ok)),
                    Verdict::AllowedWarning => ("warning", Some(Level::Low)),
                    Verdict::Rejected => ("REJECTED", Some(Level::Critical)),
                    Verdict::Unknown => ("unknown", Some(Level::Low)),
                }
            }
        }
        Detail::Codex(_) => match State::of(reading) {
            State::Ok => ("ok", Some(Level::Ok)),
            State::Error { .. } => ("error", Some(Level::Critical)),
            limited => return Some((limited.to_string(), Some(Level::Critical))),
        },
        Detail::None => {
            reading.error.as_ref()?;
            ("error", Some(Level::Critical))
        }
    };
    Some((word.to_owned(), level))
}

/// Why the read produced no quota, on one line: an API error body as its
/// message, and for Codex what to run when the token was rejected.
pub fn error_text(reading: &Reading) -> Option<String> {
    let error = reading.error.as_ref()?;
    let text = brief_error(&error.to_string());
    Some(
        if reading.service == Service::Codex && error.is_unauthorized() {
            format!("{text}; needs `tokeman codex refresh`")
        } else {
            text
        },
    )
}

/// `HTTP 403: OAuth authentication is currently not allowed ...` for an
/// error that carries the API's JSON body; any other error as it is.
fn brief_error(error: &str) -> String {
    let message = error.find('{').and_then(|start| {
        let body: serde_json::Value = serde_json::from_str(&error[start..]).ok()?;
        // The API's `{"error": {"message": …}}` or the token endpoint's
        // `{"error": …, "error_description": …}`.
        let message = body["error"]["message"]
            .as_str()
            .or_else(|| body["error_description"].as_str())?;
        Some(format!("{}{message}", &error[..start]))
    });
    message.unwrap_or_else(|| one_line(error, 200))
}

/// What an account's login is good for, for `tokeman list`.
pub fn describe_login(account: &Account, now_ms: i64) -> String {
    let Some(login) = account.login.as_ref() else {
        return "no login (inference-only)".into();
    };
    if let Some(error) = &login.error {
        return format!(
            "login refused ({}); run `tokeman claude login {}`",
            brief_error(error),
            account.name
        );
    }
    if !login.is_refreshable() {
        return "captured usage token (not refreshable)".into();
    }
    let token = match login.expires_at {
        Some(expires_at) if expires_at > now_ms => {
            format!(
                "login, full scope, access token valid {}m",
                (expires_at - now_ms) / 60_000
            )
        }
        Some(_) => "login, access token expired (refresh pending)".into(),
        None => "login, expiry unknown".into(),
    };
    format!("{token}{}", login_lifetime(account, now_ms))
}

/// What a small surface says about the login: that it was refused, or when
/// it ends. `None` when there is nothing to say.
pub fn login_note(account: &Account, now_ms: i64) -> Option<String> {
    let login = account.login.as_ref()?;
    if login.error.is_some() {
        return Some(format!(
            "login refused: run `tokeman claude login {}`",
            account.name
        ));
    }
    let lifetime = login_lifetime(account, now_ms);
    lifetime.strip_prefix("; ").map(str::to_owned)
}

/// When the login's refresh token dies: the reported date when there is one,
/// otherwise the login's age. Empty when neither is known.
fn login_lifetime(account: &Account, now_ms: i64) -> String {
    let Some(login) = account.login.as_ref() else {
        return String::new();
    };
    let renew = format!("run `tokeman claude login {}`", account.name);
    let day = |at: i64| {
        DateTime::from_timestamp_millis(at)
            .map(|at| at.with_timezone(&Local).format("%b %-d").to_string())
            .unwrap_or_else(|| "--".into())
    };
    match (login.refresh_expires_at, login.age_days(now_ms)) {
        (Some(expires_at), _) if expires_at <= now_ms => {
            format!("; login ended {}: {renew}", day(expires_at))
        }
        (Some(expires_at), _) if login.is_ageing(now_ms) => {
            let hours = (expires_at - now_ms) / 3_600_000;
            format!(
                "; login ends {} (in {}): {renew} soon",
                day(expires_at),
                if hours >= 24 {
                    format!("{}d", hours / 24)
                } else {
                    format!("{hours}h")
                }
            )
        }
        (Some(expires_at), _) => format!("; login ends {}", day(expires_at)),
        (None, Some(days)) if login.is_ageing(now_ms) => format!(
            "; logged in {days} days ago, and refresh tokens have lasted 28 to 30 days: {renew} soon"
        ),
        (None, Some(days)) => format!("; logged in {days} days ago"),
        (None, None) => String::new(),
    }
}

/// What the headers said beyond the windows: none of it is room.
pub fn signal_lines(signals: &crate::quota::Signals) -> Vec<String> {
    let mut lines = Vec::new();
    if signals.claims.len() > 1 {
        lines.push(format!("Exceeded: {}", signals.claims.join(", ")));
    }
    if let Some(slow) = &signals.slow {
        let status = slow
            .status
            .and_then(|status| serde_json::to_value(status).ok())
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "offered".into());
        let budget = slow
            .budget
            .map(|w| format!(", budget {:.0}% used", w.used * 100.0))
            .unwrap_or_default();
        lines.push(format!("Low-priority lane: {status}{budget}"));
    }
    if let Some(grace) = &signals.grace {
        let parts: Vec<String> = [("5h", grace.session_used), ("7d", grace.weekly_used)]
            .into_iter()
            .filter_map(|(name, used)| used.map(|used| format!("{name} {:.0}%", used * 100.0)))
            .collect();
        lines.push(format!("Grace past the limit: {}", parts.join(", ")));
    }
    if !signals.upgrade_paths.is_empty() {
        lines.push(format!("Could buy: {}", signals.upgrade_paths.join(", ")));
    }
    lines
}

/// `$21.96` for dollars, `21.96 EUR` for anything else.
pub fn format_money(minor: i64, currency: &str) -> String {
    let sign = if minor < 0 { "-" } else { "" };
    let (whole, cents) = (minor.abs() / 100, minor.abs() % 100);
    match currency {
        "USD" | "" => format!("{sign}${whole}.{cents:02}"),
        other => format!("{sign}{whole}.{cents:02} {other}"),
    }
}

fn balance_text(balance: &Balance) -> String {
    let left = format_money(balance.left_minor, &balance.currency);
    match balance.total_minor {
        Some(total) => format!(
            "{} {left} of {} left",
            balance.label,
            format_money(total, &balance.currency)
        ),
        None => format!("{} {left} left", balance.label),
    }
}

/// A limit's name on a line of its own: what it covers, then its length.
/// `5h` and `weekly` for the account-wide windows, `Fable weekly` for a
/// model's, `Gemini Models 5h` for a group's, `codex_spark weekly` for a
/// limit the server names itself.
pub fn limit_name(limit: &Limit) -> String {
    let span = limit.span.label();
    let label = limit.label.as_str();
    // A label that only says how long the window is names nothing by itself.
    let is_length = label == span || matches!(label, "5h" | "weekly" | "monthly" | "daily");
    let head = match &limit.scope {
        Scope::All => return limit.name(),
        Scope::Group(group) if label.is_empty() => group.clone(),
        Scope::Group(group) if is_length => return format!("{group} {label}"),
        Scope::Model(key) if label.is_empty() => key.to_string(),
        _ => label.to_owned(),
    };
    if limit.span == Span::Unknown || head.ends_with(&span) {
        head
    } else {
        format!("{head} {span}")
    }
}

/// The limits of a reading in the order every surface shows them: the
/// account-wide windows, then each model's, then each group's, as reported.
pub fn limits_in_order(reading: &Reading) -> Vec<&Limit> {
    let rank = |limit: &Limit| match limit.scope {
        Scope::All => 0,
        Scope::Model(_) => 1,
        Scope::Group(_) => 2,
    };
    let mut limits: Vec<&Limit> = reading.limits.iter().collect();
    limits.sort_by_key(|limit| rank(limit));
    limits
}

/// For a limit that is a share of the weekly: how much of the weekly it
/// still allows.
pub fn share_note(limit: &Limit, weekly: Option<&Window>) -> Option<String> {
    limit.share.map(|share| {
        format!(
            "{:.0}% of the weekly, {:.0}% of the weekly left for it",
            share * 100.0,
            limit.room(weekly).max(0.0) * 100.0
        )
    })
}

/// What a surface says after a limit's gauge: that the server refuses on
/// it, the absolute amount, where a model's reading came from, its share of
/// the weekly, why extra usage is off.
pub fn limit_note(limit: &Limit, reading: &Reading) -> Option<String> {
    let mut parts = Vec::new();
    if limit.is_rejected() {
        parts.push("limited".to_owned());
    }
    if let Some(amount) = &limit.amount {
        parts.push(format!(
            "{:.0}/{:.0} {}",
            amount.used, amount.limit, amount.unit
        ));
    }
    if limit.key().is_some() && limit.source != crate::quota::Source::Poll {
        parts.push(limit.source.label().to_owned());
    }
    parts.extend(share_note(limit, reading.weekly().as_ref()));
    if limit.scope == Scope::extra_usage()
        && let Some(reason) = reading
            .claude()
            .and_then(|claude| claude.overage.as_ref())
            .and_then(|overage| overage.disabled_reason.as_deref())
    {
        parts.push(match reason {
            "out_of_credits" => "no credits".to_owned(),
            other => other.to_owned(),
        });
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// `2 reset credits (0 usable now)`.
fn credits_text(credits: ResetCreditCount) -> String {
    let plural = if credits.available == 1 { "" } else { "s" };
    match credits.applicable {
        Some(now) => format!(
            "{} reset credit{plural} ({now} usable now)",
            credits.available
        ),
        None => format!("{} reset credit{plural}", credits.available),
    }
}

/// What a reading says beyond its limits, one line each: Claude's missing
/// 5h limit, binding limit and header signals; Codex's email, reset
/// credits, unavailable models and spending cap; any service's balances.
pub fn notes(reading: &Reading) -> Vec<String> {
    let mut notes = Vec::new();
    match &reading.detail {
        Detail::Claude(claude) => {
            if reading.error.is_none() && reading.shared().next().is_none() {
                notes.push("no unified quota headers (an API key rather than OAuth?)".to_owned());
            }
            if reading.session().is_none() && claude.session_limit == SessionLimit::Absent {
                notes.push("no 5h limit on this account".to_owned());
            }
            if let Some(claim) = claude.claim.as_deref() {
                notes.push(format!(
                    "binding limit: {}",
                    claim_label(claim).map_or(claim, |(long, _)| long)
                ));
            }
            notes.extend(signal_lines(&claude.signals));
            if claude
                .overage
                .as_ref()
                .is_some_and(|overage| overage.in_use)
            {
                notes.push("extra usage is in use".to_owned());
            }
            if reading.has_quota() && reading.models().next().is_none() {
                notes.push(
                    match claude.profile_error {
                        Some(_) => {
                            "per-model limits unavailable (profile usage credential expired or unavailable)"
                        }
                        None => "no per-model limits reported",
                    }
                    .to_owned(),
                );
            }
        }
        Detail::Codex(codex) => {
            notes.extend(codex.email.clone());
            notes.extend(
                codex
                    .reset_credits
                    .filter(|credits| credits.available > 0)
                    .map(credits_text),
            );
            for model in codex.models.iter().filter(|model| model.is_unavailable()) {
                let mut line = format!("model {} unavailable", model.slug);
                if let Some(at) = model.available_at {
                    line.push_str(&format!(" until {}", codex::fmt_epoch(at)));
                }
                if model.credits_would_enable == Some(true) {
                    line.push_str(", credits would enable it");
                }
                notes.push(line);
            }
            if let Some(spend) = &codex.spend_control
                && (spend.reached == Some(true) || spend.limit.is_some())
            {
                let mut line = format!(
                    "spend control {}",
                    if spend.reached == Some(true) {
                        "reached"
                    } else {
                        "ok"
                    }
                );
                if let Some(cap) = &spend.limit {
                    line.push_str(&format!(
                        ", used {} of {}",
                        cap.used.as_deref().unwrap_or("?"),
                        cap.limit.as_deref().unwrap_or("?")
                    ));
                    if let Some(percent) = cap.used_percent {
                        line.push_str(&format!(" ({percent:.0}%)"));
                    }
                    if let Some(at) = cap.reset_at.filter(|at| *at > 0) {
                        line.push_str(&format!(", resets {}", format_reset(at)));
                    }
                }
                notes.push(line);
            }
        }
        Detail::None => {}
    }
    notes.extend(reading.balances.iter().map(balance_text));
    notes
}

/// `5h 99% · weekly 79% · extra usage $21.96 of $50.00 left`, or the error:
/// one reading on one line, without colour. Each limit is named and says
/// what is left of it.
pub fn summary(reading: &Reading) -> String {
    if let Some(error) = error_text(reading) {
        return format!("error: {}", one_line(&error, 100));
    }
    let mut parts: Vec<String> = limits_in_order(reading)
        .into_iter()
        .map(|limit| {
            format!(
                "{} {:.0}%",
                limit_name(limit),
                (limit.window.remaining().clamp(0.0, 1.0) * 100.0).round()
            )
        })
        .collect();
    parts.extend(reading.balances.iter().map(balance_text));
    parts.extend(
        reading
            .codex()
            .and_then(|codex| codex.reset_credits)
            .filter(|credits| credits.available > 0)
            .map(credits_text),
    );
    if let Some((word, Some(Level::Critical))) = badge(reading) {
        parts.push(one_line(&word, 80));
    }
    if parts.is_empty() {
        return "no limits reported".to_owned();
    }
    parts.join(" · ")
}

/// `3m ago`, `2h ago`, `Tue 4:00PM`: how old a read is.
pub fn format_age(at: i64, now: i64) -> String {
    let secs = (now - at).max(0);
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        _ => DateTime::from_timestamp(at, 0)
            .unwrap_or_default()
            .with_timezone(&Local)
            .format("%a %-I:%M%p")
            .to_string(),
    }
}

/// How far off `at` is from `now`, and when: `in 40m`, `in 9h12m
/// (3:10PM)`, `Sat Nov 14 3PM (in 2d)`; `now` for a time not ahead.
pub fn format_until(at: i64, now: i64) -> String {
    let left = at - now;
    let local = DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .with_timezone(&Local);
    match left {
        left if left <= 0 => "now".to_owned(),
        left if left < 3_600 => format!("in {}m", (left / 60).max(1)),
        left if left < 86_400 => format!(
            "in {}h{}m ({})",
            left / 3_600,
            left % 3_600 / 60,
            local.format("%-I:%M%p")
        ),
        left => format!(
            "{} (in {}d)",
            local.format("%a %b %-d %-I%p"),
            left / 86_400
        ),
    }
}

/// One thing that needs the user, in a sentence without its remedy.
pub fn attention_text(item: &Attention, now: i64) -> String {
    match item {
        Attention::DaemonStopped => {
            "the background service is installed but not running".to_owned()
        }
        Attention::BelowFloor {
            service: Service::Claude,
        } => "Claude: every account is below its floor".to_owned(),
        Attention::BelowFloor { service } => format!("{}: no account has room", service.label()),
        Attention::RunsDry { service, at } => format!(
            "{} runs dry {} at the last {} h rate",
            service.label(),
            format_until(*at, now),
            forecast::BURN_HOURS
        ),
        Attention::LoginRefused { account } => format!("the login of {account} was refused"),
        Attention::LoginEnds {
            account, ends_at, ..
        } if *ends_at <= now => {
            format!("the login of {account} ended {}", format_age(*ends_at, now))
        }
        Attention::LoginEnds {
            account, ends_at, ..
        } => format!(
            "the login of {account} ends {}",
            format_until(*ends_at, now)
        ),
        Attention::LoginAgeing { account } => format!(
            "the login of {account} is over {} days old, and logins have lasted 28 to 30",
            crate::account::LOGIN_WARNING_DAYS
        ),
        Attention::ResetExpires {
            service,
            account,
            expires_at,
            ..
        } => format!(
            "a reset on offer to {account} ({}) expires {}",
            service.label(),
            format_until(*expires_at, now)
        ),
    }
}

/// The command that deals with an item, when one does. A reset is named
/// for the user to spend; tokeman never spends one by itself.
pub fn attention_fix(item: &Attention) -> Option<String> {
    match item {
        Attention::DaemonStopped => Some("tokeman rotate install".to_owned()),
        Attention::LoginRefused { account }
        | Attention::LoginEnds { account, .. }
        | Attention::LoginAgeing { account } => Some(format!("tokeman claude login {account}")),
        Attention::ResetExpires {
            service, account, ..
        } => Some(format!(
            "tokeman {} reset {account}",
            crate::commands::services::command_word(*service)
        )),
        Attention::BelowFloor { .. } | Attention::RunsDry { .. } => None,
    }
}

/// `2 resets` / `1 reset`.
fn resets_word(count: u32) -> String {
    format!("{count} reset{}", if count == 1 { "" } else { "s" })
}

/// A forecast on one line: the outlook, the rate it rests on and the room
/// it started from, then what the resets on offer would add.
pub fn forecast_text(forecast: &Forecast, now: i64) -> String {
    let service = forecast.service.label();
    let basis = format!("at the last {} h rate", forecast.burn_hours);
    let accounts = if forecast.accounts == 1 {
        "1 account".to_owned()
    } else {
        format!("{} accounts", forecast.accounts)
    };
    let room = format!(
        "{:.0}% of a {} window left across {accounts}",
        forecast.room * 100.0,
        forecast.span.label()
    );
    let measured = if forecast.measured < forecast.accounts {
        format!(", measured on {}", forecast.measured)
    } else {
        String::new()
    };
    let detail = format!("{room}, used at {:.1}%/h{measured}", forecast.rate * 100.0);
    let with_resets = match forecast.with_resets {
        Some(Outlook::Dry { at }) => format!(
            "; with the {} on offer, {}",
            resets_word(forecast.refills),
            format_until(at, now)
        ),
        Some(_) => format!(
            "; with the {} on offer, not within {} days",
            resets_word(forecast.refills),
            forecast::HORIZON_SECS / 86_400
        ),
        None => String::new(),
    };
    match forecast.outlook {
        Outlook::Dry { at } if at <= now => {
            format!("{service} is dry now ({detail}){with_resets}")
        }
        Outlook::Dry { at } => format!(
            "{service} runs dry {} {basis} ({detail}){with_resets}",
            format_until(at, now)
        ),
        Outlook::Beyond => format!(
            "{service} lasts more than {} days {basis} ({detail})",
            forecast::HORIZON_SECS / 86_400
        ),
        Outlook::Idle => format!(
            "{service} was not used in the last {} h: no forecast ({room})",
            forecast.burn_hours
        ),
    }
}

/// A forecast in a few words, for a header: `Claude dry in 9h12m
/// (3:10PM)`, `Claude 7d+`, `Claude idle`.
pub fn forecast_brief(forecast: &Forecast, now: i64) -> String {
    let service = forecast.service.label();
    match forecast.outlook {
        Outlook::Dry { at } if at <= now => format!("{service} dry now"),
        Outlook::Dry { at } => format!("{service} dry {}", format_until(at, now)),
        Outlook::Beyond => format!("{service} {}d+", forecast::HORIZON_SECS / 86_400),
        Outlook::Idle => format!("{service} idle"),
    }
}

/// A stored view older than this, with the background service running, is
/// said to be stale.
pub const STALE_SECS: i64 = 600;

/// Where the view's readings come from and how old they are, on one line:
/// `read now, 14/15 accounts answered`, or `stored view, last read 2m ago by
/// the background service`.
pub fn view_caption(overview: &Overview) -> String {
    if overview.origin == Origin::Fresh {
        return format!(
            "read now, {}/{} accounts answered",
            overview.answered(),
            overview.accounts.len()
        );
    }
    let Some(read_at) = overview.read_at else {
        return "stored view, nothing read yet".to_owned();
    };
    let age = format_age(read_at, overview.at);
    match overview.monitor {
        Monitor::Running if overview.at - read_at > STALE_SECS => {
            format!("stored view, STALE: the background service last read {age}")
        }
        Monitor::Running => format!("stored view, last read {age} by the background service"),
        Monitor::Paused => format!("stored view, last read {age}; rotation is paused"),
        Monitor::Stopped => format!(
            "stored view, last read {age}; the background service is not running (`tokeman rotate install`)"
        ),
        Monitor::NotInstalled => format!("stored view, last read {age}; no background service"),
    }
}

/// What holding an account means for its service.
fn held_note(service: Service) -> &'static str {
    match service {
        Service::Claude => "the default",
        Service::Antigravity => "agy's login",
        _ => "in use",
    }
}

/// One account as a card: a heading, a gauge per limit, then what else the
/// reading says. `target` is the model Claude starts on; a limit on it is
/// marked `*`. `aged_at` is the view's time when its readings are stored
/// ones, so each card says how old its reading is.
pub fn account_lines(
    account: &AccountView,
    target: Option<&ModelKey>,
    aged_at: Option<i64>,
) -> Vec<String> {
    let reading = account.reading.as_ref();
    let mut head = format!(" {BOLD}{}{RESET} — {}", account.name, account.service_label);
    if let Some(plan) = reading.and_then(|reading| reading.plan.as_deref()) {
        head.push_str(&format!("  {DIM}[{plan}]{RESET}"));
    }
    if let Some(label) = &account.label {
        head.push_str(&format!("  {DIM}{label}{RESET}"));
    }
    if account.parked {
        head.push_str(&format!("  {DIM}parked{RESET}"));
    }
    if account.held {
        head.push_str(&format!("  {GREEN}* {}{RESET}", held_note(account.service)));
    }
    if let Some(state) = &account.state {
        let color = state
            .level
            .map_or(DIM, |level| super::level_color(level, LEVELS));
        head.push_str(&format!("  {color}{}{RESET}", state.word));
    }
    if account.service == Service::Claude
        && reading.is_some_and(Reading::has_quota)
        && let Some(viable) = account.viable
    {
        head.push_str(if viable { "  ready" } else { "  below floor" });
    }
    let mut lines = vec![head];
    let Some(reading) = reading else {
        lines.push(format!("   {DIM}not read yet{RESET}"));
        return lines;
    };
    if let Some(error) = &account.error {
        lines.push(format!("   {RED}error: {error}{RESET}"));
    }
    let limits = limits_in_order(reading);
    let width = limits
        .iter()
        .map(|limit| limit_name(limit).chars().count())
        .max()
        .unwrap_or(0);
    for limit in limits {
        let mark = if target
            .zip(limit.key())
            .is_some_and(|(target, key)| target.is_limited_by(key))
        {
            "*"
        } else {
            " "
        };
        let note = limit_note(limit, reading)
            .map(|note| format!("  {DIM}{note}{RESET}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {mark}{:<width$} {}  resets {}{note}",
            limit_name(limit),
            bar(limit.window.used),
            format_reset(limit.window.resets_at),
        ));
    }
    for note in &account.notes {
        let color = if note.starts_with('!') { YELLOW } else { DIM };
        lines.push(format!("   {color}{note}{RESET}"));
    }
    if let Some(login) = account.login.as_ref().filter(|login| login.warning)
        && let Some(note) = &login.note
    {
        lines.push(format!("   {YELLOW}{note}{RESET}"));
    }
    if let Some(now) = aged_at {
        lines.push(format!(
            "   {DIM}read {}{RESET}",
            format_age(reading.at, now)
        ));
    }
    lines
}

/// The view's time, when its readings are stored ones.
fn aged_at(overview: &Overview) -> Option<i64> {
    (overview.origin == Origin::Stored).then_some(overview.at)
}

fn target_of(overview: &Overview) -> Option<ModelKey> {
    overview.target_model.as_deref().map(ModelKey::new)
}

fn print_offers(overview: &Overview) {
    for offer in &overview.resets_on_offer {
        println!(
            " reset on offer: {}: {} (`tokeman claude reset {}`; read {})",
            offer.account,
            offer.offer,
            offer.account,
            format_age(offer.read_at, overview.at)
        );
    }
}

/// The "needs attention" section and the forecasts of the services in
/// use, as the lines every text report opens with; empty when there is
/// neither.
pub fn headlines(overview: &Overview) -> Vec<String> {
    let mut lines = Vec::new();
    if !overview.attention.is_empty() {
        lines.push(format!(" {BOLD}needs attention{RESET}"));
    }
    for item in &overview.attention {
        let color = match item.level {
            Level::Critical => RED,
            _ => YELLOW,
        };
        let fix = item
            .fix
            .as_ref()
            .map(|fix| format!("  {DIM}`{fix}`{RESET}"))
            .unwrap_or_default();
        lines.push(format!("  {color}! {}{RESET}{fix}", item.text));
    }
    for forecast in &overview.forecasts {
        let color = match forecast.forecast.outlook {
            Outlook::Dry { .. } => "",
            Outlook::Beyond => DIM,
            // A service nobody used has no time to give; `tokeman stats` says so.
            Outlook::Idle => continue,
        };
        lines.push(format!(" {color}forecast: {}{RESET}", forecast.text));
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn print_headlines(overview: &Overview) {
    for line in headlines(overview) {
        println!("{line}");
    }
}

/// `tokeman notify`: what needs the user now, and whether the background
/// service tells them.
pub fn print_attention_report(overview: &Overview, notifications: bool) {
    if overview.attention.is_empty() {
        println!("nothing needs attention ({})", view_caption(overview));
    }
    for item in &overview.attention {
        match &item.fix {
            Some(fix) => println!("! {}: `{fix}`", item.text),
            None => println!("! {}", item.text),
        }
    }
    println!(
        "{}",
        match (notifications, overview.monitor) {
            (false, _) =>
                "notifications are off (`tokeman settings set notifications true`); the rotation log still names each item",
            (true, Monitor::Running | Monitor::Paused) =>
                "the background service notifies each of these once (`tokeman notify --test` sends one now)",
            (true, _) =>
                "nothing notifies: the background service is not running (`tokeman rotate install`)",
        }
    );
}

/// `tokeman notify --test`: what became of the notification.
pub fn print_notify_test(delivery: &Delivery) {
    match delivery {
        Delivery::Sent { channel } => println!(
            "sent a test notification through {}. If none appeared, allow notifications for {} in the system's notification settings (and check Focus).",
            match channel {
                Channel::Osascript => "osascript",
                Channel::NotifySend => "notify-send",
            },
            match channel {
                Channel::Osascript => "Script Editor",
                Channel::NotifySend => "tokeman",
            }
        ),
        Delivery::NoNotifier => println!(
            "this system has no notifier tokeman knows (notify-send is not on PATH): what needs you goes to the rotation log alone"
        ),
        Delivery::Failed { reason, .. } => {
            eprintln!("the notification was not delivered: {reason}")
        }
    }
}

/// The fleet view as cards: `tokeman`, and every `<service> list`.
pub fn print_overview(overview: &Overview) {
    let hint = match overview.origin {
        Origin::Stored => format!("  {DIM}(`tokeman --fresh` reads now){RESET}"),
        Origin::Fresh => String::new(),
    };
    println!(
        "\n {BOLD}tokeman{RESET} — {}{hint}\n",
        view_caption(overview)
    );
    print_headlines(overview);
    let target = target_of(overview);
    for account in &overview.accounts {
        for line in account_lines(account, target.as_ref(), aged_at(overview)) {
            println!("{line}");
        }
        println!();
    }
    print_offers(overview);
}

/// `tokeman list`: every account of every service on a line of its own,
/// from stored reads; a Claude account's credential and login follow it.
pub fn print_list(overview: &Overview) {
    if overview.accounts.is_empty() {
        println!("{NO_ACCOUNTS}");
        return;
    }
    println!("\n {BOLD}tokeman{RESET} — {}\n", view_caption(overview));
    print_headlines(overview);
    for account in &overview.accounts {
        let detail = match &account.reading {
            Some(reading) => format!(
                "{}{}  {DIM}({}){RESET}",
                reading
                    .plan
                    .as_deref()
                    .map(|plan| format!("[{plan}] "))
                    .unwrap_or_default(),
                summary(reading),
                format_age(reading.at, overview.at)
            ),
            None => format!("{DIM}not read yet (`tokeman --fresh`){RESET}"),
        };
        let (marker, held) = if account.held {
            (
                "*",
                format!("  {GREEN}{}{RESET}", held_note(account.service)),
            )
        } else {
            (" ", String::new())
        };
        let parked = if account.parked { "parked  " } else { "" };
        println!(
            "{marker} {:<24} {:<12} {parked}{detail}{held}",
            account.name, account.service_label
        );
        if let Some(login) = &account.login {
            println!(
                "  {:<24} {:<12} {DIM}offers {}; {}{RESET}",
                "", "", login.credential, login.text
            );
        }
    }
    if !overview.resets_on_offer.is_empty() {
        println!();
    }
    print_offers(overview);
}

/// `tokeman history`: one stored read per line, newest first.
pub fn print_history(readings: &[Reading]) {
    if readings.is_empty() {
        println!("No readings recorded yet.");
        return;
    }
    println!("\n {BOLD}tokeman{RESET} — {} readings\n", readings.len());
    for reading in readings {
        let at = DateTime::from_timestamp(reading.at, 0).unwrap_or_default();
        let state = match reading.claude() {
            Some(claude) => format!(
                "  [{} {}]",
                claude.verdict.as_str(),
                claude
                    .claim
                    .as_deref()
                    .map(|claim| claim_label(claim).map_or(claim, |(_, short)| short))
                    .unwrap_or("--"),
            ),
            None => String::new(),
        };
        println!(
            "  {} {BOLD}{:<16}{RESET}  {}{state}",
            at.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
            reading.account,
            summary(reading),
        );
    }
    println!();
}

/// `tokeman stats`: how fast each limit of each account is being used,
/// then when the service's accounts run dry at the recent rate.
pub fn print_stats(stats: &[Stats], forecast: Option<&ForecastView>) {
    if stats.is_empty() {
        println!("No stats available. Need at least 2 readings per account.");
        return;
    }
    println!("\n {BOLD}tokeman stats{RESET}\n");
    for account in stats {
        println!(
            " {BOLD}{}{RESET} ({} readings)",
            account.account, account.readings
        );
        if account.readings < 2 {
            println!("   Need at least 2 readings to compute rates.\n");
            continue;
        }
        for burn in &account.limits {
            let line = |what: &str, value: Option<f64>, unit: &str| {
                if let Some(value) = value {
                    println!(
                        "   {:<34}{value:.4}{unit}",
                        format!("{} {what}:", burn.label)
                    );
                }
            };
            line("burn rate (latest)", burn.latest, "/hr");
            line("burn rate (mean)", burn.mean, "/hr");
            line("burn rate (stddev)", burn.stddev, "/hr");
            line("burn rate (peak)", burn.peak, "/hr");
            line("time to deplete", burn.hours_left, "h");
        }
        println!();
    }
    match forecast {
        Some(forecast) => println!(" forecast: {}\n", forecast.text),
        None => println!(
            " forecast: none (it needs an hour of readings of one reset window within the last {} h)\n",
            forecast::BURN_HOURS
        ),
    }
}

/// `tokeman rotate status`: the rotation's state, then every Claude account
/// as it was just read.
pub fn print_status(overview: &Overview) {
    let rotation = &overview.rotation;
    println!(
        "monitor: {}",
        match overview.monitor {
            Monitor::Running => "running",
            Monitor::Paused => "paused",
            Monitor::Stopped => "installed but not running (`tokeman rotate install` restarts it)",
            Monitor::NotInstalled => "no background service",
        }
    );
    println!(
        "mode: {} ({}s sampling)",
        rotation.mode.label(),
        rotation.probe_interval_secs
    );
    println!(
        "quota probe: {} (capacity signal; large-model admission can be stricter)",
        crate::anthropic::probe::PROBE_MODEL_LABEL
    );
    if let Some(coverage) = &rotation.coverage {
        println!("premium admission: {coverage}");
    }
    if let Some(sampling) = &rotation.sampling {
        println!("header-only limits: {sampling}");
    }
    for offer in &overview.resets_on_offer {
        println!(
            "reset on offer: {}: {} (`tokeman claude reset`)",
            offer.account, offer.offer
        );
    }
    println!(
        "default for newly started Claude processes: {}",
        match (&overview.default_account, rotation.default_is_unmanaged) {
            (Some(name), _) => name,
            (None, true) => "unmanaged OAuth token",
            (None, false) => "/login",
        }
    );
    println!(
        "selection model: {} (only the limits marked * affect rotation)",
        overview.target_model.as_deref().unwrap_or("unknown")
    );
    let live = rotation.live_sessions.as_deref();
    println!(
        "live Claude processes: {} ({})",
        live.map_or("?".to_string(), |sessions| sessions.len().to_string()),
        sessions::summary(live)
    );
    println!(
        "    each spends the default from its next request; the account shown is what settings.json offers now{}",
        if live
            .unwrap_or_default()
            .iter()
            .any(|session| session.source == sessions::Attribution::SessionStart)
        {
            ", ~ = SessionStart hook record (no credential history)"
        } else {
            ""
        }
    );
    println!(
        "default thresholds: change at 5h <= {:.0}% left or 7d <= {:.0}% left\n",
        rotation.min_session_remaining * 100.0,
        rotation.min_weekly_remaining * 100.0
    );
    let target = target_of(overview);
    for account in &overview.accounts {
        let mut lines = account_lines(account, target.as_ref(), aged_at(overview));
        if let Some(login) = &account.login {
            lines.insert(1, format!("   offers {}", login.credential));
        }
        for line in lines {
            println!("{line}");
        }
        println!();
    }
}

// ---- what each command says ----

pub const NO_ACCOUNTS: &str = "No accounts configured. Use `tokeman claude login <name>` or `tokeman claude add <name> <key>` to add one.";

/// A value as pretty JSON on stdout.
pub fn print_json<T: serde::Serialize + ?Sized>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// A question on stdout, left unterminated for the answer.
pub fn print_question(question: &str) {
    use std::io::Write;
    print!("{question}");
    std::io::stdout().flush().ok();
}

pub fn print_account_change(verb: &str, name: &str) {
    println!("{verb} account '{name}'");
}

pub fn print_removed(name: &str) {
    println!("removed {name}");
}

pub fn print_kept() {
    println!("kept.");
}

pub fn print_claude_refresh(refreshed: &crate::commands::claude::Refreshed) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    for account in &refreshed.accounts {
        println!("  {} — {}", account.name, describe_login(account, now_ms));
    }
    println!("{} grant(s) refreshed.", refreshed.count);
}

pub fn print_claude_use(name: &str) {
    println!("set startup default to {name}; live sessions follow it from their next request");
}

// ---- the browser login, step by step ----

pub fn print_login_none_needed() {
    println!("every configured account already has a refreshable login.");
}

pub fn print_login_step(index: usize, total: usize, account: &str, configured: bool, url: &str) {
    println!("\n[{}/{total}] {account}", index + 1);
    if !configured {
        println!("  (not configured yet: it will be added as a login-only account)");
    }
    println!("  sign in as this account, then paste what the page shows:\n\n  {url}\n");
    print_question("  code (or `skip` / `q`): ");
}

pub fn print_login_skipped(account: &str) {
    println!("  skipped {account}");
}

pub fn print_login_outcome(
    account: &str,
    outcome: &anyhow::Result<crate::commands::claude::LoginOutcome>,
) {
    use crate::commands::claude::LoginOutcome;
    match outcome {
        Ok(LoginOutcome::NoRefreshToken) => {
            eprintln!("  {account}: the grant came back without a refresh token; not stored")
        }
        Ok(LoginOutcome::Stored { scopes, profile }) => {
            if !profile {
                eprintln!(
                    "  {account}: granted scopes are {scopes:?}; without user:profile, /feedback and usage reads stay refused"
                );
            }
            println!("  {account}: stored ({})", scopes.join(" "));
        }
        Err(error) => eprintln!("  {account}: {error:#}"),
    }
}

pub fn print_login_summary(done: usize, total: usize) {
    println!("\n{done}/{total} account(s) updated.");
    if done > 0 {
        println!(
            "the rotation service installs these on its next cycle; `tokeman rotate status` shows which credential each account uses."
        );
    }
}

// ---- Claude resets ----

/// `tokeman claude reset` with no account: every account's offers.
pub fn print_reset_offers(listings: &[crate::commands::claude::ResetListing]) {
    let now = chrono::Utc::now().timestamp();
    for listing in listings {
        let Some(status) = &listing.status else {
            continue;
        };
        println!("{}", listing.account);
        match status {
            Ok(status) => {
                print_reset_status(status, listing.pending.as_ref(), &listing.account, now)
            }
            Err(error) => println!("  unavailable: {error}"),
        }
    }
    let without: Vec<&str> = listings
        .iter()
        .filter(|listing| listing.status.is_none())
        .map(|listing| listing.account.as_str())
        .collect();
    if !without.is_empty() {
        println!(
            "{DIM}no login (resets cannot be read): {}; run `tokeman claude login <name>`{RESET}",
            without.join(", ")
        );
    }
}

fn print_reset_status(
    status: &resets::ResetStatus,
    pending: Option<&crate::claims::Pending>,
    account: &str,
    now: i64,
) {
    use resets::Standing;
    let reason =
        |reason: &Option<String>| reason.clone().unwrap_or_else(|| "no reason given".into());
    match &status.cedar_ember {
        Some(program) if program.standing() == Standing::Eligible => {
            let grants = program.grants();
            if grants.is_empty() {
                println!("  limit resets: none on offer");
            }
            for grant in grants {
                let next = program.next_grant_id.as_deref() == Some(grant.id.as_str());
                println!(
                    "  {} {}",
                    if next { "*" } else { " " },
                    resets::describe_grant(&grant, now)
                );
            }
            if let Some(until) = &program.cooldown_until {
                println!("    cooling down until {until}");
            }
        }
        Some(program) if program.standing() == Standing::Ineligible => println!(
            "  limit resets: not eligible ({})",
            reason(&program.ineligible_reason)
        ),
        Some(program) => println!(
            "  limit resets: unknown (the server did not evaluate this account: {})",
            reason(&program.ineligible_reason)
        ),
        None => println!("  limit resets: unknown (not reported)"),
    }
    if let Some(pending) = pending {
        println!(
            "  ! a claim of grant {} has no known outcome; check Settings > Usage on claude.ai, then resend it with `tokeman claude reset {account}` or forget it with `tokeman claude reset {account} --clear`",
            pending.item.as_deref().unwrap_or("?")
        );
    }
    match &status.juniper_tide {
        Some(session) if session.available => println!(
            "  session reset: available ({} per week)",
            session.resets_per_week.unwrap_or(1)
        ),
        Some(session) if session.standing() == Standing::Unknown => println!(
            "  session reset: unknown (the server did not evaluate this account: {})",
            reason(&session.ineligible_reason)
        ),
        Some(session) => println!(
            "  session reset: not available ({}{})",
            session
                .ineligible_reason
                .as_deref()
                .unwrap_or("not offered"),
            session
                .next_available_at
                .as_deref()
                .map(|at| format!("; next {at}"))
                .unwrap_or_default()
        ),
        None => println!("  session reset: unknown (not reported)"),
    }
    for error in &status.errors {
        println!("  unavailable: {error}");
    }
}

/// What a reset would clear, shown before asking.
pub fn print_reset_plan(plan: &crate::commands::claude::ResetPlan) {
    use crate::commands::claude::ResetChoice;
    match &plan.choice {
        ResetChoice::Session => println!(
            "{}: session reset (refills the 5-hour limit, paid from the weekly limit)",
            plan.account.name
        ),
        ResetChoice::Grant(grant) => println!(
            "{}: {}",
            plan.account.name,
            resets::describe_grant(grant, plan.at)
        ),
    }
}

pub fn print_reset_claimed(outcome: &resets::ClaimResponse) {
    let cleared: Vec<&str> = outcome
        .cleared
        .iter()
        .map(|window| resets::window_label(window))
        .collect();
    match &outcome.result {
        resets::ClaimResult::Reset => println!(
            "reset: cleared {}{}",
            if cleared.is_empty() {
                "nothing reported".to_string()
            } else {
                cleared.join(" ")
            },
            outcome
                .resets_left
                .map(|left| format!("; {left} left"))
                .unwrap_or_default()
        ),
        other => println!(
            "not applied: {other:?}{}{}",
            outcome
                .reason
                .as_deref()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default(),
            outcome
                .cooldown_until
                .as_deref()
                .map(|at| format!("; cooldown until {at}"))
                .unwrap_or_default()
        ),
    }
}

/// `--clear` on either reset command: the claim that was forgotten.
pub fn print_reset_cleared(name: &str, pending: Option<&crate::claims::Pending>) {
    match pending {
        Some(pending) => println!(
            "{name}: forgot the claim of {} (request {})",
            match (pending.service, pending.item.as_deref()) {
                (Service::Codex, Some(credit)) => format!("credit {credit}"),
                (Service::Codex, None) => "one reset credit".to_owned(),
                (_, item) => format!("grant {}", item.unwrap_or("?")),
            },
            pending.request_id
        ),
        None => println!("{name}: no claim pending"),
    }
}

// ---- Codex ----

/// One line per reset credit an account holds, or why the list is missing.
pub fn credit_lines(listed: &Listed) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(list) = &listed.reset_credit_list {
        lines.extend(
            list.credits
                .iter()
                .map(|credit| format!("   reset credit {}", codex_resets::describe(credit))),
        );
    }
    if let Some(error) = &listed.reset_credit_error {
        lines.push(format!(
            "   reset credit details unavailable ({error}); the count above is from the usage body"
        ));
    }
    lines
}

pub fn print_codex_added(account: &crate::account::CodexAccount) {
    println!("registered {} at {}", account.name, account.codex_home);
}

pub fn print_codex_discovery(discovery: &codex::Discovery, adopt: bool) {
    if discovery.profiles.is_empty() {
        println!("no Codex profiles found");
        return;
    }
    for profile in &discovery.profiles {
        println!(
            "{} {:<16} {}",
            if profile.registered { "*" } else { " " },
            profile.name,
            tilde(&profile.path)
        );
    }
    if adopt {
        println!("registered {} new profile(s)", discovery.adopted);
    } else if discovery.profiles.iter().any(|profile| !profile.registered) {
        println!("re-run with --adopt to register these");
    }
}

pub fn print_codex_imported(imported: &[codex::Imported]) {
    for item in imported {
        println!(
            "{:<16} {}  {}{}{}",
            item.name,
            tilde(&item.home),
            item.plan.as_deref().unwrap_or("plan unknown"),
            item.expires_at
                .map(|at| format!(", expires {}", format_reset(at)))
                .unwrap_or_default(),
            if item.registered {
                ""
            } else {
                "  (not registered)"
            }
        );
    }
    println!(
        "{} profile(s) seeded from access tokens; they cannot refresh and die when the token expires",
        imported.len()
    );
}

/// `tokeman codex list`: every profile as a card, with the reset credits it
/// holds.
pub fn print_codex_listed(overview: &Overview, listed: &[codex::Listed]) {
    println!("\n {BOLD}tokeman{RESET} — {}\n", view_caption(overview));
    for account in &overview.accounts {
        for line in account_lines(account, None, aged_at(overview)) {
            println!("{line}");
        }
        for line in listed
            .iter()
            .filter(|entry| entry.probe.account == account.name)
            .flat_map(credit_lines)
        {
            println!("{line}");
        }
        println!();
    }
}

/// One reading as a card, for a command that just read one account.
fn print_reading(reading: &Reading, held: bool) {
    let mut account = AccountView::of(reading.service, &reading.account, Some(reading));
    account.held = held;
    for line in account_lines(&account, None, None) {
        println!("{line}");
    }
    println!();
}

pub fn print_codex_refresh(reports: &[codex::RefreshReport]) {
    for report in reports {
        println!("{report}");
    }
}

pub fn print_codex_used(used: &codex::Used) {
    let warn = |warnings: &[codex::Warning]| {
        for warning in warnings {
            eprintln!("warning: {warning}");
        }
    };
    let account = |info: &Option<crate::codex::appserver::AccountInfo>| {
        info.as_ref()
            .map_or("unknown".to_string(), ToString::to_string)
    };
    match used {
        codex::Used::Seated(seated) => {
            warn(&seated.warnings);
            println!("seated {} into {}", seated.account, tilde(&seated.into));
        }
        codex::Used::Reseated(reseat) => {
            warn(&reseat.warnings);
            match &reseat.moved {
                None => println!(
                    "would move {} (socket {}) onto {}",
                    tilde(&reseat.target),
                    tilde(&reseat.socket),
                    reseat.account
                ),
                Some(moved) => println!(
                    "moved {} onto {}: {} -> {}",
                    tilde(&reseat.target),
                    reseat.account,
                    account(&moved.before),
                    account(&moved.after)
                ),
            }
        }
    }
}

pub fn print_codex_watch(event: &codex::WatchEvent) {
    match event {
        codex::WatchEvent::Watching {
            target,
            min_remaining,
        } => println!(
            "watching {} (migrate below {:.0}% left)",
            tilde(target),
            min_remaining * 100.0
        ),
        codex::WatchEvent::Headroom { remaining, span } => {
            println!("{:.0}% left in the {span} window", remaining * 100.0)
        }
        codex::WatchEvent::Migrating => println!("below the threshold; migrating"),
        codex::WatchEvent::Migrated(reseat) => println!("moved onto {}", reseat.account),
    }
}

/// What a credit claim would do, shown before asking; returns the words for
/// the credit, for the question.
pub fn print_codex_reset_plan(plan: &codex_resets::ResetPlan) -> String {
    print_reading(&plan.probe, false);
    match &plan.credits {
        Ok(list) => {
            for credit in &list.credits {
                println!("   reset credit {}", codex_resets::describe(credit));
            }
        }
        Err(error) => println!("   reset credit details unavailable ({error})"),
    }
    if let Some(pending) = &plan.pending {
        println!(
            "an earlier claim (request {}, started {}) got no definite answer; this retries it with the same request id",
            pending.request_id,
            DateTime::from_timestamp(pending.sent_at, 0)
                .map(|at| at
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string())
                .unwrap_or_else(|| pending.sent_at.to_string())
        );
    }
    match (plan.chosen(), &plan.credit_id) {
        (Some(chosen), _) => codex_resets::describe(chosen),
        (None, Some(id)) => format!("credit {id}"),
        (None, None) => "one reset credit (the backend's choice)".to_owned(),
    }
}

pub fn print_codex_reset_claimed(name: &str, claim: &codex_resets::Claim) {
    use codex_resets::ConsumeCode::*;
    if let codex_resets::Claim::Answered {
        consumed, retried, ..
    } = claim
    {
        let text = match consumed.code {
            Reset => format!("reset: {} window(s) cleared", consumed.windows_reset),
            AlreadyRedeemed if *retried => {
                "the earlier claim went through; nothing more was spent".to_owned()
            }
            AlreadyRedeemed => "already redeemed; nothing more was spent".to_owned(),
            NothingToReset => "nothing to reset; no credit was spent".to_owned(),
            NoCredit => "no credit to spend".to_owned(),
        };
        println!("{name}: {text}");
    }
}

/// The error for a claim that got no definite answer; `None` when it did.
pub fn codex_reset_uncertain(name: &str, claim: &codex_resets::Claim) -> Option<String> {
    match claim {
        codex_resets::Claim::Answered { .. } => None,
        codex_resets::Claim::Uncertain {
            redeem_request_id,
            error,
        } => Some(format!(
            "no definite answer ({error}); the claim may or may not have gone through. \
             Run `tokeman codex reset {name}` again: it resends request {redeem_request_id}, \
             which the backend redeems at most once. `tokeman codex reset {name} --clear` \
             forgets the claim once the usage page shows what happened"
        )),
    }
}

// ---- the other services ----

fn registration_line(registered: &crate::commands::services::Registered) -> String {
    format!(
        "{} {} ({})",
        if registered.replaced {
            "updated"
        } else {
            "registered"
        },
        registered.account.name,
        registered.account.service.label()
    )
}

pub fn print_service_added(registered: &crate::commands::services::Registered, reading: &Reading) {
    println!(
        "{} reading its key from {}",
        registration_line(registered),
        registered.account.key_file.as_deref().unwrap_or_default()
    );
    println!();
    print_reading(reading, false);
}

pub fn print_agy_imported(imported: &crate::commands::services::Imported) {
    if imported.registered.is_empty() {
        println!("no signed-in accounts found");
        return;
    }
    for registered in &imported.registered {
        println!("{}", registration_line(registered));
    }
    println!();
    for reading in &imported.readings {
        print_reading(
            reading,
            imported.seated.as_deref() == Some(reading.account.as_str()),
        );
    }
}

pub fn print_agy_use(report: &crate::providers::antigravity::seat::UseReport) {
    for line in &report.lines() {
        println!("{line}");
    }
}

/// The line before `tokeman agy run` hands the terminal to agy, on stderr so
/// agy's own output stays alone on stdout.
pub fn print_agy_run(launch: &crate::providers::antigravity::run::Launch) {
    use crate::providers::antigravity::run::Login;
    match &launch.login {
        Login::Normal => eprintln!(
            "{DIM}agy is signed in as {}; running it as is{RESET}",
            launch.account
        ),
        Login::Own { home } => eprintln!(
            "{DIM}agy as {} (its own login in {}){RESET}",
            launch.account,
            crate::paths::tilde(home)
        ),
    }
}

/// After `tokeman agy list`'s gauges: the sessions on homes of their own, or
/// the reason they could not be listed. Nothing when there are none.
pub fn print_agy_sessions(
    sessions: &anyhow::Result<Vec<crate::providers::antigravity::run::Session>>,
) {
    let sessions = match sessions {
        Ok(sessions) if sessions.is_empty() => return,
        Ok(sessions) => sessions,
        Err(error) => {
            println!("\n{DIM}agy sessions on their own login: not listed ({error:#}){RESET}");
            return;
        }
    };
    println!("\nagy on its own login (tokeman agy run):");
    for session in sessions {
        let account = session
            .account
            .clone()
            .unwrap_or_else(|| format!("{} (no such account)", session.home));
        let place = session
            .cwd
            .as_deref()
            .map(|cwd| format!(" in {}", crate::paths::tilde(cwd)))
            .unwrap_or_default();
        println!(
            "  {account}: {} pid {}{place}",
            session.kind.label(),
            session.pid
        );
    }
}

/// What `tokeman <service> list` says when the service has no account.
pub fn print_no_accounts(service: Service) {
    println!(
        "No {} accounts configured. Use `{}`.",
        service.label(),
        match service {
            Service::Claude => "tokeman claude login <name>".to_owned(),
            Service::Codex => "tokeman codex discover --adopt".to_owned(),
            Service::Antigravity => "tokeman agy import".to_owned(),
            other => format!(
                "tokeman {} add <name> --key-file <path>",
                crate::commands::services::command_word(other)
            ),
        }
    );
}

/// `tokeman <service> best`. Codex prints the winner's CODEX_HOME alone, for
/// `CODEX_HOME=$(tokeman codex best)`; the others print the ranking, the
/// accounts that could not be read, and how to switch to the winner.
pub fn print_best(best: &crate::commands::view::Best) {
    if best.service == Service::Codex {
        if let Some(home) = &best.home {
            println!("{}", home.display());
        }
        return;
    }
    let note = held_note(best.service);
    for (place, (name, room)) in best.ranked.iter().enumerate() {
        let Some(reading) = best.reading(name) else {
            continue;
        };
        let held = if best.current.as_deref() == Some(name.as_str()) {
            format!("  {GREEN}* {note}{RESET}")
        } else {
            String::new()
        };
        println!(
            " {}. {BOLD}{:<24}{RESET} {:>3.0}% left  {DIM}{}{RESET}{held}",
            place + 1,
            name,
            (room.max(0.0) * 100.0).round(),
            summary(reading)
        );
    }
    for reading in &best.readings {
        if let Some(error) = &reading.error {
            println!(
                " -  {} could not be read: {}",
                reading.account,
                one_line(&error.to_string(), 120)
            );
        }
    }
    let Some(winner) = best.winner() else {
        return;
    };
    if best.current.as_deref() != Some(winner) {
        match best.service {
            Service::Claude => println!("to switch Claude Code: tokeman claude use {winner}"),
            Service::Antigravity => println!("to switch agy: tokeman agy use {winner}"),
            _ => {}
        }
    }
}

/// Why `best` has no winner.
pub fn nothing_ranked(best: &crate::commands::view::Best) -> String {
    match (best.service, &best.scope) {
        (Service::Codex, Some(limit)) => {
            format!("no viable codex account with headroom for {limit}")
        }
        (Service::Codex, None) => "no viable codex account (all limited or erroring)".into(),
        (Service::Claude, _) => "no Claude account is above the landing floors".into(),
        (service, _) => format!(
            "no {} account has room in a readable limit",
            service.label()
        ),
    }
}

// ---- rotation, settings, chart ----

pub fn print_outcome(outcome: &crate::daemon::cycle::Outcome) {
    println!("{}", outcome.message);
}

pub fn print_paused() {
    println!("rotation paused; Claude Code will use /login credentials");
}

pub fn print_resumed(outcome: &crate::daemon::cycle::Outcome) {
    println!("rotation resumed; {}", outcome.message);
}

pub fn print_installed(plist: &std::path::Path) {
    println!("installed and started {}", plist.display());
}

pub fn print_uninstalled() {
    println!("uninstalled the rotation service");
}

pub fn print_quarantined(name: &str, outcome: &crate::daemon::cycle::Outcome) {
    println!("quarantined {name}");
    println!("{}", outcome.message);
}

pub fn print_quarantines_cleared(name: &str, removed: usize) {
    println!("removed {removed} quarantine(s) for {name}");
}

pub fn print_settings(entries: &[(&'static str, String)]) {
    for (key, value) in entries {
        println!("{key} = {value}");
    }
}

pub fn print_setting_set(key: &str, value: &str) {
    println!("set {key} = {value}");
}

/// Where the chart went: inline for iTerm2, otherwise the file's path.
pub fn print_chart(chart: &crate::ui::chart::Rendered) {
    match &chart.inline {
        Some(escape) => println!("{escape}"),
        None => println!(
            "{} (run directly inside iTerm2, or pass --iterm, for an inline chart)",
            chart.output.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::probe::tests::{usage_body, weekly_only_body};
    use serde_json::json;

    #[test]
    fn an_error_with_a_json_body_is_shown_by_its_message() {
        assert_eq!(
            brief_error(
                r#"token endpoint returned HTTP 400: {"error": "invalid_grant", "error_description": "Refresh token expired"}"#
            ),
            "token endpoint returned HTTP 400: Refresh token expired"
        );
        assert_eq!(
            brief_error(
                r#"HTTP 403: {"type":"error","error":{"type":"permission_error","message":"Not allowed."}}"#
            ),
            "HTTP 403: Not allowed."
        );
        assert_eq!(brief_error("network: timed out"), "network: timed out");
    }

    #[test]
    fn text_helpers_count_characters_not_bytes() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("💥💥💥", 2), "💥💥…");
        assert_eq!(truncate_chars("héllo", 2), "hé");
    }

    #[test]
    fn a_usage_reads_as_one_line_of_room_left() {
        use crate::providers::{failed, limit};
        use crate::quota::{Service, Window};
        let meter = |group: Option<&str>, label: &str, used: f64| {
            limit(
                group.map(str::to_owned),
                label,
                None,
                Window::new(used, 0),
                None,
            )
        };
        let usage = Reading {
            plan: Some("Vivace".into()),
            limits: vec![meter(None, "5h", 0.0057), meter(None, "weekly", 1.2)],
            balances: vec![Balance {
                label: "extra usage".into(),
                left_minor: 2196,
                total_minor: Some(5000),
                currency: "USD".into(),
            }],
            ..Reading::empty(Service::Kimi, "kimi", 0)
        };
        assert_eq!(
            summary(&usage),
            "5h 99% · weekly 0% · extra usage $21.96 of $50.00 left"
        );
        let failed = failed(Service::Zai, "glm", 0, "z.ai refused:\n bad key");
        assert_eq!(summary(&failed), "error: z.ai refused: bad key");
        let grouped = Reading {
            limits: vec![meter(Some("Gemini Models"), "5h", 0.5)],
            ..Reading::empty(Service::Antigravity, "a", 0)
        };
        assert_eq!(summary(&grouped), "Gemini Models 5h 50%");
        assert_eq!(
            summary(&Reading::empty(Service::Zai, "glm", 0)),
            "no limits reported"
        );
        assert_eq!(format_money(-5, "EUR"), "-0.05 EUR");
        assert_eq!(format_age(100, 100 + 7200), "2h ago");
    }

    #[test]
    fn the_login_note_is_the_refusal_or_the_end_date_alone() {
        const DAY_MS: i64 = 86_400_000;
        assert_eq!(login_note(&crate::testing::setup_account("a"), 0), None);
        let mut account = crate::testing::login_account("acct", 40 * DAY_MS);
        let login = account.login.as_mut().unwrap();
        login.logged_in_at = None;
        assert_eq!(login_note(&account, DAY_MS), None);
        account.login.as_mut().unwrap().refresh_expires_at = Some(30 * DAY_MS);
        let note = login_note(&account, DAY_MS).unwrap();
        assert!(note.starts_with("login ends "), "{note}");
        account.login.as_mut().unwrap().error = Some("invalid_grant".into());
        assert_eq!(
            login_note(&account, DAY_MS).as_deref(),
            Some("login refused: run `tokeman claude login acct`")
        );
    }

    #[test]
    fn resets_in_the_past_or_absent_read_as_words() {
        assert_eq!(format_reset(0), "--");
        assert_eq!(format_reset(1), "now");
        assert_eq!(format_reset_compact(0), "--");
    }

    #[test]
    fn signals_are_described_but_quiet_when_absent() {
        let mut reading = crate::testing::reading("a", 0.1, 0.2);
        assert!(signal_lines(&reading.claude().unwrap().signals).is_empty());
        let signals = &mut reading.claude_mut().signals;
        signals.claims = vec!["seven_day".into(), "seven_day_overage_included".into()];
        signals.slow = Some(crate::quota::SlowLane {
            status: Some(crate::quota::SlowStatus::SlotBusy),
            budget: Some(crate::quota::Window::new(0.25, 0)),
            ..Default::default()
        });
        signals.upgrade_paths = vec!["overage".into()];
        let lines = signal_lines(&reading.claude().unwrap().signals);
        assert_eq!(
            lines,
            [
                "Exceeded: seven_day, seven_day_overage_included",
                "Low-priority lane: slot_busy, budget 25% used",
                "Could buy: overage",
            ]
        );
    }

    #[test]
    fn a_shared_limit_says_how_much_of_the_weekly_it_still_allows() {
        let mut reading = crate::testing::reading("a", 0.0, 0.4);
        let mut fable =
            crate::testing::model_limit("fable", "Fable", 0.5, crate::quota::Source::Profile);
        assert_eq!(share_note(&fable, reading.weekly().as_ref()), None);
        assert_eq!(limit_note(&fable, &reading).as_deref(), Some("profile"));
        fable.share = Some(0.5);
        reading.limits.push(fable.clone());
        assert_eq!(
            share_note(&fable, reading.weekly().as_ref()).as_deref(),
            Some("50% of the weekly, 25% of the weekly left for it")
        );
    }

    #[test]
    fn the_login_line_uses_the_reported_refresh_expiry_before_the_age() {
        const DAY_MS: i64 = 86_400_000;
        let mut account = crate::testing::login_account("acct", 40 * DAY_MS);
        let login = account.login.as_mut().unwrap();
        login.logged_in_at = Some(0);
        login.refresh_expires_at = Some(30 * DAY_MS);
        let line = describe_login(&account, 20 * DAY_MS);
        assert!(line.contains("; login ends "), "{line}");
        assert!(!line.contains("soon"), "{line}");
        let line = describe_login(&account, 28 * DAY_MS);
        assert!(
            line.contains("(in 2d): run `tokeman claude login acct` soon"),
            "{line}"
        );
        let line = describe_login(&account, 31 * DAY_MS);
        assert!(line.contains("login ended"), "{line}");

        account.login.as_mut().unwrap().refresh_expires_at = None;
        let line = describe_login(&account, 22 * DAY_MS);
        assert!(line.contains("logged in 22 days ago"), "{line}");
        assert!(line.contains("soon"), "{line}");
    }

    /// The lines without their colour codes.
    fn plain(lines: Vec<String>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| {
                let mut out = String::new();
                let mut rest = line.as_str();
                while let Some(start) = rest.find('\x1b') {
                    out.push_str(&rest[..start]);
                    let end = rest[start..]
                        .find('m')
                        .map_or(rest.len(), |m| start + m + 1);
                    rest = &rest[end..];
                }
                out.push_str(rest);
                out
            })
            .collect()
    }

    fn card(reading: &Reading) -> Vec<String> {
        let account = AccountView::of(reading.service, &reading.account, Some(reading));
        plain(account_lines(&account, None, None))
    }

    #[test]
    fn a_limit_is_named_by_what_it_covers_and_how_long_it_is() {
        use crate::providers::limit;
        use crate::quota::Source;
        let window = Window::new(0.5, 0);
        let shared = |span| Limit::new(span, Scope::All, window, Source::Headers);
        assert_eq!(limit_name(&shared(Span::FiveHour)), "5h");
        assert_eq!(limit_name(&shared(Span::Weekly)), "weekly");
        assert_eq!(limit_name(&shared(Span::Other(86_400))), "daily");
        let fable = crate::testing::model_limit("fable", "Fable", 0.5, Source::Profile);
        assert_eq!(limit_name(&fable), "Fable weekly");
        let group =
            |label: &str, secs| limit(Some("Gemini Models".into()), label, secs, window, None);
        assert_eq!(
            limit_name(&group("weekly", Some(Span::WEEK_SECS))),
            "Gemini Models weekly"
        );
        assert_eq!(limit_name(&group("5h", None)), "Gemini Models 5h");
        assert_eq!(limit_name(&group("", Some(18_000))), "Gemini Models 5h");
        // A limit the server names itself keeps that name.
        let spark = Limit {
            label: "codex_spark".into(),
            ..Limit::new(
                Span::Weekly,
                Scope::Group("gpt-5.3-codex-spark".into()),
                window,
                Source::Poll,
            )
        };
        assert_eq!(limit_name(&spark), "codex_spark weekly");
        assert_eq!(
            limit_name(&Limit::extra_usage(window, Source::Headers)),
            "extra usage"
        );
        assert_eq!(
            limit_name(&limit(
                None,
                "monthly (Kimi Code share)",
                None,
                window,
                None
            )),
            "monthly (Kimi Code share)"
        );
    }

    #[test]
    fn every_service_is_drawn_by_the_one_card() {
        use crate::quota::Source;
        // Claude: account-wide windows, then the models, then extra usage.
        let mut claude = crate::testing::reading("a", 0.25, 0.4);
        claude
            .limits
            .insert(0, Limit::extra_usage(Window::new(0.1, 0), Source::Headers));
        let mut fable = crate::testing::model_limit("fable", "Fable", 0.5, Source::Profile);
        fable.share = Some(0.5);
        claude.limits.push(fable);
        claude.claude_mut().claim = Some("seven_day".into());
        let mut account = AccountView::of(Service::Claude, "a", Some(&claude));
        account.held = true;
        account.viable = Some(false);
        let target = ModelKey::new("claude-fable-5-1");
        let lines = plain(account_lines(
            &account,
            Some(&target),
            Some(claude.at + 120),
        ));
        assert_eq!(lines[0], " a — Claude  * the default  allowed  below floor");
        let names: Vec<&str> = lines[1..5]
            .iter()
            .map(|line| line[2..15].trim_end())
            .collect();
        assert_eq!(names, [" 5h", " weekly", "*Fable weekly", " extra usage"]);
        assert!(lines[1].contains(" 75% left  resets --"), "{}", lines[1]);
        assert!(
            lines[3].ends_with("profile; 50% of the weekly, 25% of the weekly left for it"),
            "{}",
            lines[3]
        );
        assert_eq!(lines[5], "   binding limit: weekly");
        assert_eq!(lines.last().unwrap(), "   read 2m ago");

        // Codex: the same card, with what only Codex says as notes.
        let at = DateTime::from_timestamp(1_000, 0).unwrap();
        let mut body = usage_body(30.0, 60.0);
        body["rate_limit"]["primary_window"]["reset_at"] = json!(0);
        let codex = probe_from(&body, "one", at);
        let lines = card(&codex);
        assert_eq!(lines[0], " one — Codex  [plus]  ok");
        assert!(lines[1].starts_with("   5h "), "{}", lines[1]);
        assert!(lines[1].contains(" 70% left"), "{}", lines[1]);
        assert!(lines[2].starts_with("   weekly "), "{}", lines[2]);
        assert!(lines[3].starts_with("   codex_spark "), "{}", lines[3]);
        assert!(lines[3].ends_with("limited"), "{}", lines[3]);
        assert_eq!(lines[4], "   a@b.c");

        // A weekly-only account has one gauge, named by its length.
        let weekly = card(&probe_from(&weekly_only_body(42.0), "one", at));
        assert_eq!(weekly[0], " one — Codex  [promax]  ok");
        assert!(weekly[1].starts_with("   weekly "), "{}", weekly[1]);
        assert!(weekly[1].contains(" 58% left"), "{}", weekly[1]);
        assert_eq!(
            weekly[2..],
            ["   a@b.c", "   2 reset credits (0 usable now)"]
        );

        // An account never read, and one whose read failed.
        let unread = AccountView::of(Service::Kimi, "kimi", None);
        assert_eq!(
            plain(account_lines(&unread, None, None)),
            [" kimi — Kimi Code", "   not read yet"]
        );
        let failed = crate::providers::failed(Service::Zai, "glm", 0, "bad key");
        assert_eq!(
            card(&failed),
            [" glm — GLM (z.ai)  error", "   error: bad key"]
        );
    }

    #[test]
    fn a_codex_reading_on_one_line_names_its_windows_credits_and_trouble() {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let mut body = weekly_only_body(42.0);
        assert_eq!(
            summary(&probe_from(&body, "one", at)),
            "weekly 58% · 2 reset credits (0 usable now)"
        );
        body["rate_limit_reset_credits"] = json!({"available_count": 0});
        body["rate_limit"]["allowed"] = json!(false);
        body["rate_limit"]["limit_reached"] = json!(true);
        let limited = summary(&probe_from(&body, "one", at));
        assert!(limited.starts_with("weekly 58% · limited"), "{limited}");
        let refused = Reading::failed(
            Service::Codex,
            "one",
            0,
            crate::quota::ReadError::Http {
                status: 401,
                body: "expired".into(),
            },
        );
        assert_eq!(
            summary(&refused),
            "error: HTTP 401: expired; needs `tokeman codex refresh`"
        );
        assert_eq!(badge(&refused).unwrap().0, "error");
    }

    #[test]
    fn an_api_error_body_is_shown_as_its_message() {
        assert_eq!(
            brief_error(
                r#"HTTP 403: {"type":"error","error":{"type":"permission_error","message":"OAuth authentication is currently not allowed for this organization."},"request_id":"req_1"}"#
            ),
            "HTTP 403: OAuth authentication is currently not allowed for this organization."
        );
        assert_eq!(brief_error("network: timed out"), "network: timed out");
        assert_eq!(brief_error("HTTP 502: {not json"), "HTTP 502: {not json");
    }

    #[test]
    fn what_needs_the_user_is_a_sentence_and_the_command_that_deals_with_it() {
        use crate::attention::Within;
        const NOW: i64 = 1_794_000_000;
        let say = |item: &Attention| (attention_text(item, NOW), attention_fix(item));
        let (text, fix) = say(&Attention::LoginEnds {
            account: "first@example.com".into(),
            ends_at: NOW + 5 * 3_600 + 240,
            within: Within::OneDay,
        });
        assert!(
            text.starts_with("the login of first@example.com ends in 5h4m ("),
            "{text}"
        );
        assert_eq!(
            fix.as_deref(),
            Some("tokeman claude login first@example.com")
        );
        let (text, _) = say(&Attention::LoginEnds {
            account: "a".into(),
            ends_at: NOW - 7_200,
            within: Within::OneDay,
        });
        assert_eq!(text, "the login of a ended 2h ago");
        let (text, fix) = say(&Attention::ResetExpires {
            service: Service::Codex,
            account: "work".into(),
            expires_at: NOW + 2 * 86_400 + 60,
            within: Within::ThreeDays,
        });
        assert!(
            text.starts_with("a reset on offer to work (Codex) expires ")
                && text.ends_with("(in 2d)"),
            "{text}"
        );
        assert_eq!(fix.as_deref(), Some("tokeman codex reset work"));
        assert_eq!(
            say(&Attention::BelowFloor {
                service: Service::Claude
            }),
            ("Claude: every account is below its floor".to_owned(), None)
        );
        assert_eq!(
            say(&Attention::BelowFloor {
                service: Service::Kimi
            })
            .0,
            "Kimi Code: no account has room"
        );
        let (text, fix) = say(&Attention::RunsDry {
            service: Service::Claude,
            at: NOW + 1_800,
        });
        assert_eq!(text, "Claude runs dry in 30m at the last 6 h rate");
        assert_eq!(fix, None);
        assert_eq!(
            say(&Attention::DaemonStopped).1.as_deref(),
            Some("tokeman rotate install")
        );
        assert_eq!(format_until(NOW, NOW), "now");
        assert_eq!(format_until(NOW + 20, NOW), "in 1m");
    }

    #[test]
    fn a_forecast_says_its_outlook_its_basis_and_what_resets_would_add() {
        const NOW: i64 = 1_794_000_000;
        let forecast = |outlook, with_resets| Forecast {
            service: Service::Claude,
            outlook,
            burn_hours: 6,
            rate: 0.041,
            room: 2.12,
            span: Span::Weekly,
            accounts: 7,
            measured: 7,
            refills: 2,
            with_resets,
        };
        let dry = forecast(
            Outlook::Dry { at: NOW + 1_800 },
            Some(Outlook::Dry { at: NOW + 3_000 }),
        );
        assert_eq!(
            forecast_text(&dry, NOW),
            "Claude runs dry in 30m at the last 6 h rate (212% of a weekly window left across \
             7 accounts, used at 4.1%/h); with the 2 resets on offer, in 50m"
        );
        assert_eq!(forecast_brief(&dry, NOW), "Claude dry in 30m");
        let carried = forecast(Outlook::Dry { at: NOW }, Some(Outlook::Beyond));
        let text = forecast_text(&carried, NOW);
        assert!(
            text.starts_with("Claude is dry now (")
                && text.ends_with("; with the 2 resets on offer, not within 7 days"),
            "{text}"
        );
        assert_eq!(forecast_brief(&carried, NOW), "Claude dry now");
        let beyond = Forecast {
            measured: 3,
            ..forecast(Outlook::Beyond, None)
        };
        assert_eq!(
            forecast_text(&beyond, NOW),
            "Claude lasts more than 7 days at the last 6 h rate (212% of a weekly window left \
             across 7 accounts, used at 4.1%/h, measured on 3)"
        );
        assert_eq!(forecast_brief(&beyond, NOW), "Claude 7d+");
        let idle = forecast(Outlook::Idle, None);
        assert_eq!(
            forecast_text(&idle, NOW),
            "Claude was not used in the last 6 h: no forecast (212% of a weekly window left \
             across 7 accounts)"
        );
        assert_eq!(forecast_brief(&idle, NOW), "Claude idle");
    }

    #[test]
    fn the_reports_open_with_what_needs_attention_and_the_forecasts() {
        use crate::account::Fleet;
        use crate::ui::overview::Facts;
        const NOW: i64 = 1_794_000_000;
        let mut refused = crate::testing::login_account("first@example.com", NOW * 1000);
        refused.login.as_mut().unwrap().error = Some("invalid_grant".into());
        let fleet = Fleet {
            accounts: vec![refused],
            ..Fleet::default()
        };
        let recent: Vec<Reading> = (0..=6)
            .map(|hour| {
                let mut reading = crate::testing::reading("first@example.com", 0.0, 0.0);
                reading.at = NOW - (6 - hour) * 3_600;
                reading.set_weekly(Some(Window::new(
                    0.10 + 0.001 * hour as f64,
                    NOW + 6 * 86_400,
                )));
                reading
            })
            .collect();
        let facts = Facts {
            readings: vec![recent.last().unwrap().clone()],
            recent,
            ..Facts::default()
        };
        let lines = plain(headlines(&Overview::build(&fleet, facts, None, NOW)));
        assert_eq!(lines[0], " needs attention");
        assert_eq!(
            lines[1],
            "  ! the login of first@example.com was refused  `tokeman claude login first@example.com`"
        );
        assert!(
            lines[2].starts_with(" forecast: Claude lasts more than 7 days at the last 6 h rate"),
            "{lines:?}"
        );
        assert_eq!(lines[3], "");
        assert_eq!(lines.len(), 4);
        // Nothing to say is no section at all.
        let quiet = Overview::build(&Fleet::default(), Facts::default(), None, NOW);
        assert!(headlines(&quiet).is_empty());
    }

    #[test]
    fn a_failed_credit_list_is_said_under_the_account() {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let listed = Listed {
            probe: probe_from(&weekly_only_body(5.0), "holder", at),
            reset_credit_list: None,
            reset_credit_error: Some(crate::quota::ReadError::Http {
                status: 500,
                body: "oops".into(),
            }),
        };
        assert_eq!(
            credit_lines(&listed),
            [
                "   reset credit details unavailable (HTTP 500: oops); the count above is from the usage body"
            ]
        );
    }

    #[test]
    fn other_windows_unavailable_models_and_spend_caps_are_said() {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let mut body = weekly_only_body(10.0);
        body["rate_limit"]["secondary_window"] =
            json!({"used_percent": 70, "limit_window_seconds": 86400});
        body["model_usage"]["gpt-7"] =
            json!({"available": false, "available_at": 7_200, "credits_would_enable": true});
        body["spend_control"] = json!({"reached": true, "individual_limit": {
            "limit": "50.00", "used": "50.00", "remaining": "0", "used_percent": 100, "reset_at": 0}});
        body["rate_limit_reset_credits"] = json!({"available_count": 1});
        let probe = probe_from(&body, "one", at);
        let lines = card(&probe);
        assert!(lines[2].starts_with("   daily "), "{}", lines[2]);
        assert!(lines[2].contains(" 30% left"), "{}", lines[2]);
        assert_eq!(
            notes(&probe),
            [
                "a@b.c",
                "1 reset credit",
                "model gpt-7 unavailable until 1970-01-01 02:00Z, credits would enable it",
                "spend control reached, used 50.00 of 50.00 (100%)",
            ]
        );
    }

    #[test]
    fn the_caption_says_where_the_view_comes_from_and_how_old_it_is() {
        use crate::account::Fleet;
        use crate::ui::overview::Facts;
        let fleet = Fleet {
            accounts: vec![crate::testing::setup_account("a")],
            ..Fleet::default()
        };
        let mut reading = crate::testing::reading("a", 0.1, 0.2);
        reading.at = 1_000;
        let view = |monitor, now| {
            let facts = Facts {
                monitor,
                readings: vec![reading.clone()],
                ..Facts::default()
            };
            Overview::build(&fleet, facts, None, now)
        };
        assert_eq!(
            view_caption(&view(Monitor::Running, 1_125)),
            "stored view, last read 2m ago by the background service"
        );
        assert_eq!(
            view_caption(&view(Monitor::Running, 1_000 + 7_200)),
            "stored view, STALE: the background service last read 2h ago"
        );
        assert_eq!(
            view_caption(&view(Monitor::Paused, 1_030)),
            "stored view, last read just now; rotation is paused"
        );
        let fresh = Overview::build(
            &fleet,
            Facts::default().with_fresh(vec![reading.clone()]),
            None,
            1_000,
        );
        assert_eq!(view_caption(&fresh), "read now, 1/1 accounts answered");
        let empty = Overview::build(&fleet, Facts::default(), None, 1_000);
        assert_eq!(view_caption(&empty), "stored view, nothing read yet");
    }

    fn probe_from(body: &serde_json::Value, name: &str, at: DateTime<Utc>) -> Reading {
        crate::codex::probe::decode_usage(name, &body.to_string(), at).unwrap()
    }
}

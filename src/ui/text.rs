//! Terminal text: gauges, reset times, labels, and the printed reports.

use chrono::{DateTime, Local, Utc};

use crate::account::Account;
use crate::anthropic::resets;
use crate::claude::sessions;
use crate::codex::probe::State;
use crate::codex::resets as codex_resets;
use crate::commands::codex::{self, Listed};
use crate::daemon::status::Report;
use crate::paths::tilde;
use crate::quota::Scope;
use crate::quota::{
    Balance, CodexDetail, Level, Limit, Reading, ResetCreditCount, Service, SessionLimit, Span,
    Verdict, Window,
};
use crate::store::Stats;
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

/// The first 16 and last 4 characters of a key, or `****` for a short one.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<_> = key.chars().collect();
    if chars.len() > 20 {
        format!(
            "{}...{}",
            chars[..16].iter().collect::<String>(),
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    } else {
        "****".into()
    }
}

/// `Opus 4.8` -> `O48`, `Fable` -> `Fab`: three characters for a column.
pub fn short_bucket_label(label: &str) -> String {
    let mut words = label.split_whitespace();
    let name = words.next().unwrap_or("?");
    let version: String = words
        .flat_map(str::chars)
        .filter(char::is_ascii_digit)
        .collect();
    let short = if version.is_empty() {
        name.chars().take(3).collect::<String>()
    } else {
        format!("{}{version}", name.chars().next().unwrap_or('?'))
    };
    short.chars().take(3).collect()
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
    let remaining = 1.0 - used;
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

/// A word for the reading's state and the level to colour it with.
pub fn status_badge(reading: &Reading) -> (&'static str, Option<Level>) {
    if reading.session().is_none() && reading.weekly().is_none() {
        return if reading.error.is_some() {
            ("error", Some(Level::Critical))
        } else {
            ("no quota", None)
        };
    }
    match reading.verdict() {
        Verdict::Allowed => ("allowed", Some(Level::Ok)),
        Verdict::AllowedWarning => ("warning", Some(Level::Low)),
        Verdict::Rejected => ("REJECTED", Some(Level::Critical)),
        Verdict::Unknown => ("unknown", Some(Level::Low)),
    }
}

/// What an account's login is good for, for `tokeman list`.
pub fn describe_login(account: &Account, now_ms: i64) -> String {
    let Some(login) = account.login.as_ref() else {
        return "no login (inference-only)".into();
    };
    if let Some(error) = &login.error {
        return format!(
            "login refused ({error}); run `tokeman claude login {}`",
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

/// The gauges `tokeman` prints with no subcommand.
pub fn print_readings(readings: &[Reading], probe_model: &str) {
    let total = readings.len();
    let ok = readings.iter().filter(|r| r.error.is_none()).count();
    println!("\n {BOLD}tokeman{RESET} — {ok}/{total} accounts probed with {probe_model}\n");
    for reading in readings {
        println!(" {BOLD}{}{RESET}", reading.account);
        let reported = reading.session().is_some() || reading.weekly().is_some();
        match (&reading.error, reported) {
            (Some(error), false) => println!("   {RED}error: {error}{RESET}"),
            (_, true) => print_windows(reading),
            (None, false) => println!(
                "   {YELLOW}no unified quota headers (an API key rather than OAuth?){RESET}"
            ),
        }
        print_buckets(reading);
        println!();
    }
}

fn print_windows(reading: &Reading) {
    match reading.session() {
        Some(w) => println!(
            "   Session (5h) {}  resets {}",
            bar(w.used),
            format_reset(w.resets_at)
        ),
        None if reading.session_limit() == SessionLimit::Absent => {
            println!("   Session (5h) {DIM}no 5h limit on this account{RESET}");
        }
        None => {}
    }
    if let Some(w) = reading.weekly() {
        let warning = match w.remaining() {
            r if r < 0.10 => format!("  {RED}!! critical{RESET}"),
            r if r < 0.20 => format!("  {YELLOW}! low{RESET}"),
            _ => String::new(),
        };
        println!(
            "   Weekly  (7d) {}  resets {}{warning}",
            bar(w.used),
            format_reset(w.resets_at)
        );
    }
    let claude = reading.claude();
    if let Some(w) = reading.window(Span::Unknown, &Scope::extra_usage()) {
        let disabled = claude
            .and_then(|claude| claude.overage.as_ref())
            .and_then(|overage| overage.disabled_reason.as_deref());
        let label = match disabled {
            Some("out_of_credits") => format!("Extra usage  {RED}(no credits){RESET}"),
            Some(reason) => format!("Extra usage  {YELLOW}({reason}){RESET}"),
            None => "Extra usage ".into(),
        };
        println!(
            "   {label} {}  resets {}",
            bar(w.used),
            format_reset(w.resets_at)
        );
    }
    let (word, level) = status_badge(reading);
    let color = level.map_or(DIM, |level| super::level_color(level, LEVELS));
    let claim = claude
        .and_then(|claude| claude.claim.as_deref())
        .map(|claim| claim_label(claim).map_or(claim, |(long, _)| long))
        .unwrap_or("--");
    println!("   Status: {color}{word}{RESET}  (limit: {claim})");
    for line in claude
        .map(|claude| signal_lines(&claude.signals))
        .unwrap_or_default()
    {
        println!("   {DIM}{line}{RESET}");
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

fn print_buckets(reading: &Reading) {
    if reading.models().next().is_none() {
        let reason = match reading
            .claude()
            .and_then(|claude| claude.profile_error.as_ref())
        {
            Some(_) => "profile usage credential expired or unavailable",
            None => "no per-model limits reported",
        };
        println!("   Model   (7d) --  {DIM}{reason}{RESET}");
    }
    for limit in reading.models() {
        println!(
            "   {:<12} {}  resets {}  {DIM}{}{}{RESET}",
            format!("{} (7d)", limit.label),
            bar(limit.window.used),
            format_reset(limit.window.resets_at),
            limit.source.label(),
            share_note(limit, reading.weekly().as_ref())
                .map(|note| format!("; {note}"))
                .unwrap_or_default(),
        );
    }
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

/// `5h 99% · weekly 79% · extra usage $21.96 of $50.00 left`, or the error:
/// one service account on one line, without colour.
pub fn usage_summary(usage: &Reading) -> String {
    if let Some(error) = &usage.error {
        return format!("error: {}", one_line(&error.to_string(), 100));
    }
    let mut parts: Vec<String> = usage
        .limits
        .iter()
        .map(|meter| {
            format!(
                "{} {:.0}%",
                limit_name(meter),
                (meter.window.remaining().max(0.0) * 100.0).round()
            )
        })
        .collect();
    parts.extend(usage.balances.iter().map(balance_text));
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

/// The services section of `tokeman list`: the last stored read of each
/// configured service account, without network.
///
/// `seated` names the Antigravity account the agy CLI is signed in as; it is
/// marked `*` like the Claude default.
pub fn print_service_list(
    accounts: &[crate::providers::ServiceAccount],
    latest: &[Reading],
    seated: Option<&str>,
    now: i64,
) {
    if accounts.is_empty() {
        return;
    }
    println!("\n {BOLD}services{RESET}");
    for account in accounts {
        let read = latest
            .iter()
            .find(|usage| usage.service == account.service && usage.account == account.name);
        let detail = match read {
            Some(usage) => format!(
                "{}{}  {DIM}({}){RESET}",
                usage
                    .plan
                    .as_deref()
                    .map(|plan| format!("[{plan}] "))
                    .unwrap_or_default(),
                usage_summary(usage),
                format_age(usage.at, now)
            ),
            None => format!(
                "{DIM}not read yet (`tokeman {} list`){RESET}",
                crate::commands::services::command_word(account.service)
            ),
        };
        let (marker, note) = if seated == Some(account.name.as_str()) {
            ("*", format!("  {GREEN}agy's login{RESET}"))
        } else {
            (" ", String::new())
        };
        println!(
            "{marker} {:<24} {:<12} {detail}{note}",
            account.name,
            account.service.label()
        );
    }
}

/// `5H 90% · WEEKLY 58% · 2 reset credits (0 usable now)`, with the state
/// when it is not `ok`: one stored Codex probe on one line.
pub fn codex_summary(reading: &crate::ui::overview::CodexReading) -> String {
    let mut parts: Vec<String> = reading
        .windows
        .iter()
        .map(|named| {
            format!(
                "{} {:.0}%",
                named.label,
                (named.window.remaining().clamp(0.0, 1.0) * 100.0).round()
            )
        })
        .collect();
    if let Some(credits) = reading
        .reset_credits
        .filter(|credits| credits.available > 0)
    {
        let plural = if credits.available == 1 { "" } else { "s" };
        parts.push(match credits.applicable {
            Some(now) => format!(
                "{} reset credit{plural} ({now} usable now)",
                credits.available
            ),
            None => format!("{} reset credit{plural}", credits.available),
        });
    }
    if reading.state != "ok" {
        parts.push(one_line(&reading.state, 80));
    }
    parts.join(" · ")
}

/// The Codex section of `tokeman list`: the last stored probe of each profile.
pub fn print_codex_list(accounts: &[crate::ui::overview::CodexView], now: i64) {
    if accounts.is_empty() {
        return;
    }
    println!("\n {BOLD}codex{RESET}");
    for account in accounts {
        let detail = match &account.reading {
            Some(reading) => format!(
                "{}{}  {DIM}({}){RESET}",
                reading
                    .plan
                    .as_deref()
                    .map(|plan| format!("[{plan}] "))
                    .unwrap_or_default(),
                codex_summary(reading),
                format_age(reading.at, now)
            ),
            None => format!("{DIM}not read yet (`tokeman codex list`){RESET}"),
        };
        println!(
            "  {:<24} {:<12} {detail}",
            account.name,
            if account.parked { "parked" } else { "" }
        );
    }
}

/// Service accounts with their gauges. `seated` names the Antigravity
/// account the agy CLI is signed in as.
fn print_gauges(usages: &[Reading], seated: Option<&str>) {
    println!();
    for usage in usages {
        let plan = usage
            .plan
            .as_deref()
            .map(|plan| format!("  {DIM}[{plan}]{RESET}"))
            .unwrap_or_default();
        let note = if seated == Some(usage.account.as_str()) {
            format!("  {GREEN}* agy's login{RESET}")
        } else {
            String::new()
        };
        println!(
            " {BOLD}{}{RESET} — {}{plan}{note}",
            usage.account,
            usage.service.label()
        );
        if let Some(error) = &usage.error {
            println!("   {RED}error: {error}{RESET}");
        }
        let width = usage
            .limits
            .iter()
            .map(|meter| limit_name(meter).chars().count())
            .max()
            .unwrap_or(0);
        for meter in &usage.limits {
            let amount = meter
                .amount
                .as_ref()
                .map(|amount| {
                    format!(
                        "  {DIM}{:.0}/{:.0} {}{RESET}",
                        amount.used, amount.limit, amount.unit
                    )
                })
                .unwrap_or_default();
            println!(
                "   {:<width$} {}  resets {}{amount}",
                limit_name(meter),
                bar(meter.window.used.clamp(0.0, 1.0)),
                format_reset(meter.window.resets_at),
            );
        }
        for balance in &usage.balances {
            println!("   {}", balance_text(balance));
        }
        println!();
    }
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
            usage_summary(reading),
        );
    }
    println!();
}

/// `tokeman stats`: how fast each limit of each account is being used.
pub fn print_stats(stats: &[Stats]) {
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
}

pub fn print_status(report: &Report) {
    println!(
        "monitor: {}{}",
        report.monitor,
        if report.service_installed {
            " (service installed)"
        } else {
            ""
        }
    );
    println!(
        "mode: {} ({}s sampling)",
        report.mode.label(),
        report.probe_interval_secs
    );
    println!(
        "quota probe: {} (capacity signal; large-model admission can be stricter)",
        crate::anthropic::probe::PROBE_MODEL_LABEL
    );
    println!("premium admission: {}", report.coverage);
    println!("header-only limits: {}", report.sampling);
    for offer in &report.resets_on_offer {
        println!("reset on offer: {offer} (`tokeman claude reset`)");
    }
    println!(
        "default for newly started Claude processes: {}",
        report.default_account.as_deref().unwrap_or("/login")
    );
    println!(
        "selection model: {} (only matching model-scoped buckets affect rotation)",
        report.target_model.as_deref().unwrap_or("unknown")
    );
    let live = report.live_sessions.as_deref();
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
        "default thresholds: change at 5h <= {:.0}% left or 7d <= {:.0}% left",
        report.min_session_remaining * 100.0,
        report.min_weekly_remaining * 100.0
    );
    for account in &report.accounts {
        let marker = if account.is_default { "D" } else { " " };
        if account.remaining_session.is_some() || account.remaining_weekly.is_some() {
            let cell = |left: Option<f64>| {
                left.map_or("  --".to_string(), |left| format!("{:>3.0}%", left * 100.0))
            };
            println!(
                " {marker} {:<24} 5h {}  7d {}  {}",
                account.name,
                cell(account.remaining_session),
                cell(account.remaining_weekly),
                if account.viable {
                    "ready"
                } else {
                    "below floor"
                }
            );
        } else {
            println!(
                " {marker} {:<24} {}",
                account.name,
                account.error.as_deref().unwrap_or("no quota headers")
            );
        }
        println!("   {:<24} offers {}", "", account.credential);
        if account.buckets.is_empty() {
            println!("   {:<24} model-scoped 7d: unavailable", "");
        }
        for bucket in &account.buckets {
            println!(
                "   {:<24} {}{} {:>3.0}%  resets {}  {}",
                "",
                if bucket.relevant { "*" } else { " " },
                bucket.label,
                bucket.remaining * 100.0,
                format_reset_compact(bucket.resets_at),
                bucket.source.short_label(),
            );
        }
        for limit in &account.limits {
            println!("   {:<24} ! {}", "", limit.summary());
        }
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

/// Fresh reads of any mix of services: Claude's gauges, the Codex table,
/// then each other service's gauges.
pub fn print_everything(readings: &[Reading]) {
    let of = |wanted: fn(Service) -> bool| -> Vec<Reading> {
        readings
            .iter()
            .filter(|reading| wanted(reading.service))
            .cloned()
            .collect()
    };
    let claude = of(|service| service == Service::Claude);
    if !claude.is_empty() {
        print_readings(&claude, crate::anthropic::probe::PROBE_MODEL_LABEL);
    }
    let codex = of(|service| service == Service::Codex);
    if !codex.is_empty() {
        println!(" {BOLD}codex{RESET}\n");
        println!("{ROW_HEADER}");
        let now = chrono::Utc::now().timestamp();
        for reading in &codex {
            for row in rows(reading, now) {
                println!("{row}");
            }
        }
        println!();
    }
    let others = of(|service| !matches!(service, Service::Claude | Service::Codex));
    if !others.is_empty() {
        print_gauges(&others, None);
    }
}

/// `tokeman list`: every account of every service, from stored reads.
pub fn print_stored(
    fleet: &crate::account::Fleet,
    overview: &crate::ui::overview::Overview,
    seated: Option<&str>,
) {
    if fleet.accounts.is_empty() && fleet.services.is_empty() && fleet.codex.is_empty() {
        println!("{NO_ACCOUNTS}");
    }
    let now_ms = overview.at * 1000;
    for account in &fleet.accounts {
        let marker = if overview.default_account.as_deref() == Some(account.name.as_str()) {
            "*"
        } else {
            " "
        };
        let setup = account
            .setup_key
            .as_deref()
            .map(mask_key)
            .unwrap_or_else(|| "no setup key".into());
        println!(
            "{marker} {:<24} {setup}  {}",
            account.name,
            describe_login(account, now_ms)
        );
    }
    print_codex_list(&overview.codex, overview.at);
    let latest: Vec<_> = overview
        .services
        .iter()
        .filter_map(|service| service.usage.clone())
        .collect();
    print_service_list(&fleet.services, &latest, seated, overview.at);
    for offer in &overview.resets_on_offer {
        println!(
            "\n reset on offer: {}: {} (`tokeman claude reset {}`; read {})",
            offer.account,
            offer.offer,
            offer.account,
            format_age(offer.read_at, overview.at)
        );
    }
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

pub fn print_reset_cleared(name: &str, pending: Option<&crate::claims::Pending>) {
    match pending {
        Some(pending) => println!(
            "{name}: forgot the claim of grant {} (request {})",
            pending.item.as_deref().unwrap_or("?"),
            pending.request_id
        ),
        None => println!("{name}: no claim pending"),
    }
}

// ---- Codex ----

/// The header of the Codex table.
pub const ROW_HEADER: &str = "ACCOUNT      EMAIL                      PLAN    5H               WEEKLY           RESETS     STATE";

/// `45% (2h10m)`, or `-` for no window.
pub fn window_cell(window: Option<Window>, now: i64) -> String {
    let Some(window) = window else {
        return "-".to_owned();
    };
    let used = format!("{:.0}%", window.used * 100.0);
    let left = window.resets_at - now;
    if window.resets_at > 0 && left > 0 {
        format!("{used} ({})", duration_cell(left))
    } else {
        used
    }
}

pub fn duration_cell(seconds: i64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// Reset credits held, and how many would clear a limit now: `2 (0 now)`.
pub fn resets_cell(count: Option<ResetCreditCount>) -> String {
    match count {
        None => "-".to_owned(),
        Some(ResetCreditCount {
            available,
            applicable: Some(applicable),
        }) => format!("{available} ({applicable} now)"),
        Some(ResetCreditCount { available, .. }) => available.to_string(),
    }
}

/// Every window of a scoped limit, each named: `weekly 40% (3d2h)  5h 10%`.
fn windows_cell(limits: &[&Limit], now: i64) -> String {
    limits
        .iter()
        .map(|limit| format!("{} {}", limit.span, window_cell(Some(limit.window), now)))
        .collect::<Vec<_>>()
        .join("  ")
}

/// A probe as one table row under [`ROW_HEADER`], then one indented line
/// for each window of another length, per-model bucket, unavailable model
/// and spending cap.
pub fn rows(probe: &Reading, now: i64) -> Vec<String> {
    let detail = CodexDetail::default();
    let codex = probe.codex().unwrap_or(&detail);
    let mut lines = vec![format!(
        "{:<12} {:<26} {:<7} {:<16} {:<16} {:<10} {}",
        probe.account,
        codex.email.as_deref().unwrap_or("-"),
        probe.plan.as_deref().unwrap_or("-"),
        window_cell(probe.session(), now),
        window_cell(probe.weekly(), now),
        resets_cell(codex.reset_credits),
        State::of(probe),
    )];
    for limit in probe
        .shared()
        .filter(|limit| !matches!(limit.span, Span::FiveHour | Span::Weekly))
    {
        lines.push(format!(
            "  └ {:<24} {}",
            format!("{} window", limit.span),
            window_cell(Some(limit.window), now),
        ));
    }
    for (_, limits) in probe.scoped() {
        lines.push(format!(
            "  └ {:<24} {}  {}",
            limits[0].label,
            windows_cell(&limits, now),
            if limits.iter().any(|limit| limit.is_rejected()) {
                "limited"
            } else {
                "ok"
            },
        ));
    }
    for model in codex.models.iter().filter(|model| model.is_unavailable()) {
        let mut line = format!("  └ model {} unavailable", model.slug);
        if let Some(at) = model.available_at {
            line.push_str(&format!(" until {}", crate::commands::codex::fmt_epoch(at)));
        }
        if model.credits_would_enable == Some(true) {
            line.push_str(", credits would enable it");
        }
        lines.push(line);
    }
    if let Some(spend) = &codex.spend_control
        && (spend.reached == Some(true) || spend.limit.is_some())
    {
        let mut line = format!(
            "  └ spend control {}",
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
            if let Some(at) = cap.reset_at.filter(|at| *at > now) {
                line.push_str(&format!(", resets in {}", duration_cell(at - now)));
            }
        }
        lines.push(line);
    }
    lines
}

/// [`rows`], then one line per reset credit, or why the list is missing.
pub fn listed_rows(listed: &Listed, now: i64) -> Vec<String> {
    let mut lines = rows(&listed.probe, now);
    if let Some(list) = &listed.reset_credit_list {
        lines.extend(
            list.credits
                .iter()
                .map(|credit| format!("  └ reset credit {}", codex_resets::describe(credit))),
        );
    }
    if let Some(error) = &listed.reset_credit_error {
        lines.push(format!(
            "  └ reset credit details unavailable ({error}); counts above are from the usage body"
        ));
    }
    lines
}

pub fn print_codex_added(account: &crate::account::CodexAccount) {
    println!("registered {} at {}", account.name, account.codex_home);
}

pub fn print_no_codex() {
    println!("no codex accounts configured — try `tokeman codex discover --adopt`");
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

/// `tokeman codex list`: the table, with each account's reset credits.
pub fn print_codex_listed(listed: &[codex::Listed]) {
    let now = chrono::Utc::now().timestamp();
    println!("{ROW_HEADER}");
    for entry in listed {
        for row in listed_rows(entry, now) {
            println!("{row}");
        }
    }
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
    let now = chrono::Utc::now().timestamp();
    println!("{ROW_HEADER}");
    for row in rows(&plan.probe, now) {
        println!("{row}");
    }
    match &plan.credits {
        Ok(list) => {
            for credit in &list.credits {
                println!("  └ reset credit {}", codex_resets::describe(credit));
            }
        }
        Err(error) => println!("  └ reset credit details unavailable ({error})"),
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
             which the backend redeems at most once"
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
    print_gauges(std::slice::from_ref(reading), None);
}

pub fn print_agy_imported(imported: &crate::commands::services::Imported) {
    if imported.registered.is_empty() {
        println!("no signed-in accounts found");
        return;
    }
    for registered in &imported.registered {
        println!("{}", registration_line(registered));
    }
    print_gauges(&imported.readings, imported.seated.as_deref());
}

pub fn print_agy_use(report: &crate::providers::antigravity::seat::UseReport) {
    for line in &report.lines() {
        println!("{line}");
    }
}

/// `tokeman <service> list`: the service's accounts with their gauges.
pub fn print_usages(service: Service, usages: &[Reading], seated: Option<&str>) {
    if usages.is_empty() {
        println!(
            "No {} accounts configured. Use `{}`.",
            service.label(),
            match service {
                Service::Antigravity => "tokeman agy import".to_owned(),
                other => format!(
                    "tokeman {} add <name> --key-file <path>",
                    crate::commands::services::command_word(other)
                ),
            }
        );
        return;
    }
    print_gauges(usages, seated);
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
    let note = match best.service {
        Service::Claude => "the default",
        _ => "agy's login",
    };
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
            usage_summary(reading)
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
    fn short_labels_keep_family_and_version() {
        assert_eq!(short_bucket_label("Opus 4.8"), "O48");
        assert_eq!(short_bucket_label("Opus 5"), "O5");
        assert_eq!(short_bucket_label("Fable"), "Fab");
        assert_eq!(short_bucket_label("Sonnet"), "Son");
    }

    #[test]
    fn text_helpers_count_characters_not_bytes() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("💥💥💥", 2), "💥💥…");
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert!(mask_key("abcdefghijklmnop💥qrstuv").starts_with("abcdefghijklmnop..."));
        assert_eq!(mask_key("short"), "****");
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
            usage_summary(&usage),
            "5h 99% · weekly 0% · extra usage $21.96 of $50.00 left"
        );
        let failed = failed(Service::Zai, "glm", 0, "z.ai refused:\n bad key");
        assert_eq!(usage_summary(&failed), "error: z.ai refused: bad key");
        let grouped = Reading {
            limits: vec![meter(Some("Gemini Models"), "5h", 0.5)],
            ..Reading::empty(Service::Antigravity, "a", 0)
        };
        assert_eq!(usage_summary(&grouped), "Gemini Models 5h 50%");
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
    fn a_codex_probe_reads_as_named_windows_credits_and_trouble() {
        use crate::quota::{ResetCreditCount, Window};
        use crate::ui::overview::{CodexReading, NamedWindow};
        let mut reading = CodexReading {
            at: 0,
            email: None,
            plan: Some("pro".into()),
            state: "ok".into(),
            viable: true,
            windows: vec![
                NamedWindow {
                    label: "5H".into(),
                    window: Window::new(0.1, 0),
                },
                NamedWindow {
                    label: "WEEKLY".into(),
                    window: Window::new(0.42, 0),
                },
            ],
            extra: Vec::new(),
            reset_credits: Some(ResetCreditCount {
                available: 2,
                applicable: Some(0),
            }),
        };
        assert_eq!(
            codex_summary(&reading),
            "5H 90% · WEEKLY 58% · 2 reset credits (0 usable now)"
        );
        reading.reset_credits = Some(ResetCreditCount {
            available: 0,
            applicable: None,
        });
        reading.state = "limited (weekly)".into();
        assert_eq!(
            codex_summary(&reading),
            "5H 90% · WEEKLY 58% · limited (weekly)"
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

    #[test]
    fn cells_format_usage_and_time_left() {
        assert_eq!(window_cell(None, 0), "-");
        assert_eq!(window_cell(Some(Window::new(0.456, 0)), 0), "46%");
        assert_eq!(
            window_cell(Some(Window::new(0.1, 1_000 + 7_800)), 1_000),
            "10% (2h10m)"
        );
        assert_eq!(window_cell(Some(Window::new(0.1, 500)), 1_000), "10%");
        assert_eq!(duration_cell(90_000), "1d1h");
        assert_eq!(duration_cell(120), "2m");
    }

    #[test]
    fn rows_follow_the_header_and_list_extra_buckets() {
        let at = DateTime::from_timestamp(1_000, 0).unwrap();
        let mut body = usage_body(30.0, 60.0);
        body["rate_limit"]["primary_window"]["reset_at"] = json!(1_000 + 3_600);
        let probe = probe_from(&body, "one", at);
        let lines = rows(&probe, 1_000);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with(
                "one          a@b.c                      plus    30% (1h0m)       60%"
            ),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with(" -          ok"), "{}", lines[0]);
        assert!(lines[1].starts_with("  └ codex_spark "), "{}", lines[1]);
        assert!(lines[1].contains("window 100%"), "{}", lines[1]);
        assert!(lines[1].ends_with("limited"));
        assert_eq!(
            ROW_HEADER,
            format!(
                "{:<12} {:<26} {:<7} {:<16} {:<16} {:<10} STATE",
                "ACCOUNT", "EMAIL", "PLAN", "5H", "WEEKLY", "RESETS"
            )
        );
    }

    #[test]
    fn a_failed_credit_list_is_said_under_the_account_row() {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let listed = Listed {
            probe: probe_from(&weekly_only_body(5.0), "holder", at),
            reset_credit_list: None,
            reset_credit_error: Some(crate::quota::ReadError::Http {
                status: 500,
                body: "oops".into(),
            }),
        };
        let lines = listed_rows(&listed, 0);
        assert!(lines[0].contains("2 (0 now)"), "{}", lines[0]);
        assert_eq!(
            lines.last().unwrap(),
            "  └ reset credit details unavailable (HTTP 500: oops); counts above are from the usage body"
        );
    }

    #[test]
    fn a_weekly_only_account_fills_the_weekly_column_not_the_first() {
        let at = DateTime::from_timestamp(1_000, 0).unwrap();
        let mut body = weekly_only_body(42.0);
        body["rate_limit"]["primary_window"]["reset_at"] = json!(1_000 + 2 * 86_400 + 3_600);
        let probe = probe_from(&body, "one", at);
        let lines = rows(&probe, 1_000);
        assert_eq!(
            lines,
            vec![format!(
                "{:<12} {:<26} {:<7} {:<16} {:<16} {:<10} ok",
                "one", "a@b.c", "promax", "-", "42% (2d1h)", "2 (0 now)"
            )]
        );
    }

    #[test]
    fn other_windows_unavailable_models_and_spend_caps_get_their_own_lines() {
        let at = DateTime::from_timestamp(0, 0).unwrap();
        let mut body = weekly_only_body(10.0);
        body["rate_limit"]["secondary_window"] =
            json!({"used_percent": 70, "limit_window_seconds": 86400});
        body["model_usage"]["gpt-7"] =
            json!({"available": false, "available_at": 7_200, "credits_would_enable": true});
        body["spend_control"] = json!({"reached": true, "individual_limit": {
            "limit": "50.00", "used": "50.00", "remaining": "0", "used_percent": 100, "reset_at": 3_600}});
        body["rate_limit_reset_credits"] = json!({"available_count": 1});
        let probe = probe_from(&body, "one", at);
        let lines = rows(&probe, 0);
        assert!(lines[0].contains(" 1          ok"), "{}", lines[0]);
        assert_eq!(
            &lines[1..],
            &[
                format!("  └ {:<24} 70%", "daily window"),
                "  └ model gpt-7 unavailable until 1970-01-01 02:00Z, credits would enable it"
                    .to_owned(),
                "  └ spend control reached, used 50.00 of 50.00 (100%), resets in 1h0m".to_owned(),
            ]
        );
        assert_eq!(resets_cell(None), "-");
    }

    fn probe_from(body: &serde_json::Value, name: &str, at: DateTime<Utc>) -> Reading {
        crate::codex::probe::decode_usage(name, &body.to_string(), at).unwrap()
    }
}

//! Terminal text: gauges, reset times, labels, and the printed reports.

use chrono::{DateTime, Local, Utc};

use crate::account::Account;
use crate::claude::sessions;
use crate::daemon::status::Report;
use crate::providers::{Balance, Meter, Usage};
use crate::quota::{Bucket, Level, Reading, SessionLimit, Verdict};
use crate::store::{Snapshot, Stats};

const BAR_WIDTH: usize = 40;
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[90m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// Collapse whitespace and cap at `max_chars`, marking a cut with `…`.
pub fn one_line(text: &str, max_chars: usize) -> String {
    let single = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= max_chars {
        single
    } else {
        let cut: String = single.chars().take(max_chars).collect();
        format!("{cut}…")
    }
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

fn level_color(level: Level) -> &'static str {
    match level {
        Level::Ok => GREEN,
        Level::Low => YELLOW,
        Level::Critical => RED,
    }
}

/// A forty-cell bar of the fraction left, coloured by level.
pub fn bar(used: f64) -> String {
    let remaining = 1.0 - used;
    let filled = ((remaining * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
    let empty = BAR_WIDTH - filled;
    format!(
        "{}{}{RESET}{} {:>3}% left",
        level_color(Level::of(remaining)),
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
        (remaining * 100.0).round() as u8,
    )
}

/// A word for the reading's state and the level to colour it with.
pub fn status_badge(reading: &Reading) -> (&'static str, Option<Level>) {
    if reading.session.is_none() && reading.weekly.is_none() {
        return if reading.error.is_some() {
            ("error", Some(Level::Critical))
        } else {
            ("no quota", None)
        };
    }
    match reading.verdict {
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
            "login refused ({error}); run `tokeman login {}`",
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
            "login refused: run `tokeman login {}`",
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
    let renew = format!("run `tokeman login {}`", account.name);
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
        let reported = reading.session.is_some() || reading.weekly.is_some();
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
    match reading.session {
        Some(w) => println!(
            "   Session (5h) {}  resets {}",
            bar(w.used),
            format_reset(w.resets_at)
        ),
        None if reading.session_limit == SessionLimit::Absent => {
            println!("   Session (5h) {DIM}no 5h limit on this account{RESET}");
        }
        None => {}
    }
    if let Some(w) = reading.weekly {
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
    if let Some(overage) = &reading.overage
        && let Some(w) = overage.window
    {
        let label = match overage.disabled_reason.as_deref() {
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
    let color = level.map_or(DIM, level_color);
    let claim = reading
        .claim
        .as_deref()
        .map(|claim| claim_label(claim).map_or(claim, |(long, _)| long))
        .unwrap_or("--");
    println!("   Status: {color}{word}{RESET}  (limit: {claim})");
    for line in signal_lines(&reading.signals) {
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
    if reading.buckets.is_empty() {
        let reason = match &reading.profile_error {
            Some(_) => "profile usage credential expired or unavailable",
            None => "no per-model limits reported",
        };
        println!("   Model   (7d) --  {DIM}{reason}{RESET}");
    }
    for bucket in &reading.buckets {
        println!(
            "   {:<12} {}  resets {}  {DIM}{}{}{RESET}",
            format!("{} (7d)", bucket.label),
            bar(bucket.window.used),
            format_reset(bucket.window.resets_at),
            bucket.source.label(),
            share_note(bucket, reading),
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

fn meter_name(meter: &Meter) -> String {
    match &meter.group {
        Some(group) => format!("{group} {}", meter.label),
        None => meter.label.clone(),
    }
}

/// `5h 99% · weekly 79% · extra usage $21.96 of $50.00 left`, or the error:
/// one service account on one line, without colour.
pub fn usage_summary(usage: &Usage) -> String {
    if let Some(error) = &usage.error {
        return format!("error: {}", one_line(error, 100));
    }
    let mut parts: Vec<String> = usage
        .meters
        .iter()
        .map(|meter| {
            format!(
                "{} {:.0}%",
                meter_name(meter),
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
    latest: &[Usage],
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
            None => format!("{DIM}not read yet (`tokeman services`){RESET}"),
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

/// `tokeman services`: every service account with its gauges. `seated`
/// names the Antigravity account the agy CLI is signed in as.
pub fn print_usages(usages: &[Usage], seated: Option<&str>) {
    if usages.is_empty() {
        println!(
            "No service accounts configured. Use `tokeman services add <zai|kimi> <name> --key-file <path>`."
        );
        return;
    }
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
            .meters
            .iter()
            .map(|meter| meter_name(meter).chars().count())
            .max()
            .unwrap_or(0);
        for meter in &usage.meters {
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
                meter_name(meter),
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
fn share_note(bucket: &Bucket, reading: &Reading) -> String {
    match bucket.share {
        Some(share) => format!(
            "; {:.0}% of the weekly, {:.0}% of the weekly left for it",
            share * 100.0,
            bucket.room(reading.weekly.as_ref()).max(0.0) * 100.0
        ),
        None => String::new(),
    }
}

/// `tokeman services best`: accounts by room left, most first.
pub fn print_ranking(ranked: &[(&Usage, f64)], seated: Option<&str>) {
    for (place, (usage, room)) in ranked.iter().enumerate() {
        let note = if seated == Some(usage.account.as_str()) {
            format!("  {GREEN}* agy's login{RESET}")
        } else {
            String::new()
        };
        println!(
            " {}. {BOLD}{:<24}{RESET} {:>3.0}% left  {DIM}{}{RESET}{note}",
            place + 1,
            usage.account,
            (room.max(0.0) * 100.0).round(),
            usage_summary(usage)
        );
    }
}

pub fn print_history(snapshots: &[Snapshot]) {
    if snapshots.is_empty() {
        println!("No snapshots recorded yet.");
        return;
    }
    println!("\n {BOLD}tokeman{RESET} — {} snapshots\n", snapshots.len());
    let left = |w: Option<crate::quota::Window>| {
        w.map_or("--".to_string(), |w| {
            format!("{:.1}%", w.remaining() * 100.0)
        })
    };
    for s in snapshots {
        let overage = s
            .overage
            .map(|w| format!("  ov: {:>5.1}%", w.remaining() * 100.0))
            .unwrap_or_default();
        let models: Vec<_> = s
            .buckets
            .iter()
            .map(|b| format!("{}:{:.1}%", b.label, b.window.remaining() * 100.0))
            .collect();
        let models = if models.is_empty() {
            String::new()
        } else {
            format!("  {}", models.join(" "))
        };
        println!(
            "  {} {BOLD}{:<16}{RESET}  5h: {:>6} left  7d: {:>6} left{overage}{models}  [{} {}]",
            s.at.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
            s.account,
            left(s.session),
            left(s.weekly),
            s.status.as_deref().unwrap_or("--"),
            s.claim
                .as_deref()
                .map(|c| claim_label(c).map_or(c, |(_, short)| short))
                .unwrap_or("--"),
        );
    }
    println!();
}

pub fn print_stats(stats: &[Stats]) {
    if stats.is_empty() {
        println!("No stats available. Need at least 2 snapshots per account.");
        return;
    }
    println!("\n {BOLD}tokeman stats{RESET}\n");
    for s in stats {
        println!(" {BOLD}{}{RESET} ({} snapshots)", s.account, s.snapshots);
        if s.snapshots < 2 {
            println!("   Need at least 2 snapshots to compute rates.\n");
            continue;
        }
        let line = |label: &str, value: Option<f64>, unit: &str| {
            if let Some(value) = value {
                println!("   {label:<23}{value:.4}{unit}");
            }
        };
        line("5h burn rate (latest):", s.session_rate, "/hr");
        line("5h time to deplete:", s.session_hours_left, "h");
        line("7d burn rate (latest):", s.weekly_rate, "/hr");
        line("7d burn rate (mean):", s.weekly_rate_mean, "/hr");
        line("7d burn rate (stddev):", s.weekly_rate_stddev, "/hr");
        line("7d burn rate (peak):", s.weekly_rate_peak, "/hr");
        line("7d time to deplete:", s.weekly_hours_left, "h");
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
        println!("reset on offer: {offer} (`tokeman resets use`)");
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

#[cfg(test)]
mod tests {
    use super::*;

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
        use crate::providers::{Meter, Service};
        use crate::quota::Window;
        let meter = |label: &str, used: f64| Meter {
            group: None,
            label: label.into(),
            window: Window::new(used, 0),
            span_secs: None,
            amount: None,
        };
        let usage = Usage {
            service: Service::Kimi,
            account: "kimi".into(),
            at: 0,
            plan: Some("Vivace".into()),
            meters: vec![meter("5h", 0.0057), meter("weekly", 1.2)],
            balances: vec![Balance {
                label: "extra usage".into(),
                left_minor: 2196,
                total_minor: Some(5000),
                currency: "USD".into(),
            }],
            error: None,
        };
        assert_eq!(
            usage_summary(&usage),
            "5h 99% · weekly 0% · extra usage $21.96 of $50.00 left"
        );
        let failed = Usage::failed(Service::Zai, "glm", 0, "z.ai refused:\n bad key");
        assert_eq!(usage_summary(&failed), "error: z.ai refused: bad key");
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
            Some("login refused: run `tokeman login acct`")
        );
    }

    #[test]
    fn a_codex_probe_reads_as_named_windows_credits_and_trouble() {
        use crate::codex::probe::ResetCreditCount;
        use crate::quota::Window;
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
        assert!(signal_lines(&reading.signals).is_empty());
        reading.signals.claims = vec!["seven_day".into(), "seven_day_overage_included".into()];
        reading.signals.slow = Some(crate::quota::SlowLane {
            status: Some(crate::quota::SlowStatus::SlotBusy),
            budget: Some(crate::quota::Window::new(0.25, 0)),
            ..Default::default()
        });
        reading.signals.upgrade_paths = vec!["overage".into()];
        let lines = signal_lines(&reading.signals);
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
            crate::testing::bucket("fable", "Fable", 0.5, crate::quota::Source::Profile);
        assert_eq!(share_note(&fable, &reading), "");
        fable.share = Some(0.5);
        reading.buckets.push(fable.clone());
        assert_eq!(
            share_note(&fable, &reading),
            "; 50% of the weekly, 25% of the weekly left for it"
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
            line.contains("(in 2d): run `tokeman login acct` soon"),
            "{line}"
        );
        let line = describe_login(&account, 31 * DAY_MS);
        assert!(line.contains("login ended"), "{line}");

        account.login.as_mut().unwrap().refresh_expires_at = None;
        let line = describe_login(&account, 22 * DAY_MS);
        assert!(line.contains("logged in 22 days ago"), "{line}");
        assert!(line.contains("soon"), "{line}");
    }
}

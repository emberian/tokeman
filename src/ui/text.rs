//! Terminal text: gauges, reset times, labels, and the printed reports.

use chrono::{DateTime, Local, Utc};

use crate::account::Account;
use crate::claude::sessions;
use crate::daemon::status::Report;
use crate::quota::{Level, Reading, Verdict};
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
    if reading.windows().is_none() {
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
    match login.expires_at {
        Some(expires_at) if expires_at > now_ms => {
            format!(
                "login, full scope, access token valid {}m",
                (expires_at - now_ms) / 60_000
            )
        }
        Some(_) => "login, access token expired (refresh pending)".into(),
        None => "login, expiry unknown".into(),
    }
}

/// The gauges `tokeman` prints with no subcommand.
pub fn print_readings(readings: &[Reading], probe_model: &str) {
    let total = readings.len();
    let ok = readings.iter().filter(|r| r.error.is_none()).count();
    println!("\n {BOLD}tokeman{RESET} — {ok}/{total} accounts probed with {probe_model}\n");
    for reading in readings {
        println!(" {BOLD}{}{RESET}", reading.account);
        match (&reading.error, reading.windows()) {
            (Some(error), None) => println!("   {RED}error: {error}{RESET}"),
            (_, Some(_)) => print_windows(reading),
            (None, None) => println!(
                "   {YELLOW}no unified quota headers (an API key rather than OAuth?){RESET}"
            ),
        }
        print_buckets(reading);
        println!();
    }
}

fn print_windows(reading: &Reading) {
    if let Some(w) = reading.session {
        println!(
            "   Session (5h) {}  resets {}",
            bar(w.used),
            format_reset(w.resets_at)
        );
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
            "   {:<12} {}  resets {}  {DIM}{}{RESET}",
            format!("{} (7d)", bucket.label),
            bar(bucket.window.used),
            format_reset(bucket.window.resets_at),
            bucket.source.label(),
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
        match (account.remaining_session, account.remaining_weekly) {
            (Some(session), Some(weekly)) => println!(
                " {marker} {:<24} 5h {:>3.0}%  7d {:>3.0}%  {}",
                account.name,
                session * 100.0,
                weekly * 100.0,
                if account.viable {
                    "ready"
                } else {
                    "below floor"
                }
            ),
            _ => println!(
                " {marker} {:<24} {}",
                account.name,
                account.error.as_deref().unwrap_or("no quota headers")
            ),
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
    fn resets_in_the_past_or_absent_read_as_words() {
        assert_eq!(format_reset(0), "--");
        assert_eq!(format_reset(1), "now");
        assert_eq!(format_reset_compact(0), "--");
    }
}

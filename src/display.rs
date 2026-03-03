use chrono::{DateTime, Local, Utc};

use crate::probe::{ProbeResult, UnifiedQuota};

const BAR_WIDTH: usize = 40;

fn format_reset(ts: i64) -> String {
    if ts == 0 {
        return "--".to_string();
    }
    let reset = DateTime::from_timestamp(ts, 0).unwrap_or_default();
    let local = reset.with_timezone(&Local);
    let now = Utc::now();
    let diff = reset - now;

    if diff.num_seconds() < 0 {
        return "now".to_string();
    }

    let hours = diff.num_hours();
    let mins = diff.num_minutes() % 60;

    if hours > 24 {
        local.format("%a %-I:%M%p").to_string()
    } else if hours > 0 {
        format!("{}h{}m ({})", hours, mins, local.format("%-I:%M%p"))
    } else if mins > 0 {
        format!("{}m", mins)
    } else {
        format!("{}s", diff.num_seconds())
    }
}

fn utilization_color(remaining_frac: f64) -> &'static str {
    if remaining_frac > 0.50 {
        "\x1b[32m" // green
    } else if remaining_frac > 0.20 {
        "\x1b[33m" // yellow
    } else {
        "\x1b[31m" // red
    }
}

fn render_bar(utilization: f64) -> String {
    let remaining = 1.0 - utilization;
    let filled = (remaining * BAR_WIDTH as f64).round() as usize;
    let filled = filled.min(BAR_WIDTH);
    let empty = BAR_WIDTH - filled;
    let pct = (remaining * 100.0).round() as u8;

    let color = utilization_color(remaining);
    let reset = "\x1b[0m";

    format!(
        "{color}{}{reset}{} {:>3}% left",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
        pct,
    )
}

pub fn print_results(results: &[ProbeResult]) {
    let total = results.len();
    let ok = results.iter().filter(|r| r.error.is_none()).count();

    println!();
    println!(
        " \x1b[1mtokeman\x1b[0m — {ok}/{total} tokens probed"
    );
    println!();

    for result in results {
        println!(" \x1b[1m{}\x1b[0m", result.token_name);

        if let Some(err) = &result.error {
            if let Some(ref q) = result.quota {
                print_quota(q);
            } else {
                println!("   \x1b[31merror: {err}\x1b[0m");
            }
        } else if let Some(ref q) = result.quota {
            print_quota(q);
        } else {
            println!("   \x1b[33mno unified quota headers (might be an API key, not OAuth)\x1b[0m");
            print_rate_limits(result);
        }

        println!();
    }
}

fn print_quota(q: &UnifiedQuota) {
    if let Some(ref w) = q.session {
        println!(
            "   Session (5h) {}  resets {}",
            render_bar(w.utilization),
            format_reset(w.reset),
        );
    }

    if let Some(ref w) = q.weekly {
        let remaining = 1.0 - w.utilization;
        let warning = if remaining < 0.10 {
            "  \x1b[31m!! critical\x1b[0m"
        } else if remaining < 0.20 {
            "  \x1b[33m! low\x1b[0m"
        } else {
            ""
        };
        println!(
            "   Weekly  (7d) {}  resets {}{}",
            render_bar(w.utilization),
            format_reset(w.reset),
            warning,
        );
    }

    if let Some(ref w) = q.overage {
        let label = match q.overage_disabled_reason.as_deref() {
            Some("out_of_credits") => "Extra usage  \x1b[31m(no credits)\x1b[0m",
            Some(r) => &format!("Extra usage  \x1b[33m({r})\x1b[0m"),
            None => "Extra usage ",
        };
        println!(
            "   {} {}  resets {}",
            label,
            render_bar(w.utilization),
            format_reset(w.reset),
        );
    }

    let status_str = match q.status.as_str() {
        "allowed" => format!("\x1b[32m{}\x1b[0m", q.status),
        "allowed_warning" => format!("\x1b[33m{}\x1b[0m", q.status),
        _ => format!("\x1b[31m{}\x1b[0m", q.status),
    };

    let claim = match q.representative_claim.as_str() {
        "five_hour" => "session",
        "seven_day" => "weekly",
        "seven_day_opus" => "Opus weekly",
        "seven_day_sonnet" => "Sonnet weekly",
        "overage" => "extra usage",
        other => other,
    };

    println!("   Status: {status_str}  (limit: {claim})");
}

fn print_rate_limits(result: &ProbeResult) {
    let rl = &result.rate_limits;
    if let (Some(lim), Some(rem)) = (rl.requests_limit, rl.requests_remaining) {
        println!("   RPM: {rem}/{lim}");
    }
    if let (Some(lim), Some(rem)) = (rl.input_tokens_limit, rl.input_tokens_remaining) {
        println!("   Input TPM: {rem}/{lim}");
    }
    if let (Some(lim), Some(rem)) = (rl.output_tokens_limit, rl.output_tokens_remaining) {
        println!("   Output TPM: {rem}/{lim}");
    }
}

pub fn print_history(snapshots: &[crate::store::Snapshot]) {
    if snapshots.is_empty() {
        println!("No snapshots recorded yet.");
        return;
    }

    println!();
    println!(" \x1b[1mtokeman\x1b[0m — {} snapshots", snapshots.len());
    println!();

    for s in snapshots {
        let local = s.probed_at.with_timezone(&Local);
        let u5 = s.utilization_5h.map(|u| format!("{:.1}%", (1.0 - u) * 100.0)).unwrap_or_else(|| "--".into());
        let u7 = s.utilization_7d.map(|u| format!("{:.1}%", (1.0 - u) * 100.0)).unwrap_or_else(|| "--".into());
        let status = s.unified_status.as_deref().unwrap_or("--");
        let claim = s.representative_claim.as_deref().map(|c| match c {
            "five_hour" => "session",
            "seven_day" => "weekly",
            "seven_day_opus" => "Opus",
            "seven_day_sonnet" => "Sonnet",
            "overage" => "extra",
            other => other,
        }).unwrap_or("--");

        let overage = s.utilization_overage
            .map(|u| format!("  ov: {:>5.1}%", (1.0 - u) * 100.0))
            .unwrap_or_default();

        println!(
            "  {} \x1b[1m{:<16}\x1b[0m  5h: {:>6} left  7d: {:>6} left{}  [{} {}]",
            local.format("%Y-%m-%d %H:%M"),
            s.token_name,
            u5,
            u7,
            overage,
            status,
            claim,
        );
    }
    println!();
}

pub fn format_reset_compact(ts: i64) -> String {
    if ts == 0 {
        return "--".to_string();
    }
    let reset = DateTime::from_timestamp(ts, 0).unwrap_or_default();
    let now = Utc::now();
    let diff = reset - now;

    if diff.num_seconds() < 0 {
        return "now".to_string();
    }

    let hours = diff.num_hours();
    let mins = diff.num_minutes() % 60;

    if hours > 24 {
        let local = reset.with_timezone(&Local);
        local.format("%a %-I%p").to_string()
    } else if hours > 0 {
        format!("{}h{}m", hours, mins)
    } else {
        format!("{}m", mins)
    }
}

pub fn print_stats(stats: &[crate::stats::TokenStats]) {
    if stats.is_empty() {
        println!("No stats available. Need at least 2 snapshots per token.");
        return;
    }

    println!();
    println!(" \x1b[1mtokeman stats\x1b[0m");
    println!();

    for s in stats {
        println!(" \x1b[1m{}\x1b[0m ({} snapshots)", s.token_name, s.snapshot_count);

        if s.snapshot_count < 2 {
            println!("   Need at least 2 snapshots to compute rates.");
            println!();
            continue;
        }

        if let Some(rate) = s.burn_rate_5h {
            println!("   5h burn rate (latest): {:.4}/hr", rate);
        }
        if let Some(h) = s.hours_to_depletion_5h {
            println!("   5h time to deplete:    {:.1}h", h);
        }
        if let Some(rate) = s.burn_rate_7d {
            println!("   7d burn rate (latest): {:.4}/hr", rate);
        }
        if let Some(mean) = s.mean_burn_7d {
            println!("   7d burn rate (mean):   {:.4}/hr", mean);
        }
        if let Some(sd) = s.stddev_burn_7d {
            println!("   7d burn rate (stddev): {:.4}/hr", sd);
        }
        if let Some(peak) = s.peak_burn_7d {
            println!("   7d burn rate (peak):   {:.4}/hr", peak);
        }
        if let Some(h) = s.hours_to_depletion_7d {
            println!("   7d time to deplete:    {:.1}h", h);
        }

        println!();
    }
}

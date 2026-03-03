use std::io::{self, BufRead, Write};

use anyhow::{bail, Result};
use chrono::{DateTime, Local, Utc};

use crate::config::Config;
use crate::probe::{self, ProbeResult};
use crate::store::Store;

pub fn select_best(results: &[ProbeResult]) -> Option<&ProbeResult> {
    results
        .iter()
        .filter(|r| {
            r.quota
                .as_ref()
                .is_some_and(|q| q.status == "allowed" || q.status == "allowed_warning")
        })
        .min_by(|a, b| {
            let a_weekly = a
                .quota
                .as_ref()
                .and_then(|q| q.weekly.as_ref())
                .map(|w| w.utilization)
                .unwrap_or(1.0);
            let b_weekly = b
                .quota
                .as_ref()
                .and_then(|q| q.weekly.as_ref())
                .map(|w| w.utilization)
                .unwrap_or(1.0);
            a_weekly
                .partial_cmp(&b_weekly)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn weekly_remaining_pct(result: &ProbeResult) -> String {
    result
        .quota
        .as_ref()
        .and_then(|q| q.weekly.as_ref())
        .map(|w| format!("{:.0}%", (1.0 - w.utilization) * 100.0))
        .unwrap_or_else(|| "??%".into())
}

fn session_remaining_pct(result: &ProbeResult) -> String {
    result
        .quota
        .as_ref()
        .and_then(|q| q.session.as_ref())
        .map(|w| format!("{:.0}%", (1.0 - w.utilization) * 100.0))
        .unwrap_or_else(|| "??%".into())
}

fn print_status_line(r: &ProbeResult) {
    let s5 = session_remaining_pct(r);
    let s7 = weekly_remaining_pct(r);
    let status = r
        .quota
        .as_ref()
        .map(|q| q.status.as_str())
        .unwrap_or("error");
    eprintln!(
        "   {:<20} 5h: {:>4} left  7d: {:>4} left  [{}]",
        r.token_name, s5, s7, status
    );
}

fn earliest_reset(results: &[ProbeResult]) -> Option<(String, i64)> {
    let now = Utc::now().timestamp();
    results
        .iter()
        .filter_map(|r| {
            let q = r.quota.as_ref()?;
            // Use the reset time of the limiting claim, not the minimum across all windows
            let reset = match q.representative_claim.as_str() {
                "five_hour" => q.session.as_ref().map(|w| w.reset),
                "overage" => q.overage.as_ref().map(|w| w.reset),
                // seven_day, seven_day_opus, seven_day_sonnet → weekly window
                _ => q.weekly.as_ref().map(|w| w.reset),
            };
            let reset = reset.filter(|&t| t > now)?;
            Some((r.token_name.clone(), reset))
        })
        .min_by_key(|(_, t)| *t)
}

pub fn format_duration_until(ts: i64) -> String {
    let now = Utc::now().timestamp();
    let diff = ts - now;
    if diff <= 0 {
        return "now".into();
    }
    let hours = diff / 3600;
    let mins = (diff % 3600) / 60;
    if hours > 0 {
        format!("{}h{}m", hours, mins)
    } else {
        format!("{}m", mins)
    }
}

fn prompt_relaunch(token_name: &str) -> bool {
    eprint!(
        " \x1b[1mtokeman:\x1b[0m relaunch with \x1b[1m{}\x1b[0m? [Y/n] ",
        token_name
    );
    io::stderr().flush().ok();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    let trimmed = line.trim().to_lowercase();
    trimmed.is_empty() || trimmed == "y" || trimmed == "yes"
}

pub async fn run(config: Config, auto: bool, claude_args: Vec<String>) -> Result<()> {
    if config.tokens.is_empty() {
        bail!("No tokens configured. Use `tokeman add <name> <key>` to add one.");
    }

    let store = Store::open()?;
    let mut last_token_name: Option<String> = None;

    loop {
        eprintln!();
        eprintln!(" \x1b[1mtokeman:\x1b[0m probing {} tokens...", config.tokens.len());

        let results = probe::probe_all(&config.tokens).await;
        for r in &results {
            let _ = store.insert(r);
        }

        // Show status of all tokens
        for r in &results {
            print_status_line(r);
        }

        // Pick best token
        let best = match select_best(&results) {
            Some(b) => b,
            None => {
                // All tokens exhausted — wait for reset
                eprintln!();
                eprintln!(" \x1b[1mtokeman:\x1b[31m all tokens exhausted\x1b[0m");

                if let Some((name, reset_ts)) = earliest_reset(&results) {
                    let reset_dt = DateTime::from_timestamp(reset_ts, 0)
                        .unwrap_or_default()
                        .with_timezone(&Local);

                    eprintln!(
                        " \x1b[1mtokeman:\x1b[0m earliest reset: {} in {} ({})",
                        name,
                        format_duration_until(reset_ts),
                        reset_dt.format("%-I:%M%p"),
                    );
                    eprintln!(" \x1b[1mtokeman:\x1b[0m waiting... (ctrl-c to quit)");

                    // Sleep and re-probe every 60s
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        let fresh = probe::probe_all(&config.tokens).await;
                        for r in &fresh {
                            let _ = store.insert(r);
                        }
                        if select_best(&fresh).is_some() {
                            eprintln!();
                            eprintln!(" \x1b[1mtokeman:\x1b[32m token available!\x1b[0m");
                            break;
                        }
                        if let Some((_, ts)) = earliest_reset(&fresh) {
                            eprint!(
                                "\r \x1b[1mtokeman:\x1b[0m waiting... {} until reset   ",
                                format_duration_until(ts),
                            );
                            io::stderr().flush().ok();
                        }
                    }
                    continue; // re-probe and select
                } else {
                    bail!("All tokens exhausted and no reset time available.");
                }
            }
        };

        let token_name = best.token_name.clone();
        let weekly_pct = weekly_remaining_pct(best);

        // Find the actual key for this token
        let token_key = config
            .tokens
            .iter()
            .find(|t| t.name == token_name)
            .map(|t| t.key.clone())
            .ok_or_else(|| anyhow::anyhow!("Token '{}' disappeared from config", token_name))?;

        // Check if this is a re-launch with a different token
        let is_switch = last_token_name
            .as_ref()
            .is_some_and(|prev| prev != &token_name);

        if is_switch {
            eprintln!();
            eprintln!(
                " \x1b[1mtokeman:\x1b[0m switching to \x1b[1m{}\x1b[0m ({} weekly left)",
                token_name, weekly_pct
            );

            if !auto && !prompt_relaunch(&token_name) {
                eprintln!(" \x1b[1mtokeman:\x1b[0m bye!");
                return Ok(());
            }
        } else {
            eprintln!();
            eprintln!(
                " \x1b[1mtokeman:\x1b[0m best token: \x1b[1m{}\x1b[0m ({} weekly left)",
                token_name, weekly_pct
            );
        }

        // Build the claude command
        let claude_bin = std::env::var("TOKEMAN_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
        let args_display = if claude_args.is_empty() {
            String::new()
        } else {
            format!(" {}", claude_args.join(" "))
        };
        eprintln!(
            " \x1b[1mtokeman:\x1b[0m launching: {}{}\n",
            claude_bin, args_display
        );

        // Spawn claude as a child process
        let mut child = tokio::process::Command::new(&claude_bin)
            .args(&claude_args)
            .env("CLAUDE_CODE_OAUTH_TOKEN", &token_key)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;

        // Background probe: sample the active token every 5 minutes while claude runs
        let bg_tokens = config.tokens.clone();
        let bg_handle = tokio::spawn(async move {
            let bg_store = Store::open().ok();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                let results = probe::probe_all(&bg_tokens).await;
                if let Some(ref s) = bg_store {
                    for r in &results {
                        let _ = s.insert(r);
                    }
                }
            }
        });

        let status = child.wait().await;
        bg_handle.abort();

        last_token_name = Some(token_name.clone());

        match status {
            Ok(s) if s.success() => {
                eprintln!();
                eprintln!(" \x1b[1mtokeman:\x1b[0m claude exited cleanly");
                if !auto {
                    return Ok(());
                }
                // In auto mode, still loop — user might want to keep going
                if !prompt_relaunch("continue") {
                    return Ok(());
                }
            }
            Ok(s) => {
                let code = s.code().unwrap_or(-1);
                eprintln!();
                eprintln!(
                    " \x1b[1mtokeman:\x1b[33m claude exited with code {}\x1b[0m",
                    code
                );
                // Re-probe and try again (or prompt)
            }
            Err(e) => {
                eprintln!();
                eprintln!(
                    " \x1b[1mtokeman:\x1b[31m failed to launch claude: {}\x1b[0m",
                    e
                );
                bail!("Could not launch claude: {e}");
            }
        }
    }
}

//! Commands that talk to the user: the browser login, resets, and capturing
//! Claude's own /login credential.

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};

use crate::account::Fleet;
use crate::anthropic::profile::{self, Cache};
use crate::anthropic::{Client, oauth, resets};
use crate::daemon;

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Walk the browser login for one account, or for every account that still
/// lacks a refreshable login. Each round: open a URL, paste what the page
/// shows, move on. `skip` passes, `q` stops. An unconfigured name enrolls a
/// login-only account.
pub async fn login_accounts(name: Option<&str>, more: bool) -> Result<()> {
    let fleet = Fleet::load()?;
    let needs_login = |account: &&crate::account::Account| !account.is_refreshable();
    let targets: Vec<String> = match (name, more) {
        (Some(name), _) => vec![name.to_owned()],
        (None, true) => fleet
            .accounts
            .iter()
            .filter(needs_login)
            .map(|a| a.name.clone())
            .collect(),
        (None, false) => fleet
            .accounts
            .iter()
            .find(needs_login)
            .map(|a| vec![a.name.clone()])
            .unwrap_or_default(),
    };
    if targets.is_empty() {
        println!("every configured account already has a refreshable login.");
        return Ok(());
    }
    let total = targets.len();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut done = 0usize;
    for (index, account) in targets.iter().enumerate() {
        let pkce = oauth::begin(account.contains('@').then_some(account.as_str()))?;
        println!("\n[{}/{total}] {account}", index + 1);
        if fleet.account(account).is_none() {
            println!("  (not configured yet: it will be added as a login-only account)");
        }
        println!(
            "  sign in as this account, then paste what the page shows:\n\n  {}\n",
            pkce.url
        );
        print!("  code (or `skip` / `q`): ");
        std::io::stdout().flush().ok();
        let Some(line) = lines.next() else { break };
        let line = line?;
        let answer = line.trim();
        if answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit") {
            break;
        }
        if answer.is_empty() || answer.eq_ignore_ascii_case("skip") {
            println!("  skipped {account}");
            continue;
        }
        match oauth::exchange(Client::shared(), answer, &pkce).await {
            Ok(grant) => {
                if grant.refresh_token.is_none() {
                    eprintln!(
                        "  {account}: the grant came back without a refresh token; not stored"
                    );
                    continue;
                }
                if !grant.scopes.iter().any(|scope| scope == "user:profile") {
                    eprintln!(
                        "  {account}: granted scopes are {:?}; without user:profile, /feedback and usage reads stay refused",
                        grant.scopes
                    );
                }
                let scopes = grant.scopes.join(" ");
                Fleet::update(|fleet| {
                    fleet
                        .ensure_account(account)
                        .apply_grant(grant, chrono::Utc::now().timestamp_millis());
                    Ok(())
                })?;
                done += 1;
                println!("  {account}: stored ({scopes})");
            }
            Err(error) => eprintln!("  {account}: {error:#}"),
        }
    }
    println!("\n{done}/{total} account(s) updated.");
    if done > 0 {
        println!(
            "the rotation service installs these on its next cycle; `tokeman rotate status` shows which credential each account uses."
        );
    }
    Ok(())
}

/// Every logged-in account's reset offers.
pub async fn list_resets(only: Option<&str>) -> Result<()> {
    let fleet = Fleet::load()?;
    let now = now_secs();
    let cache = Cache::default();
    for account in fleet
        .accounts
        .iter()
        .filter(|a| only.is_none_or(|name| a.name == name))
    {
        if account.profile_token(now * 1000).is_none() {
            if only.is_some() {
                println!(
                    "{}: no login; run `tokeman login {}`",
                    account.name, account.name
                );
            }
            continue;
        }
        println!("{}", account.name);
        match resets::status(Client::shared(), &cache, account, false, now).await {
            Ok(status) => print_reset_status(&status),
            Err(error) => println!("  unavailable: {error:#}"),
        }
    }
    Ok(())
}

fn print_reset_status(status: &resets::ResetStatus) {
    match &status.cedar_ember {
        Some(program) if program.eligible => {
            let grants = program.grants();
            if grants.is_empty() {
                println!("  limit resets: none on offer");
            }
            for grant in grants {
                let next = program.next_grant_id.as_deref() == Some(grant.id.as_str());
                println!(
                    "  {} {}",
                    if next { "*" } else { " " },
                    resets::describe_grant(&grant)
                );
            }
            if let Some(until) = &program.cooldown_until {
                println!("    cooling down until {until}");
            }
        }
        Some(program) => println!(
            "  limit resets: not eligible ({})",
            program
                .ineligible_reason
                .as_deref()
                .unwrap_or("no reason given")
        ),
        None => println!("  limit resets: not offered"),
    }
    match &status.juniper_tide {
        Some(session) if session.available => {
            println!(
                "  session reset: available ({} per week)",
                session.resets_per_week.unwrap_or(1)
            )
        }
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
        None => println!("  session reset: not offered"),
    }
}

/// Claim a reset after showing what it clears. Resets are scarce and cannot
/// be taken back, so this asks unless `yes`.
pub async fn use_reset(name: &str, grant: Option<&str>, session: bool, yes: bool) -> Result<()> {
    let fleet = Fleet::load()?;
    let account = fleet
        .account(name)
        .with_context(|| format!("no configured account named {name}"))?;
    let now = now_secs();
    let client = Client::shared();
    let status = resets::status(client, &Cache::default(), account, true, now).await?;
    let grant_id = if session {
        let session = status
            .juniper_tide
            .as_ref()
            .context("the session reset is not offered to this account")?;
        if !session.available {
            bail!(
                "the session reset is not available ({})",
                session
                    .ineligible_reason
                    .as_deref()
                    .unwrap_or("not offered")
            );
        }
        println!("{name}: session reset (refills the 5-hour limit, paid from the weekly limit)");
        None
    } else {
        let program = status
            .cedar_ember
            .as_ref()
            .context("limit resets are not offered to this account")?;
        if !program.eligible {
            bail!(
                "not eligible for limit resets ({})",
                program
                    .ineligible_reason
                    .as_deref()
                    .unwrap_or("no reason given")
            );
        }
        let wanted = grant
            .map(str::to_owned)
            .or_else(|| program.next_grant_id.clone());
        let chosen = program
            .grants()
            .into_iter()
            .find(|candidate| Some(&candidate.id) == wanted.as_ref())
            .context("no such grant on offer; see `tokeman resets list`")?;
        if !chosen.usable_now {
            bail!(
                "that grant is not usable right now{}",
                if chosen.use_requires_limit {
                    " (it only works at a usage limit)"
                } else {
                    ""
                }
            );
        }
        println!("{name}: {}", resets::describe_grant(&chosen));
        Some(chosen.id)
    };
    if !yes {
        print!("use it now? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("kept.");
            return Ok(());
        }
    }
    let outcome = resets::claim(client, account, grant_id.as_deref(), now).await?;
    let cleared: Vec<&str> = outcome
        .cleared
        .iter()
        .map(|w| resets::window_label(w))
        .collect();
    use resets::ClaimResult::*;
    match outcome.result {
        Reset => println!(
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
                .map(|r| format!(" ({r})"))
                .unwrap_or_default(),
            outcome
                .cooldown_until
                .as_deref()
                .map(|at| format!("; cooldown until {at}"))
                .unwrap_or_default()
        ),
    }
    Ok(())
}

/// Copy Claude's own /login access token to an account, for profile reads.
pub async fn capture_usage(name: &str) -> Result<()> {
    let token = claude_login_token()?;
    let now = now_secs();
    match profile::validate(Client::shared(), &Cache::default(), &token, now).await {
        Ok(buckets) => {
            Fleet::update(|fleet| {
                fleet.ensure_account(name).capture_profile_token(token);
                Ok(())
            })?;
            println!(
                "captured Claude's login credential for {name} ({} model bucket(s) readable)",
                buckets.len()
            );
            daemon::reconcile_after_login()
        }
        Err(error) => bail!("Claude's current login cannot read profile usage: {error}"),
    }
}

#[cfg(target_os = "macos")]
fn claude_login_token() -> Result<String> {
    let output = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()?;
    if !output.status.success() {
        bail!("could not read Claude's Keychain credential");
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("Claude Keychain credential is not valid JSON")?;
    value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context("Claude Keychain credential has no OAuth access token")
}

#[cfg(not(target_os = "macos"))]
fn claude_login_token() -> Result<String> {
    bail!("`tokeman usage capture` reads Claude's macOS Keychain")
}

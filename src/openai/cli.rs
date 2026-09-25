//! `tokeman codex …` command handlers.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

use crate::config::{CodexAccount, Config, expand_tilde, tilde};
use crate::openai::appserver::{self, AppServer};
use crate::openai::authfile::CodexHome;
use crate::openai::probe::{self, CodexProbeResult};
use crate::openai::refresh;

/// Directories worth checking when the user has not registered anything yet.
fn discovery_roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut roots = vec![home.join(".codex")];
    for parent in [home.join(".codex-homes"), home.join(".codex-lanes")] {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                roots.push(entry.path());
            }
        }
    }
    roots.retain(|root| root.join("auth.json").exists());
    roots.sort();
    roots
}

fn short_name(path: &std::path::Path) -> String {
    match path.file_name().and_then(|n| n.to_str()) {
        // ~/.codex is the default profile; ".codex" reads better as "default".
        Some(".codex") => "default".to_string(),
        Some(name) => name.to_string(),
        None => path.display().to_string(),
    }
}

pub fn add(name: String, codex_home: String, label: Option<String>) -> Result<()> {
    let (mut config, config_lock) = Config::load_locked()?;
    let home = CodexHome::new(expand_tilde(&codex_home));
    if !home.exists() {
        bail!(
            "{} has no auth.json — run `CODEX_HOME={} codex login` first",
            home.path().display(),
            home.path().display()
        );
    }
    config.upsert_codex_account(CodexAccount {
        name: name.clone(),
        codex_home,
        label,
        ..Default::default()
    });
    config.save(&config_lock)?;
    println!("added codex account {name}");
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    let (mut config, config_lock) = Config::load_locked()?;
    if !config.remove_codex_account(name) {
        bail!("no codex account named {name}");
    }
    config.save(&config_lock)?;
    println!("removed codex account {name}");
    Ok(())
}

pub fn discover(adopt: bool) -> Result<()> {
    let (mut config, config_lock) = Config::load_locked()?;
    let roots = discovery_roots();
    if roots.is_empty() {
        println!("no Codex profiles found under ~/.codex, ~/.codex-homes, or ~/.codex-lanes");
        return Ok(());
    }

    let mut added = 0;
    for root in &roots {
        let name = short_name(root);
        let known = config
            .codex_accounts
            .iter()
            .any(|account| account.home().path() == root);
        let marker = if known { "registered" } else { "new" };
        println!("{:<12} {:<40} {marker}", name, tilde(root));
        if adopt && !known {
            config.upsert_codex_account(CodexAccount {
                name,
                codex_home: tilde(root),
                ..Default::default()
            });
            added += 1;
        }
    }

    if adopt && added > 0 {
        config.save(&config_lock)?;
        println!("\nregistered {added} account(s)");
    } else if !adopt {
        println!("\nre-run with --adopt to register these");
    }
    Ok(())
}

fn fmt_window(window: Option<&probe::CodexWindow>) -> String {
    let Some(window) = window else {
        return "-".to_string();
    };
    let used = window
        .used_percent
        .map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| "?".into());
    match window.reset_after_seconds {
        Some(secs) if secs > 0 => format!("{used} ({})", fmt_duration(secs)),
        _ => used,
    }
}

fn fmt_duration(seconds: i64) -> String {
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

fn state_label(result: &CodexProbeResult) -> String {
    if let Some(error) = &result.error {
        let hint = if result.unauthorized {
            " — needs `tokeman codex refresh`"
        } else {
            ""
        };
        return format!("ERROR: {error}{hint}");
    }
    if result.is_viable() {
        "ok".to_string()
    } else {
        match result.limit_reason() {
            Some(reason) => format!("limited ({reason})"),
            None => "limited".to_string(),
        }
    }
}

pub async fn list(json: bool) -> Result<()> {
    let config = Config::load()?;
    if config.codex_accounts.is_empty() {
        println!("no codex accounts configured — try `tokeman codex discover --adopt`");
        return Ok(());
    }

    let results = probe::probe_all(&config.codex_accounts).await;

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(());
    }

    println!(
        "{:<12} {:<26} {:<7} {:<16} {:<16} {}",
        "ACCOUNT", "EMAIL", "PLAN", "PRIMARY", "SECONDARY", "STATE"
    );
    for result in &results {
        println!(
            "{:<12} {:<26} {:<7} {:<16} {:<16} {}",
            result.account_name,
            result.email().unwrap_or("-"),
            result.plan().unwrap_or("-"),
            fmt_window(result.primary()),
            fmt_window(result.secondary()),
            state_label(result),
        );
        // Per-model buckets are metered independently of the account-wide
        // window, so they get their own line rather than being folded in.
        for extra in result.additional() {
            let rate_limit = extra.rate_limit.as_ref();
            println!(
                "  └ {:<24} {:<16} {}",
                extra.limit_name.as_deref().unwrap_or("?"),
                fmt_window(rate_limit.and_then(|r| r.primary_window.as_ref())),
                if rate_limit.and_then(|r| r.limit_reached) == Some(true) {
                    "limited"
                } else {
                    "ok"
                },
            );
        }
    }
    Ok(())
}

pub async fn refresh_accounts(name: Option<String>, force: bool, max_age_days: i64) -> Result<()> {
    let config = Config::load()?;
    let accounts: Vec<_> = match &name {
        Some(name) => vec![
            config
                .codex_account(name)
                .with_context(|| format!("no codex account named {name}"))?
                .clone(),
        ],
        None => config.codex_accounts.clone(),
    };
    if accounts.is_empty() {
        bail!("no codex accounts configured");
    }

    let mut failures = 0;
    for account in accounts {
        match refresh::refresh(&account.home(), force, max_age_days).await {
            Ok(Some(_)) => println!("{:<12} refreshed", account.name),
            Ok(None) => println!("{:<12} still fresh", account.name),
            Err(err) => {
                failures += 1;
                println!("{:<12} FAILED: {err:#}", account.name);
            }
        }
    }

    if failures > 0 {
        bail!("{failures} account(s) failed to refresh");
    }
    Ok(())
}

pub async fn seat(account_name: &str, into: &str) -> Result<()> {
    let config = Config::load()?;
    let account = config
        .codex_account(account_name)
        .with_context(|| format!("no codex account named {account_name}"))?;

    let source = account.home();
    let target = CodexHome::new(expand_tilde(into));
    if source.path() == target.path() {
        bail!("source and target are the same profile");
    }

    // Refresh first so we never seat a bundle that is about to 401. A 401 in
    // the target would send it down the account-mismatch recovery path.
    if let Err(err) = refresh::refresh_if_stale(&source, refresh::DEFAULT_MAX_AGE_DAYS).await {
        eprintln!("warning: could not refresh {account_name} before seating: {err:#}");
    }

    let bundle = source
        .read()?
        .bundle()
        .with_context(|| format!("{} has no usable OAuth bundle", source.path().display()))?;
    crate::openai::authfile::seat(&bundle, &target)?;
    println!("seated {account_name} into {}", tilde(target.path()));
    Ok(())
}

/// The viable, unparked account scoring highest; `score` returning `None`
/// rules an account out. Ties go to the later account.
fn pick<'a>(
    results: &'a [CodexProbeResult],
    config: &Config,
    score: impl Fn(&CodexProbeResult) -> Option<f64>,
) -> Option<&'a CodexProbeResult> {
    results
        .iter()
        .filter(|result| result.is_viable())
        .filter(|result| {
            config
                .codex_account(&result.account_name)
                .is_none_or(|account| !account.parked)
        })
        .filter_map(|result| score(result).map(|value| (result, value)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(result, _)| result)
}

/// The viable account with the most headroom.
fn best<'a>(results: &'a [CodexProbeResult], config: &Config) -> Option<&'a CodexProbeResult> {
    pick(results, config, |result| {
        Some(result.remaining().unwrap_or(0.0))
    })
}

/// Print the best account's CODEX_HOME.
///
/// With `limit`, rank by that per-model bucket instead of the account-wide
/// window — the account with the most general headroom is not necessarily the
/// one with Spark left, since those are metered separately.
pub async fn best_home(limit: Option<String>) -> Result<()> {
    let config = Config::load()?;
    let results = probe::probe_all(&config.codex_accounts).await;

    let winner = match &limit {
        Some(limit) => pick(&results, &config, |result| {
            result.remaining_for(limit).filter(|left| *left > 0.0)
        }),
        None => best(&results, &config),
    };

    let Some(winner) = winner else {
        match &limit {
            Some(limit) => bail!("no viable codex account with headroom for {limit}"),
            None => bail!("no viable codex account (all limited or erroring)"),
        }
    };
    println!("{}", winner.codex_home);
    Ok(())
}

/// `--codex-home` if given, else `~/.codex`.
fn target_home(codex_home: Option<String>) -> Result<CodexHome> {
    match codex_home {
        Some(path) => Ok(CodexHome::new(expand_tilde(&path))),
        None => CodexHome::default_home(),
    }
}

pub async fn reseat(
    codex_home: Option<String>,
    account: Option<String>,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    reseat_home(&target_home(codex_home)?, account, dry_run, force).await
}

async fn reseat_home(
    target_home: &CodexHome,
    account: Option<String>,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let config = Config::load()?;
    let results = probe::probe_all(&config.codex_accounts).await;
    let chosen = match &account {
        Some(name) => results
            .iter()
            .find(|result| result.account_name == *name)
            .with_context(|| format!("no codex account named {name}"))?,
        None => best(&results, &config).context("no viable codex account to migrate to")?,
    };

    if !chosen.is_viable() {
        if !force {
            bail!(
                "{} is not viable right now: {} (pass --force to migrate anyway)",
                chosen.account_name,
                state_label(chosen)
            );
        }
        eprintln!(
            "warning: {} is {} — migrating anyway because --force was given",
            chosen.account_name,
            state_label(chosen)
        );
    }

    let socket = appserver::control_socket_path(target_home.path());
    println!(
        "migrating {} -> {} (socket {})",
        tilde(target_home.path()),
        chosen.account_name,
        tilde(&socket),
    );

    if dry_run {
        println!("dry run: not contacting the app-server");
        return Ok(());
    }

    let source = config
        .codex_account(&chosen.account_name)
        .context("chosen account vanished from config")?
        .home();

    // The app-server takes the access token as-is and does not refresh it, so
    // hand it a freshly minted one.
    if let Err(err) = refresh::refresh_if_stale(&source, refresh::DEFAULT_MAX_AGE_DAYS).await {
        eprintln!(
            "warning: could not refresh {} first: {err:#}",
            chosen.account_name
        );
    }

    let auth = source.read()?;
    let access_token = auth
        .access_token()
        .context("chosen account has no access_token")?;
    let account_id = auth
        .effective_account_id()
        .context("chosen account has no chatgpt_account_id")?;
    let plan = auth.claims().and_then(|claims| claims.chatgpt_plan_type);

    let mut server = AppServer::connect(&socket).await?;
    let before = server.read_account().await.ok();
    server
        .reseat(access_token, &account_id, plan.as_deref())
        .await?;

    // Verify rather than trust: a successful RPC only means the request was
    // accepted, and nothing on disk changes, so this is the only way to know
    // the running process actually moved.
    let after = server.read_account().await.ok();
    server.close().await;

    let describe = |value: &Option<serde_json::Value>| -> String {
        let Some(value) = value else {
            return "unknown".into();
        };
        let find = |key: &str| {
            let mut found = Vec::new();
            json_values(value, key, &mut found);
            found
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string()
        };
        format!("{} ({})", find("email"), find("planType"))
    };

    println!(
        "re-seated onto {}: {} -> {}",
        chosen.account_name,
        describe(&before),
        describe(&after)
    );
    Ok(())
}

/// Import externally-supplied ChatGPT access tokens as codex profiles.
///
/// Input is one `<name> <access-token-jwt>` per line (blank lines and `#`
/// comments ignored). Each token is decoded for its account id, seeded into a
/// fresh profile directory, and registered as an access-only account. These are
/// stopgaps — see [`seat_access_only`] — so they carry an `access_only` flag and
/// a label noting when the token expires.
pub async fn import(from: PathBuf, homes_root: Option<String>, register: bool) -> Result<()> {
    use crate::openai::authfile::{IdClaims, seat_access_only};

    let text = std::fs::read_to_string(&from)
        .with_context(|| format!("failed to read {}", from.display()))?;
    let root = homes_root
        .map(|value| expand_tilde(&value))
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".codex-homes")
        });

    let (mut config, config_lock) = Config::load_locked()?;
    let mut imported = 0;
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, token)) = line.split_once(char::is_whitespace) else {
            bail!("line {}: expected `<name> <access-token>`", lineno + 1);
        };
        let name = name.trim();
        let token = token.trim();

        let claims = IdClaims::decode(token)
            .with_context(|| format!("line {}: {name} token is not a decodable JWT", lineno + 1))?;
        let Some(account_id) = claims.chatgpt_account_id.clone() else {
            bail!(
                "line {}: {name} token carries no chatgpt_account_id",
                lineno + 1
            );
        };

        let dir = root.join(slugify(name));
        let home = CodexHome::new(&dir);
        seat_access_only(token, &account_id, &home)?;

        let expiry = claims
            .expires_at
            .map(|exp| format!("access token expires {}", fmt_epoch(exp)))
            .unwrap_or_else(|| "access-only".to_string());
        let plan = claims.chatgpt_plan_type.as_deref().unwrap_or("?");

        if register {
            config.upsert_codex_account(CodexAccount {
                name: name.to_string(),
                codex_home: tilde(&dir),
                label: Some(format!("pug import, {plan}, {expiry}")),
                access_only: true,
                ..Default::default()
            });
        }
        println!("imported {name:<28} plan={plan:<5} -> {}", tilde(&dir));
        imported += 1;
    }

    if register && imported > 0 {
        config.save(&config_lock)?;
    }
    if imported == 0 {
        bail!("no tokens found in {}", from.display());
    }
    println!(
        "\n{imported} account(s) imported. These are access-token-only and cannot be refreshed;"
    );
    println!(
        "ask the source for full auth.json bundles (with refresh tokens) to make them durable."
    );
    Ok(())
}

fn slugify(name: &str) -> String {
    // ops@example.com -> ops-example
    let (local, domain) = name.split_once('@').unwrap_or((name, ""));
    let domain_head = domain.split('.').next().unwrap_or("");
    let raw = if domain_head.is_empty() {
        local.to_string()
    } else {
        format!("{local}-{domain_head}")
    };
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn fmt_epoch(epoch: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_else(|| epoch.to_string())
}

/// Follow a running Codex and migrate it before it runs out of room.
///
/// Threshold is on *remaining* headroom, and it is deliberately not zero: the
/// swap lands between turns, so waiting for 100% used means the user eats a
/// failed turn first. Migrating at ~20% remaining keeps the handover invisible.
pub async fn watch(codex_home: Option<String>, min_remaining: f64, once: bool) -> Result<()> {
    let target_home = target_home(codex_home)?;
    let socket = appserver::control_socket_path(target_home.path());
    // Absence just means no GUI or daemon is running against this profile.
    if !socket.exists() {
        bail!(
            "no app-server control socket at {} — start Codex, or run `codex app-server --listen unix://`",
            tilde(&socket)
        );
    }

    let mut server = AppServer::connect(&socket).await?;
    println!(
        "watching {} (migrate below {:.0}% remaining)",
        tilde(target_home.path()),
        min_remaining * 100.0
    );

    // Evaluate what it reports right now before waiting for a push.
    let mut pending = server.rate_limits().await.ok();
    loop {
        if let Some(limits) = &pending
            && let Some(remaining) = remaining_from_rate_limits(limits)
        {
            println!("  headroom: {:.0}%", remaining * 100.0);
            if remaining < min_remaining {
                println!("  below threshold — migrating");
                server.close().await;
                reseat_home(
                    &target_home,
                    None,
                    /*dry_run*/ false,
                    /*force*/ false,
                )
                .await?;
                if once {
                    return Ok(());
                }
                server = AppServer::connect(&socket).await?;
                pending = server.rate_limits().await.ok();
                continue;
            }
        }
        if once {
            return Ok(());
        }
        // Codex pushes this whenever its view of the limits changes, so there
        // is nothing to poll.
        pending = Some(
            server
                .next_notification("account/rateLimits/updated")
                .await?,
        );
    }
}

/// Every value stored under `key` anywhere in `value`, each object's own
/// entry before those nested inside it.
fn json_values<'a>(value: &'a serde_json::Value, key: &str, out: &mut Vec<&'a serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            out.extend(map.get(key));
            for nested in map.values() {
                json_values(nested, key, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                json_values(item, key, out);
            }
        }
        _ => {}
    }
}

/// Pull a remaining-headroom fraction out of whatever shape the app-server
/// reports, without depending on the exact nesting.
fn remaining_from_rate_limits(value: &serde_json::Value) -> Option<f64> {
    let mut used = Vec::new();
    json_values(value, "usedPercent", &mut used);
    json_values(value, "used_percent", &mut used);
    // The fullest window is the one that will bite first.
    used.into_iter()
        .filter_map(serde_json::Value::as_f64)
        .reduce(f64::max)
        .map(|worst| (1.0 - worst / 100.0).clamp(0.0, 1.0))
}

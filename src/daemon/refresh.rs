//! Renew login grants before they expire. A refresh token is single-use and
//! a refresh revokes the access token it replaces, so each account is
//! re-read under the fleet lock right before redeeming, and the revocation
//! is recorded for attribution.

use std::collections::BTreeMap;

use anyhow::Result;

use super::lock::RotationLock;
use super::log::log;
use super::now_epoch;
use crate::account::{Account, Fleet};
use crate::anthropic::{Client, oauth};
use crate::claude::settings as claude_settings;
use crate::history::{self, Event, EventKind};
use crate::ui::text::one_line;

/// Per-account retry schedule for refreshes that failed transiently. Held in
/// the daemon's memory: a restart retrying at once is the right behaviour.
#[derive(Debug, Default)]
pub struct Backoff {
    accounts: BTreeMap<String, (u32, f64)>,
}

impl Backoff {
    fn allows(&self, account: &str, now: f64) -> bool {
        self.accounts
            .get(account)
            .is_none_or(|(_, next_attempt)| now >= *next_attempt)
    }

    /// Record a failure; true for the first in a streak.
    fn fail(&mut self, account: &str, now: f64) -> bool {
        let entry = self.accounts.entry(account.to_owned()).or_insert((0, 0.0));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = now + (60.0 * 2f64.powi(entry.0.min(4) as i32 - 1)).min(600.0);
        entry.0 == 1
    }

    fn clear(&mut self, account: &str) {
        self.accounts.remove(account);
    }
}

/// Refresh every grant that is due; returns how many accounts changed.
pub async fn refresh_due(backoff: &mut Backoff) -> Result<usize> {
    refresh_where(backoff, |account, now_ms| account.needs_refresh(now_ms)).await
}

/// Refresh one account, or every account with a login, now.
pub async fn refresh_now(name: Option<&str>) -> Result<usize> {
    let mut backoff = Backoff::default();
    refresh_where(&mut backoff, |account, _| {
        account.is_refreshable() && name.is_none_or(|name| account.name == name)
    })
    .await
}

async fn refresh_where(
    backoff: &mut Backoff,
    due: impl Fn(&Account, i64) -> bool,
) -> Result<usize> {
    let now = now_epoch();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let names: Vec<String> = Fleet::load()?
        .accounts
        .iter()
        .filter(|account| due(account, now_ms) && backoff.allows(&account.name, now))
        .map(|account| account.name.clone())
        .collect();
    if names.is_empty() {
        return Ok(0);
    }
    let _lock = RotationLock::acquire()?;
    let mut changed = 0;
    for name in names {
        let (mut fleet, fleet_lock) = Fleet::load_locked()?;
        let Some(account) = fleet.account_mut(&name) else {
            continue;
        };
        if !due(account, chrono::Utc::now().timestamp_millis()) {
            continue;
        }
        let Some(login) = account.login.as_ref() else {
            continue;
        };
        let Some(refresh_token) = login.refresh_token.clone() else {
            continue;
        };
        let scopes = login.scopes.clone();
        match oauth::refresh(Client::shared(), &refresh_token, &scopes).await {
            Ok(grant) => {
                let replaced = login.access_token.clone();
                account.apply_grant(grant, chrono::Utc::now().timestamp_millis());
                fleet.save(&fleet_lock)?;
                backoff.clear(&name);
                changed += 1;
                if let Some(old) = replaced
                    && let Err(error) =
                        history::append(&Event::revoke(chrono::Utc::now().timestamp(), &name, &old))
                {
                    log(&format!(
                        "ERROR could not record credential history: {error:#}"
                    ));
                }
            }
            Err(error) if oauth::is_permanent(&error) => {
                let fallback = if account.setup_key.is_some() {
                    "its setup key"
                } else {
                    "nothing"
                };
                let reason = one_line(&format!("{error:#}"), 160);
                if let Some(login) = account.login.as_mut() {
                    login.error = Some(reason.clone());
                }
                fleet.save(&fleet_lock)?;
                changed += 1;
                log(&format!(
                    "ERROR login for {name} was refused permanently ({reason}); it falls back to {fallback} until `tokeman login {name}`"
                ));
            }
            Err(error) => {
                if backoff.fail(&name, now) {
                    log(&format!(
                        "login refresh for {name} failed; retrying with backoff ({})",
                        one_line(&format!("{error:#}"), 160)
                    ));
                }
            }
        }
    }
    if changed > 0 {
        reoffer_default(&Fleet::load()?)?;
    }
    Ok(changed)
}

/// After a refresh, make sure the default account's settings entry offers
/// its newest token.
pub fn reoffer_default(fleet: &Fleet) -> Result<()> {
    let mut settings = claude_settings::load()?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let default =
        claude_settings::configured_token(&settings).and_then(|value| fleet.account_holding(value));
    if let Some(account) = default
        && let Some(kind) = claude_settings::reinstall_reason(&settings, account, now_ms)
    {
        let event = claude_settings::install(&mut settings, account, now_ms, kind)?;
        claude_settings::commit(&settings, Some(event))?;
        if !matches!(kind, EventKind::Refresh | EventKind::Repair) {
            log(&format!(
                "{}: {} now offers {}",
                kind.label(),
                account.name,
                claude_settings::describe_credential(account, now_ms)
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Backoff;

    #[test]
    fn transient_refresh_failures_back_off_a_minute_then_double_to_ten() {
        let mut backoff = Backoff::default();
        assert!(backoff.fail("a", 0.0));
        assert!(!backoff.allows("a", 30.0));
        assert!(backoff.allows("a", 60.0));
        assert!(!backoff.fail("a", 60.0));
        assert!(!backoff.allows("a", 170.0));
        for _ in 0..5 {
            backoff.fail("a", 1_000.0);
        }
        // The step caps at 2^3 minutes (480 s).
        assert!(!backoff.allows("a", 1_479.0));
        assert!(backoff.allows("a", 1_480.0));
        backoff.clear("a");
        assert!(backoff.allows("a", 0.0));
    }
}

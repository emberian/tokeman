//! `tokeman claude ...`: accounts, the browser login in two halves, grant
//! refresh, and usage-limit resets.

use anyhow::{Context, Result, bail};

use super::now;
use crate::account::{Account, Fleet};
use crate::anthropic::profile::{Cache, Endpoint, ReadPolicy};
use crate::anthropic::{Client, oauth, resets};
use crate::claims::{Journal, Pending};
use crate::daemon;
use crate::quota::Service;

/// Add an account with a `claude setup-token` key.
pub fn add(name: &str, key: String) -> Result<()> {
    Fleet::update(|fleet| {
        fleet.add_setup_key(name, key);
        Ok(())
    })?;
    daemon::reconcile_after_login()
}

pub fn remove(name: &str) -> Result<()> {
    let (fleet, removed) = Fleet::update(|fleet| {
        let existing = fleet.account(name).cloned();
        fleet.remove_account(name);
        Ok(existing)
    })?;
    let Some(removed) = removed else {
        bail!("Account '{name}' not found");
    };
    {
        let _lock = daemon::lock::RotationLock::acquire()?;
        if let Ok(mut settings) = crate::claude::settings::load() {
            let matches_name =
                crate::claude::settings::env(&settings, crate::claude::settings::ACCOUNT)
                    == Some(name);
            let matches_token = crate::claude::settings::configured_token(&settings)
                .is_some_and(|token| removed.holds(token));
            if (matches_name || matches_token) && crate::claude::settings::clear(&mut settings) {
                let _ = crate::claude::settings::commit(
                    &settings,
                    Some(crate::history::Event::pause(now())),
                );
            }
        }
    }
    if !fleet.accounts.is_empty() {
        daemon::reconcile_after_login()?;
    }
    Ok(())
}

/// The accounts a `login` walks: the named one (configured or not), every
/// account without a refreshable login (`more`), or the first such.
pub fn login_targets(fleet: &Fleet, name: Option<&str>, more: bool) -> Vec<String> {
    let needs_login = |account: &&Account| !account.is_refreshable();
    match (name, more) {
        (Some(name), _) => vec![name.to_owned()],
        (None, true) => fleet
            .accounts
            .iter()
            .filter(needs_login)
            .map(|account| account.name.clone())
            .collect(),
        (None, false) => fleet
            .accounts
            .iter()
            .find(needs_login)
            .map(|account| vec![account.name.clone()])
            .unwrap_or_default(),
    }
}

/// The first half of a login: the URL to open, and the verifier that the
/// pasted code is exchanged with. An email-shaped name is the login hint.
pub fn begin_login(account: &str) -> Result<oauth::Pkce> {
    Ok(oauth::begin(account.contains('@').then_some(account))?)
}

/// How a pasted code ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    /// The grant is in tokens.toml. `profile` is false when the granted
    /// scopes lack `user:profile` (usage reads stay refused).
    Stored { scopes: Vec<String>, profile: bool },
    /// The grant came back without a refresh token and was not stored.
    NoRefreshToken,
}

/// The second half: exchange the pasted code and store the grant, enrolling
/// `account` as login-only when it is not configured.
pub async fn finish_login(account: &str, pasted: &str, pkce: &oauth::Pkce) -> Result<LoginOutcome> {
    let grant = oauth::exchange(Client::shared(), pasted, pkce).await?;
    if grant.refresh_token.is_none() {
        return Ok(LoginOutcome::NoRefreshToken);
    }
    let scopes = grant.scopes.clone();
    Fleet::update(|fleet| {
        fleet
            .ensure_account(account)
            .record_login(grant, chrono::Utc::now().timestamp_millis());
        Ok(())
    })?;
    Ok(LoginOutcome::Stored {
        profile: scopes.iter().any(|scope| scope == "user:profile"),
        scopes,
    })
}

/// What `tokeman claude refresh` did.
#[derive(Debug, Clone)]
pub struct Refreshed {
    /// How many grants were renewed.
    pub count: usize,
    /// The accounts asked about that have a login, as they are now.
    pub accounts: Vec<Account>,
}

/// Renew login grants now instead of waiting for the daemon.
pub async fn refresh(name: Option<&str>) -> Result<Refreshed> {
    let summary = daemon::refresh::refresh_now(name).await?;
    if summary.renewed == 0
        && (!summary.permanent_failures.is_empty() || !summary.transient_failures.is_empty())
    {
        let details: Vec<String> = summary
            .permanent_failures
            .iter()
            .chain(&summary.transient_failures)
            .map(|(acct, err)| format!("{acct}: {err}"))
            .collect();
        bail!("failed to refresh login grant(s): {}", details.join("; "));
    }
    let accounts = Fleet::load()?
        .accounts
        .into_iter()
        .filter(|account| name.is_none_or(|name| account.name == name))
        .filter(Account::is_refreshable)
        .collect();
    Ok(Refreshed {
        count: summary.renewed,
        accounts,
    })
}

/// Make `name` the default now.
pub fn use_account(fleet: &Fleet, name: &str) -> Result<()> {
    daemon::activate(fleet, name)
}

/// One account's reset offers, for `tokeman claude reset` with no account.
#[derive(Debug, Clone)]
pub struct ResetListing {
    pub account: String,
    /// `None` for an account with no login (nothing can be read).
    pub status: Option<Result<resets::ResetStatus, String>>,
    /// A grant claim sent without a known outcome.
    pub pending: Option<Pending>,
}

/// Every logged-in account's reset offers, from the shared profile cache.
pub async fn reset_offers(fleet: &Fleet, named: bool) -> Vec<ResetListing> {
    let at = now();
    let cache = Cache::default();
    let journal = Journal::default();
    let mut listings = Vec::new();
    for account in &fleet.accounts {
        let status = match account.profile_token(at * 1000) {
            None => None,
            Some(_) => {
                let first_policy = if named {
                    ReadPolicy::Cached
                } else {
                    ReadPolicy::Stored
                };
                let mut result =
                    resets::status(Client::shared(), &cache, account, first_policy, at).await;
                if !named
                    && result
                        .as_ref()
                        .is_err_and(|_| cache.stored(&account.name, Endpoint::CedarEmber).is_none())
                {
                    result =
                        resets::status(Client::shared(), &cache, account, ReadPolicy::Cached, at)
                            .await;
                } else if !named
                    && let Ok(ref current) = result
                    && current.cedar_ember.is_none()
                    && cache.stored(&account.name, Endpoint::CedarEmber).is_none()
                    && let Ok(cedar) = resets::program(
                        Client::shared(),
                        &cache,
                        account,
                        Endpoint::CedarEmber,
                        ReadPolicy::Cached,
                        at,
                    )
                    .await
                    && let Ok(ref mut merged) = result
                {
                    merged.cedar_ember = cedar.cedar_ember;
                    merged.errors.clear();
                }
                Some(result.map_err(|error| format!("{error:#}")))
            }
        };
        listings.push(ResetListing {
            account: account.name.clone(),
            pending: journal
                .pending(Service::Claude, &account.name)
                .ok()
                .flatten(),
            status,
        });
    }
    listings
}

/// Which reset a claim would use.
#[derive(Debug, Clone, PartialEq)]
pub enum ResetChoice {
    /// The session reset: refills the 5h limit, paid from the weekly limit.
    Session,
    Grant(Box<resets::ResetGrant>),
}

/// A reset checked against a fresh read, to show before asking.
#[derive(Debug, Clone)]
pub struct ResetPlan {
    pub account: Account,
    pub choice: ResetChoice,
    /// Epoch seconds of the read.
    pub at: i64,
}

/// Check that a reset can be claimed on `name` now: the session reset, the
/// named grant, the grant of a pending claim, or the one the server offers
/// next. Resets are scarce and cannot be taken back; nothing is claimed here.
pub async fn reset_plan(
    fleet: &Fleet,
    name: &str,
    grant: Option<&str>,
    session: bool,
) -> Result<ResetPlan> {
    let account = fleet
        .account(name)
        .with_context(|| format!("no configured account named {name}"))?;
    let at = now();
    let pending = Journal::default().pending(Service::Claude, name)?;
    if let Some(ref pending) = pending {
        if session {
            bail!(
                "a claim of grant {} on {name} has no known outcome; resolve it with `tokeman claude reset {name}` or `tokeman claude reset {name} --clear` before claiming a session reset",
                pending.item.as_deref().unwrap_or("unknown")
            );
        }
        if let Some(requested) = grant
            && pending.item.as_deref() != Some(requested)
        {
            bail!(
                "a claim of grant {} on {name} has no known outcome; {requested} cannot be claimed until it is answered or cleared with `tokeman claude reset {name} --clear`",
                pending.item.as_deref().unwrap_or("unknown")
            );
        }
    }
    let endpoint = if session {
        Endpoint::AtWall
    } else {
        Endpoint::CedarEmber
    };
    let status = resets::program(
        Client::shared(),
        &Cache::default(),
        account,
        endpoint,
        ReadPolicy::Plan,
        at,
    )
    .await?;
    let choice = if session {
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
        ResetChoice::Session
    } else {
        let pending_item = pending.and_then(|pending| pending.item);
        let program = status.cedar_ember.as_ref();
        let wanted = grant
            .map(str::to_owned)
            .or_else(|| pending_item.clone())
            .or_else(|| program.and_then(|p| p.next_grant_id.clone()));
        let retrying_pending = pending_item.is_some() && pending_item == wanted;
        if !retrying_pending {
            let program = program.context("limit resets are not offered to this account")?;
            if !program.eligible {
                bail!(
                    "not eligible for limit resets ({})",
                    program
                        .ineligible_reason
                        .as_deref()
                        .unwrap_or("no reason given")
                );
            }
        }
        let chosen = match program.and_then(|program| {
            program
                .grants()
                .into_iter()
                .find(|candidate| Some(&candidate.id) == wanted.as_ref())
        }) {
            Some(chosen) => chosen,
            None if retrying_pending => {
                let id = wanted.expect("pending_item was Some");
                resets::ResetGrant {
                    id,
                    label: String::new(),
                    kind: None,
                    resets_total: 0,
                    resets_left: 0,
                    starts_at: None,
                    ends_at: None,
                    clears: Vec::new(),
                    gives_back_weekly: false,
                    paused: false,
                    usable_now: false,
                    use_requires_limit: true,
                    unavailable_reason: None,
                    blocking: Vec::new(),
                    percent_used: std::collections::BTreeMap::new(),
                }
            }
            None => bail!("no such grant on offer; see `tokeman claude reset`"),
        };
        if !retrying_pending {
            if !chosen.is_usable(at) {
                bail!(
                    "that grant cannot be used: {}",
                    resets::describe_grant(&chosen, at)
                );
            }
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
        }
        ResetChoice::Grant(Box::new(chosen))
    };
    Ok(ResetPlan {
        account: account.clone(),
        choice,
        at,
    })
}

/// Claim the reset `plan` describes.
pub async fn reset_claim(plan: &ResetPlan) -> Result<resets::ClaimResponse> {
    let grant_id = match &plan.choice {
        ResetChoice::Session => None,
        ResetChoice::Grant(grant) => Some(grant.id.as_str()),
    };
    resets::claim(
        Client::shared(),
        &plan.account,
        grant_id,
        &Journal::default(),
        &Cache::default(),
        now(),
    )
    .await
}

/// Forget `name`'s pending grant claim; the claim that was forgotten.
pub fn reset_clear(name: &str) -> Result<Option<Pending>> {
    Journal::default().clear(Service::Claude, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{HOUR_MS, login_account};

    #[test]
    fn login_walks_the_named_account_or_those_without_a_login() {
        let fleet = Fleet {
            accounts: vec![
                login_account("logged-in", 8 * HOUR_MS),
                Account::new("setup-a"),
                Account::new("setup-b"),
            ],
            ..Fleet::default()
        };
        assert_eq!(login_targets(&fleet, None, false), ["setup-a"]);
        assert_eq!(login_targets(&fleet, None, true), ["setup-a", "setup-b"]);
        // A name is taken as given, configured or not.
        assert_eq!(
            login_targets(&fleet, Some("logged-in"), true),
            ["logged-in"]
        );
        assert_eq!(login_targets(&fleet, Some("new@x.y"), false), ["new@x.y"]);
        let all_in = Fleet {
            accounts: vec![login_account("a", 8 * HOUR_MS)],
            ..Fleet::default()
        };
        assert!(login_targets(&all_in, None, true).is_empty());
    }
}

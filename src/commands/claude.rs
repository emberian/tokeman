//! `tokeman claude ...`: accounts, the browser login in two halves, grant
//! refresh, and usage-limit resets.

use anyhow::{Context, Result, bail};

use super::now;
use crate::account::{Account, Fleet};
use crate::anthropic::profile::Cache;
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
    let (_, removed) = Fleet::update(|fleet| Ok(fleet.remove_account(name)))?;
    if !removed {
        bail!("Account '{name}' not found");
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
    let count = daemon::refresh::refresh_now(name).await?;
    let accounts = Fleet::load()?
        .accounts
        .into_iter()
        .filter(|account| name.is_none_or(|name| account.name == name))
        .filter(Account::is_refreshable)
        .collect();
    Ok(Refreshed { count, accounts })
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
pub async fn reset_offers(fleet: &Fleet) -> Vec<ResetListing> {
    let at = now();
    let cache = Cache::default();
    let journal = Journal::default();
    let mut listings = Vec::new();
    for account in &fleet.accounts {
        let status = match account.profile_token(at * 1000) {
            None => None,
            Some(_) => Some(
                resets::status(Client::shared(), &cache, account, false, at)
                    .await
                    .map_err(|error| format!("{error:#}")),
            ),
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
    let status = resets::status(Client::shared(), &Cache::default(), account, true, at).await?;
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
        let pending = Journal::default().pending(Service::Claude, name)?;
        let wanted = grant
            .map(str::to_owned)
            .or_else(|| pending.and_then(|pending| pending.item))
            .or_else(|| program.next_grant_id.clone());
        let chosen = program
            .grants()
            .into_iter()
            .find(|candidate| Some(&candidate.id) == wanted.as_ref())
            .context("no such grant on offer; see `tokeman claude reset`")?;
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

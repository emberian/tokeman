//! Keeping a pushed seat alive.
//!
//! What codex 0.167.0 does with a token handed to `account/login/start`
//! (`chatgptAuthTokens`), from its source:
//!
//! - `app-server/src/request_processors/account_processor.rs`
//!   (`login_chatgpt_auth_tokens_response`) installs an `ExternalAuthBridge`
//!   holding the token in memory; a later login replaces it.
//! - `login/src/auth/manager.rs` (`AuthManager::auth`) refreshes ahead of
//!   expiry only for auth it manages itself. With external auth it never
//!   does: the pushed token is used until the backend answers 401.
//! - On that 401 (`UnauthorizedRecovery`, step `ExternalRefresh`) the bridge
//!   (`app-server/src/external_auth.rs`) sends the server request
//!   `account/chatgptAuthTokens/refresh` to every initialized connection
//!   (`transport.rs`, `OutgoingEnvelope::Broadcast`) and waits ten seconds.
//!   With no client connected nobody answers, and the turn fails.
//!
//! tokeman is not connected between looks, so it does not wait to be asked.
//! On every look the daemon renews each pushed seat whose token expires
//! within [`RENEW_WITHIN_SECS`] through `refresh.rs`, which owns auth.json's
//! single-use refresh token, and pushes the new token with another
//! `account/login/start`. A seat whose Codex stopped answering its limits is
//! checked against the backend and renewed at once when its token is dead.
//! What cannot be renewed is written to the seat file as trouble, which the
//! fleet view turns into an attention item.

use anyhow::Result;
use chrono::Utc;
use std::fmt;

use super::Client;
use super::appserver::{self, AppServer};
use super::auth::{CodexHome, IdClaims};
use super::probe;
use super::refresh::{self, DEFAULT_MAX_AGE_DAYS, RefreshError};
use super::seats::{Cause, Pushed, Seats, Trouble};
use crate::account::{CodexAccount, Fleet};
use crate::history::Fingerprint;
use crate::paths::expand_tilde;
use crate::quota::ReadError;

/// A pushed token is renewed once it has this long left. Codex access tokens
/// live ten days (240 h between `iat` and `exp`, read 2026-11-11); a day is
/// 288 looks, and leaves the user most of a day to act on a renewal that
/// was refused.
pub const RENEW_WITHIN_SECS: i64 = 24 * 3_600;

/// Epoch seconds when `access_token` stops working, when it is a JWT that
/// says.
pub fn expiry(access_token: &str) -> Option<i64> {
    IdClaims::decode(access_token).and_then(|claims| claims.expires_at)
}

/// Whether a token expiring at `expires_at` is due for renewal at `now`.
pub fn due(expires_at: Option<i64>, now: i64) -> bool {
    expires_at.is_some_and(|at| at - now <= RENEW_WITHIN_SECS)
}

/// Make the bundle in `home` one worth pushing: renewed when its access
/// token is due, when `force`d, or when it is old enough that the profile's
/// own schedule would renew it. `Ok(true)` when a new bundle was written.
pub async fn freshen(
    client: &Client,
    home: &CodexHome,
    force: bool,
    now: i64,
) -> Result<bool, RefreshError> {
    let expiring = home
        .read()
        .ok()
        .and_then(|auth| auth.access_token().map(expiry))
        .is_some_and(|expires_at| due(expires_at, now));
    refresh::refresh(
        client,
        home,
        force || expiring,
        DEFAULT_MAX_AGE_DAYS,
        Utc::now(),
    )
    .await
    .map(|bundle| bundle.is_some())
}

/// What one look did for one pushed seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tended {
    /// The token has time left and the Codex still holds it.
    Fine,
    /// A new token was pushed; it expires at `expires_at`.
    Renewed { expires_at: Option<i64> },
    /// No Codex is on that seat any more (no socket, nothing listening, or
    /// it is on another account): the seat was forgotten. Nothing was sent.
    Ended,
    /// It could not be renewed; the seat file says why.
    Troubled(Trouble),
}

/// One look at one pushed seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tend {
    pub home: String,
    pub account: String,
    pub outcome: Tended,
}

impl Tend {
    /// A word for the outcome, for telling one look from the next.
    pub fn kind(&self) -> &'static str {
        match &self.outcome {
            Tended::Fine => "fine",
            Tended::Renewed { .. } => "renewed",
            Tended::Ended => "ended",
            Tended::Troubled(trouble) => trouble.cause.name(),
        }
    }
}

impl fmt::Display for Tend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seat = format!("codex {}: seat on {}", self.home, self.account);
        match &self.outcome {
            Tended::Fine => write!(f, "{seat} is current"),
            Tended::Renewed { expires_at } => {
                write!(f, "{seat} renewed")?;
                match expires_at.and_then(|at| chrono::DateTime::from_timestamp(at, 0)) {
                    Some(at) => write!(
                        f,
                        "; the new token lasts until {}",
                        at.format("%Y-%m-%d %H:%MZ")
                    ),
                    None => Ok(()),
                }
            }
            Tended::Ended => write!(f, "{seat} ended; forgotten"),
            Tended::Troubled(trouble) => write!(
                f,
                "ERROR {seat} could not be renewed ({}): {}",
                trouble.cause.name(),
                trouble.message
            ),
        }
    }
}

/// Whether a failed connect means nothing listens there (the Codex is
/// gone), as opposed to a socket that is slow or broken.
fn nobody_listens(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        })
}

/// Tend one pushed seat: forget it when its Codex is gone or on another
/// account, renew and push when its token is due or dead, and record what
/// stood in the way. Only the seat file and (through `refresh.rs`) the
/// profile's auth.json are written.
pub async fn tend(client: &Client, fleet: &Fleet, seats: &Seats, seat: &Pushed, now: i64) -> Tend {
    let outcome = match tend_seat(client, fleet, seat, now).await {
        Ok(Step::Fine) => {
            if seat.trouble.is_some() {
                let _ = seats.trouble(seat, None);
            }
            Tended::Fine
        }
        Ok(Step::Ended) => {
            let _ = seats.forget(&seat.home);
            Tended::Ended
        }
        Ok(Step::Pushed(renewed)) => {
            let expires_at = renewed.expires_at;
            match seats.put(renewed) {
                Ok(()) => Tended::Renewed { expires_at },
                Err(error) => Tended::Troubled(Trouble {
                    cause: Cause::Unknown,
                    at: now,
                    message: format!("renewed, but the seat file was not written: {error:#}"),
                }),
            }
        }
        Err((cause, message)) => {
            let trouble = Trouble {
                cause,
                at: now,
                message,
            };
            let _ = seats.trouble(seat, Some(trouble.clone()));
            Tended::Troubled(trouble)
        }
    };
    Tend {
        home: seat.home.clone(),
        account: seat.account.clone(),
        outcome,
    }
}

enum Step {
    Fine,
    Ended,
    Pushed(Pushed),
}

async fn tend_seat(
    client: &Client,
    fleet: &Fleet,
    seat: &Pushed,
    now: i64,
) -> Result<Step, (Cause, String)> {
    let home = CodexHome::new(expand_tilde(&seat.home));
    let socket = appserver::control_socket_path(home.path());
    if !socket.exists() {
        return Ok(Step::Ended);
    }
    let mut server = match AppServer::connect(&socket).await {
        Ok(server) => server,
        Err(error) if nobody_listens(&error) => return Ok(Step::Ended),
        Err(error) => return Err((Cause::Unanswered, format!("{error:#}"))),
    };
    let step = tend_connected(client, fleet, seat, &mut server, now).await;
    server.close().await;
    step
}

async fn tend_connected(
    client: &Client,
    fleet: &Fleet,
    seat: &Pushed,
    server: &mut AppServer,
    now: i64,
) -> Result<Step, (Cause, String)> {
    let info = server
        .read_account()
        .await
        .map_err(|error| (Cause::Unanswered, format!("{error:#}")))?;
    let limits = server.rate_limits().await;
    // On another account, the token tokeman pushed is no longer in use.
    let moved_by_id = limits
        .as_ref()
        .ok()
        .and_then(|limits| limits.account_id.as_ref())
        .is_some_and(|id| *id != seat.account_id);
    let moved_by_email = info
        .email
        .as_ref()
        .zip(seat.email.as_ref())
        .is_some_and(|(now_on, pushed)| now_on != pushed);
    if moved_by_id || moved_by_email {
        return Ok(Step::Ended);
    }
    let account: &CodexAccount = fleet.codex_account(&seat.account).ok_or_else(|| {
        (
            Cause::ProfileGone,
            format!("no codex account named {} is registered", seat.account),
        )
    })?;
    let profile = CodexHome::new(account.home());
    // A Codex that cannot read its limits may be holding a dead token: ask
    // the backend with the profile's own.
    let dead = limits.is_err()
        && !due(seat.expires_at, now)
        && probe::probe(client, account)
            .await
            .error
            .as_ref()
            .is_some_and(ReadError::is_unauthorized);
    if due(seat.expires_at, now) || dead {
        freshen(client, &profile, dead, now)
            .await
            .map_err(|error| {
                let cause = if error.failure.is_permanent() {
                    Cause::Refused
                } else {
                    Cause::RefreshFailed
                };
                (cause, error.to_string())
            })?;
    }
    let auth = profile
        .read()
        .map_err(|error| (Cause::ProfileGone, format!("{error:#}")))?;
    let (Some(access_token), Some(account_id)) = (auth.access_token(), auth.effective_account_id())
    else {
        return Err((
            Cause::ProfileGone,
            format!("{} holds no token to push", seat.account),
        ));
    };
    let token = Fingerprint::of(access_token);
    if token == seat.token {
        return match due(seat.expires_at, now) {
            // Renewal left the same token in place: nothing newer exists.
            true => Err((
                Cause::RefreshFailed,
                "the profile's token was not replaced".to_owned(),
            )),
            false => Ok(Step::Fine),
        };
    }
    let claims = auth.claims();
    let plan = claims
        .as_ref()
        .and_then(|claims| claims.chatgpt_plan_type.clone());
    server
        .reseat(access_token, &account_id, plan.as_deref())
        .await
        .map_err(|error| (Cause::PushFailed, format!("{error:#}")))?;
    Ok(Step::Pushed(Pushed {
        account_id,
        email: claims
            .and_then(|claims| claims.email)
            .or(seat.email.clone()),
        token,
        expires_at: expiry(access_token),
        pushed_at: now,
        trouble: None,
        ..seat.clone()
    }))
}

/// Tend every seat in the file, in order.
pub async fn tend_all(client: &Client, fleet: &Fleet, seats: &Seats, now: i64) -> Vec<Tend> {
    let mut tended = Vec::new();
    for seat in seats.load() {
        tended.push(tend(client, fleet, seats, &seat, now).await);
    }
    tended
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codex::appserver::tests::{FakeAppServer, limits_answer};
    use crate::codex::auth::TokenBundle;
    use crate::codex::auth::tests::{bundle, id_token, jwt};
    use crate::codex::mover::By;
    use crate::codex::probe::tests::Backend;
    use crate::paths::tilde;
    use crate::testing::codex_account;
    use axum::http::StatusCode;
    use serde_json::json;
    use std::path::Path;

    pub(crate) const NOW: i64 = 1_794_000_000;
    const HOUR: i64 = 3_600;

    /// An access token for `name` that expires at `exp`.
    pub(crate) fn access(name: &str, exp: i64) -> String {
        jwt(json!({
            "exp": exp,
            "sub": format!("{name}-{exp}"),
            "https://api.openai.com/auth": {"chatgpt_account_id": format!("acct-{name}")},
        }))
    }

    /// A registered profile whose access token expires at `exp`.
    pub(crate) fn profile_expiring(dir: &Path, name: &str, exp: i64) -> CodexAccount {
        let home = CodexHome::new(dir.join(name));
        home.write_tokens(&TokenBundle {
            id_token: Some(id_token(
                &format!("{name}@x.y"),
                &format!("acct-{name}"),
                "plus",
            )),
            ..bundle(&access(name, exp), "r-old")
        })
        .unwrap();
        codex_account(name, &home.path().display().to_string())
    }

    /// The seat file entry of `account`'s current token pushed to `home`.
    pub(crate) fn pushed_from(home: &Path, account: &CodexAccount) -> Pushed {
        let auth = CodexHome::new(account.home()).read().unwrap();
        let token = auth.access_token().unwrap();
        Pushed {
            home: tilde(home),
            account: account.name.clone(),
            account_id: auth.effective_account_id().unwrap(),
            email: auth.claims().and_then(|claims| claims.email),
            token: Fingerprint::of(token),
            expires_at: expiry(token),
            pushed_at: NOW - 9 * 24 * HOUR,
            by: By::Daemon,
            trouble: None,
        }
    }

    fn renewal(name: &str, exp: i64) -> (StatusCode, String) {
        (
            StatusCode::OK,
            json!({
                "access_token": access(name, exp),
                "refresh_token": "r-new",
                "id_token": id_token(&format!("{name}@x.y"), &format!("acct-{name}"), "plus"),
            })
            .to_string(),
        )
    }

    /// A Codex running on b's account, and the seat file saying tokeman
    /// pushed b's token (expiring `left` seconds from [`NOW`]) to it.
    fn seated(dir: &Path, left: i64) -> (FakeAppServer, Fleet, Seats, Pushed) {
        let fake = FakeAppServer::start(vec![("acct-b", "b@x.y", limits_answer(10.0, 10.0))]);
        let account = profile_expiring(dir, "b", NOW + left);
        let seats = Seats::at(dir.join("codex-seats.json"));
        let seat = pushed_from(fake.home.path(), &account);
        seats.put(seat.clone()).unwrap();
        let fleet = Fleet {
            codex: vec![account],
            ..Fleet::default()
        };
        (fake, fleet, seats, seat)
    }

    #[tokio::test]
    async fn a_token_near_expiry_is_renewed_through_the_profile_and_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let (fake, fleet, seats, seat) = seated(dir.path(), 23 * HOUR);
        let later = NOW + 240 * HOUR;
        let backend = Backend::default().token_answers(vec![renewal("b", later)]);
        let client = backend.client().await;

        let tended = tend(&client, &fleet, &seats, &seat, NOW).await;
        assert_eq!(
            tended.outcome,
            Tended::Renewed {
                expires_at: Some(later)
            }
        );
        // The profile's single-use refresh token was redeemed once, through
        // refresh.rs, and the bundle it returned is on disk.
        let sent = backend.requests("/token");
        assert_eq!(sent.len(), 1);
        assert!(sent[0].body.contains("r-old"));
        let auth = CodexHome::new(fleet.codex[0].home()).read().unwrap();
        assert_eq!(auth.refresh_token(), Some("r-new"));
        assert_eq!(auth.access_token(), Some(access("b", later).as_str()));
        // The running Codex was handed exactly that token.
        assert_eq!(
            fake.logins(),
            vec![json!({
                "type": "chatgptAuthTokens",
                "accessToken": access("b", later),
                "chatgptAccountId": "acct-b",
                "chatgptPlanType": "plus",
            })]
        );
        let known = seats.on(&seat.home).unwrap();
        assert_eq!(known.expires_at, Some(later));
        assert_eq!(known.token, Fingerprint::of(&access("b", later)));
        assert_eq!(known.pushed_at, NOW);
        assert_eq!(known.trouble, None);

        // The next look finds it current: no refresh, no push.
        let again = tend(&client, &fleet, &seats, &known, NOW + 300).await;
        assert_eq!(again.outcome, Tended::Fine);
        assert_eq!(backend.requests("/token").len(), 1);
        assert_eq!(fake.logins().len(), 1);
    }

    #[tokio::test]
    async fn a_token_with_time_left_is_left_alone_and_a_newer_one_on_disk_is_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let (fake, fleet, seats, seat) = seated(dir.path(), 25 * HOUR);
        let backend = Backend::default();
        let client = backend.client().await;
        let tended = tend(&client, &fleet, &seats, &seat, NOW).await;
        assert_eq!(tended.outcome, Tended::Fine);
        assert!(backend.requests("/token").is_empty());
        assert!(backend.requests("/usage").is_empty());
        assert!(fake.logins().is_empty());

        // The profile was renewed by someone else (the half-hour sweep, its
        // own Codex): the running Codex gets the token now on disk.
        let later = NOW + 200 * HOUR;
        CodexHome::new(fleet.codex[0].home())
            .write_tokens(&TokenBundle {
                id_token: Some(id_token("b@x.y", "acct-b", "plus")),
                ..bundle(&access("b", later), "r-2")
            })
            .unwrap();
        let tended = tend(&client, &fleet, &seats, &seat, NOW).await;
        assert_eq!(
            tended.outcome,
            Tended::Renewed {
                expires_at: Some(later)
            }
        );
        assert!(backend.requests("/token").is_empty());
        assert_eq!(fake.logins()[0]["accessToken"], access("b", later));
    }

    #[tokio::test]
    async fn a_refused_refresh_is_recorded_as_trouble_and_nothing_is_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let (fake, fleet, seats, seat) = seated(dir.path(), 2 * HOUR);
        let backend = Backend::default().token_answers(vec![(
            StatusCode::BAD_REQUEST,
            r#"{"error":"refresh_token_expired"}"#.into(),
        )]);
        let client = backend.client().await;
        let tended = tend(&client, &fleet, &seats, &seat, NOW).await;
        let Tended::Troubled(trouble) = &tended.outcome else {
            panic!("{tended}");
        };
        assert_eq!(trouble.cause, Cause::Refused);
        assert_eq!(trouble.at, NOW);
        assert!(fake.logins().is_empty());
        assert_eq!(tended.kind(), "refused");
        assert!(tended.to_string().starts_with("ERROR codex "), "{tended}");
        // The seat stays, with its expiry and why it was not renewed.
        let known = seats.on(&seat.home).unwrap();
        assert_eq!(known.expires_at, seat.expires_at);
        assert_eq!(known.trouble.as_ref().unwrap().cause, Cause::Refused);

        // A transient failure is told apart, and a later success clears it.
        let backend = Backend::default().token_answers(vec![
            (StatusCode::BAD_GATEWAY, "upstream".into()),
            renewal("b", NOW + 240 * HOUR),
        ]);
        let client = backend.client().await;
        let tended = tend(&client, &fleet, &seats, &known, NOW).await;
        assert_eq!(tended.kind(), "refresh-failed");
        let known = seats.on(&seat.home).unwrap();
        let tended = tend(&client, &fleet, &seats, &known, NOW + 300).await;
        assert!(matches!(tended.outcome, Tended::Renewed { .. }), "{tended}");
        assert_eq!(seats.on(&seat.home).unwrap().trouble, None);
        assert_eq!(fake.logins().len(), 1);
    }

    #[tokio::test]
    async fn no_socket_or_another_account_ends_the_seat_and_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::default().token_answers(vec![renewal("b", NOW + 240 * HOUR)]);
        let client = backend.client().await;

        // No socket: the Codex is gone. Even a due token is not renewed.
        let account = profile_expiring(dir.path(), "b", NOW + HOUR);
        let fleet = Fleet {
            codex: vec![account.clone()],
            ..Fleet::default()
        };
        let seats = Seats::at(dir.path().join("codex-seats.json"));
        let empty = tempfile::Builder::new()
            .prefix("tk")
            .tempdir_in("/tmp")
            .unwrap();
        let gone = pushed_from(empty.path(), &account);
        seats.put(gone.clone()).unwrap();
        let tended = tend(&client, &fleet, &seats, &gone, NOW).await;
        assert_eq!(tended.outcome, Tended::Ended);
        assert_eq!(seats.load(), []);
        assert!(backend.requests("/token").is_empty());

        // A socket file nothing listens behind is the same.
        let socket = appserver::control_socket_path(empty.path());
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        seats.put(gone.clone()).unwrap();
        let tended = tend(&client, &fleet, &seats, &gone, NOW).await;
        assert_eq!(tended.outcome, Tended::Ended);
        assert!(backend.requests("/token").is_empty());

        // A Codex that has since moved to another account is not b's seat.
        let fake = FakeAppServer::start(vec![("acct-z", "z@x.y", limits_answer(10.0, 10.0))]);
        let stale = pushed_from(fake.home.path(), &account);
        seats.put(stale.clone()).unwrap();
        let tended = tend(&client, &fleet, &seats, &stale, NOW).await;
        assert_eq!(tended.outcome, Tended::Ended);
        assert_eq!(seats.load(), []);
        assert!(fake.logins().is_empty());
        assert!(backend.requests("/token").is_empty());
    }

    #[tokio::test]
    async fn a_token_the_backend_rejects_early_is_renewed_at_the_next_look() {
        use crate::codex::probe::tests::{ok, status, usage_body};
        let dir = tempfile::tempdir().unwrap();
        let (fake, fleet, seats, seat) = seated(dir.path(), 100 * HOUR);
        fake.running.lock().unwrap().limits_fail = true;
        let later = NOW + 240 * HOUR;

        // The Codex cannot read its limits, but the backend takes the
        // profile's token: whatever is wrong, the token is not, and a
        // single-use refresh token is not spent on it.
        let backend = Backend::default()
            .usage_answers(vec![ok(usage_body(10.0, 10.0))])
            .token_answers(vec![renewal("b", later)]);
        let client = backend.client().await;
        let tended = tend(&client, &fleet, &seats, &seat, NOW).await;
        assert_eq!(tended.outcome, Tended::Fine);
        assert!(backend.requests("/token").is_empty());
        assert!(fake.logins().is_empty());

        // The backend rejects it too: renewed now, days before its expiry.
        let backend = Backend::default()
            .usage_answers(vec![status(StatusCode::UNAUTHORIZED, "{}")])
            .token_answers(vec![renewal("b", later)]);
        let client = backend.client().await;
        let tended = tend(&client, &fleet, &seats, &seat, NOW).await;
        assert_eq!(
            tended.outcome,
            Tended::Renewed {
                expires_at: Some(later)
            }
        );
        assert_eq!(backend.requests("/token").len(), 1);
        assert_eq!(fake.logins()[0]["accessToken"], access("b", later));
    }

    #[tokio::test]
    async fn a_seat_whose_profile_left_the_fleet_is_trouble() {
        let dir = tempfile::tempdir().unwrap();
        let (fake, mut fleet, seats, seat) = seated(dir.path(), 100 * HOUR);
        fleet.codex.clear();
        let client = Backend::default().client().await;
        let tended = tend_all(&client, &fleet, &seats, NOW).await;
        assert_eq!(tended.len(), 1);
        assert_eq!(tended[0].kind(), "profile-gone");
        assert!(fake.logins().is_empty());
        assert_eq!(
            seats.on(&seat.home).unwrap().trouble.unwrap().cause,
            Cause::ProfileGone
        );
    }

    #[test]
    fn a_token_is_due_a_day_before_it_expires() {
        assert!(!due(Some(NOW + RENEW_WITHIN_SECS + 1), NOW));
        assert!(due(Some(NOW + RENEW_WITHIN_SECS), NOW));
        assert!(due(Some(NOW - 5), NOW));
        // A token that does not say when it expires is never forced.
        assert!(!due(None, NOW));
        assert_eq!(expiry(&access("a", 42)), Some(42));
        assert_eq!(expiry("opaque"), None);
    }
}

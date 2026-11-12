//! Moving a running Codex off an account that is running out.
//!
//! A running Codex is asked over its control socket what its account-wide
//! windows hold. While they are above the leaving floors nothing else
//! happens (Codex itself fetches its usage to answer; tokeman sends
//! nothing). Below them the other profiles
//! are probed, and the best one above the landing floors is announced to the
//! app-server; the swap lands between turns. `tokeman codex watch` runs this
//! in the foreground on each push from Codex, the daemon on [`INTERVAL`].
//!
//! Nothing here spends a reset credit.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Client;
use super::appserver::{self, AccountInfo, AppServer, ReportedLimits};
use super::auth::CodexHome;
use super::keep;
use super::probe;
use super::refresh::RefreshError;
use super::seats::{Pushed, Seats};
use crate::account::{CodexAccount, Fleet};
use crate::fs::{append_line, read_if_present, write_atomic};
use crate::history::Fingerprint;
use crate::paths::{self, tilde};
use crate::policy::{self, Floors, Relocation};
use crate::quota::{CodexDetail, Detail, Limit, Reading, Scope, Service, Source, Span, Window};

/// How often the daemon looks at each running Codex. A look at a healthy one
/// is a local socket round trip; the windows it watches last hours.
pub const INTERVAL: Duration = Duration::from_secs(5 * 60);
/// One look at every home never runs longer than this.
pub const SWEEP_TIMEOUT: Duration = Duration::from_secs(120);

const COMPACT_ABOVE_BYTES: u64 = 256 * 1024;
const KEPT_MOVES: usize = 200;

/// Where a running Codex leaves, and what a landing must offer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveFloors {
    pub leave: Floors,
    pub land: Floors,
}

impl MoveFloors {
    /// The `[rotation]` Codex floors.
    pub fn of(fleet: &Fleet) -> Self {
        Self {
            leave: fleet.policy.codex_floors(),
            land: fleet.policy.codex_landing_floors(),
        }
    }

    /// One leaving floor for every window; a landing offers at least that.
    pub fn leaving_at(self, min_remaining: f64) -> Self {
        Self {
            leave: Floors {
                session: min_remaining,
                weekly: min_remaining,
            },
            land: Floors {
                session: self.land.session.max(min_remaining),
                weekly: self.land.weekly.max(min_remaining),
            },
        }
    }
}

/// What a look moves by: the floors, the seat file a push is recorded in,
/// and who is asking.
#[derive(Debug, Clone)]
pub struct Terms {
    pub floors: MoveFloors,
    pub seats: Seats,
    pub by: By,
}

impl Terms {
    /// The `[rotation]` floors and the live seat file.
    pub fn of(fleet: &Fleet, by: By) -> Self {
        Self {
            floors: MoveFloors::of(fleet),
            seats: Seats::default(),
            by,
        }
    }
}

/// What a running Codex says about the account it is on.
#[derive(Debug, Clone, PartialEq)]
pub struct Seat {
    /// Its account-wide windows, as a reading the policy can judge.
    pub reading: Reading,
    pub info: Option<AccountInfo>,
    /// The backend's id for the account, when the limits carried it.
    pub account_id: Option<String>,
}

impl Seat {
    fn from_reported(limits: &ReportedLimits, info: Option<AccountInfo>, at: i64) -> Self {
        let name = info
            .as_ref()
            .and_then(|info| info.email.clone())
            .unwrap_or_default();
        let reading = Reading {
            limits: limits
                .windows
                .iter()
                .map(|window| {
                    Limit::new(
                        window.span,
                        Scope::All,
                        Window::new(window.used, window.resets_at),
                        Source::Poll,
                    )
                })
                .collect(),
            detail: Detail::Codex(CodexDetail {
                email: info.as_ref().and_then(|info| info.email.clone()),
                allowed: Some(!limits.refused),
                ..CodexDetail::default()
            }),
            ..Reading::empty(Service::Codex, &name, at)
        };
        Self {
            reading,
            info,
            account_id: limits.account_id.clone(),
        }
    }

    /// Ask a connected app-server for its limits and its account.
    pub async fn read(server: &mut AppServer) -> Result<Self> {
        let limits = server.rate_limits().await?;
        let info = server.read_account().await.ok();
        Ok(Self::from_reported(&limits, info, Utc::now().timestamp()))
    }

    /// Connect to `socket`, ask, and hang up. Every step is bounded.
    pub async fn read_at(socket: &Path) -> Result<Self> {
        let mut server = AppServer::connect(socket).await?;
        let seat = Self::read(&mut server).await;
        server.close().await;
        seat
    }

    /// Whether `account`'s profile holds the account this Codex is on: by
    /// the backend's account id when both sides have one, else by email.
    fn is_on(&self, account: &CodexAccount) -> bool {
        let Ok(auth) = CodexHome::new(account.home()).read() else {
            return false;
        };
        match (&self.account_id, auth.effective_account_id()) {
            (Some(seated), Some(held)) => *seated == held,
            _ => {
                let email = auth.claims().and_then(|claims| claims.email);
                email.is_some() && email == self.info.as_ref().and_then(|info| info.email.clone())
            }
        }
    }

    /// The registered name of the account this Codex is on, else its email.
    fn name(&self, fleet: &Fleet) -> Option<String> {
        fleet
            .codex
            .iter()
            .find(|account| self.is_on(account))
            .map(|account| account.name.clone())
            .or_else(|| self.info.as_ref().and_then(|info| info.email.clone()))
    }
}

/// Who asked for a move. The serde names are the on-disk strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum By {
    /// The background service's own look.
    Daemon,
    /// `tokeman codex watch`.
    Watch,
    /// `tokeman codex use`.
    Use,
}

impl By {
    pub fn label(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Watch => "watch",
            Self::Use => "use",
        }
    }
}

/// One move of a running Codex, as `codex-moves.jsonl` keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Move {
    /// Epoch seconds.
    pub at: i64,
    /// The CODEX_HOME whose Codex moved, `~`-relative.
    pub home: String,
    /// The account it was on, when it could be named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
    /// What was left in the window that made it leave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    pub by: By,
}

impl fmt::Display for Move {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "codex: moved {} from {} to {}",
            self.home,
            self.from.as_deref().unwrap_or("an unregistered account"),
            self.to
        )?;
        if let (Some(remaining), Some(span)) = (self.remaining, self.span) {
            write!(
                f,
                " ({:.0}% left in its {} window)",
                remaining * 100.0,
                span.label()
            )?;
        }
        write!(f, " [{}]", self.by.label())
    }
}

/// Append `moved` to the move history; this module is its only writer.
pub fn record(moved: &Move) -> Result<()> {
    record_at(&paths::codex_moves(), moved)
}

/// [`record`], into the history at `path`.
pub fn record_at(path: &Path, moved: &Move) -> Result<()> {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > COMPACT_ABOVE_BYTES) {
        let kept = moves_at(path);
        let from = kept.len().saturating_sub(KEPT_MOVES);
        let mut text = String::new();
        for kept in &kept[from..] {
            text.push_str(&serde_json::to_string(kept)?);
            text.push('\n');
        }
        write_atomic(path, text.as_bytes(), 0o600)?;
    }
    append_line(path, &serde_json::to_string(moved)?)
}

/// Every recorded move, oldest first. A line this build does not read is
/// left out.
pub fn moves() -> Vec<Move> {
    moves_at(&paths::codex_moves())
}

/// [`moves`], from the history at `path`.
pub fn moves_at(path: &Path) -> Vec<Move> {
    read_if_present(path)
        .ok()
        .flatten()
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// What announcing an account to a running Codex did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announced {
    /// Refreshing the account first failed; its stored token was sent as is.
    pub refresh_error: Option<RefreshError>,
    /// The push could not be written to the seat file, so the daemon will
    /// not renew this token.
    pub record_error: Option<String>,
    /// What the running Codex reported before and after. A successful RPC
    /// only means the request was accepted; `after` is the proof.
    pub before: Option<AccountInfo>,
    pub after: Option<AccountInfo>,
}

/// Put the Codex running on `home` onto `account`: renew the account's
/// bundle when it is due ([`keep::freshen`]; the app-server takes the token
/// as is and never renews it), hand its token to the app-server, and record
/// the push in the seat file so the daemon keeps it alive.
pub async fn announce(
    client: &Client,
    account: &CodexAccount,
    home: &CodexHome,
    seats: &Seats,
    by: By,
) -> Result<Announced> {
    let now = Utc::now().timestamp();
    let source = CodexHome::new(account.home());
    let refresh_error = keep::freshen(client, &source, false, now).await.err();
    let auth = source.read()?;
    let access_token = auth
        .access_token()
        .with_context(|| format!("{} has no access_token", account.name))?;
    let account_id = auth
        .effective_account_id()
        .with_context(|| format!("{} has no chatgpt_account_id", account.name))?;
    let claims = auth.claims();
    let plan = claims
        .as_ref()
        .and_then(|claims| claims.chatgpt_plan_type.clone());
    let socket = appserver::control_socket_path(home.path());
    let mut server = AppServer::connect(&socket).await?;
    let before = server.read_account().await.ok();
    let sent = server
        .reseat(access_token, &account_id, plan.as_deref())
        .await;
    let after = match &sent {
        Ok(_) => server.read_account().await.ok(),
        Err(_) => None,
    };
    server.close().await;
    sent?;
    // From here the Codex runs on a token it cannot renew; the seat file is
    // what gets it renewed.
    let record_error = seats
        .put(Pushed {
            home: tilde(home.path()),
            account: account.name.clone(),
            account_id,
            email: claims.and_then(|claims| claims.email),
            token: Fingerprint::of(access_token),
            expires_at: keep::expiry(access_token),
            pushed_at: now,
            by,
            trouble: None,
        })
        .err()
        .map(|error| format!("{error:#}"));
    Ok(Announced {
        refresh_error,
        record_error,
        before,
        after,
    })
}

/// What one look at one running Codex found.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// No control socket: no Codex there to move.
    NoSocket,
    /// A socket that did not answer.
    Unreachable {
        message: String,
    },
    /// The running Codex reports no account-wide window.
    NoWindows,
    /// Every window is above its leaving floor.
    Healthy,
    /// Below a leaving floor, and no other profile offers the landing room.
    Draining,
    /// A dry run: it would move.
    WouldMove {
        to: String,
    },
    Moved {
        to: String,
        announced: Announced,
    },
    /// The move was decided and the app-server refused or failed it.
    Failed {
        to: String,
        message: String,
    },
}

/// One look at the Codex running on one CODEX_HOME.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    pub home: PathBuf,
    /// The account it is on: a registered name, else the email it reports.
    pub seated: Option<String>,
    /// What is left in its fullest account-wide window.
    pub tightest: Option<(f64, Span)>,
    /// The other profiles as read for this look; empty unless the seat was
    /// below its floor. The caller keeps them in the history.
    pub probed: Vec<Reading>,
    pub outcome: Outcome,
}

impl Look {
    fn idle(home: &CodexHome, outcome: Outcome) -> Self {
        Self {
            home: home.path().to_path_buf(),
            seated: None,
            tightest: None,
            probed: Vec::new(),
            outcome,
        }
    }

    /// The record of this look's move, when it made one.
    pub fn moved(&self, by: By, at: i64) -> Option<Move> {
        let Outcome::Moved { to, .. } = &self.outcome else {
            return None;
        };
        Some(Move {
            at,
            home: tilde(&self.home),
            from: self.seated.clone(),
            to: to.clone(),
            remaining: self.tightest.map(|(remaining, _)| remaining),
            span: self.tightest.map(|(_, span)| span),
            by,
        })
    }

    /// A word for the outcome, for telling one look from the next.
    pub fn kind(&self) -> &'static str {
        match self.outcome {
            Outcome::NoSocket => "no-socket",
            Outcome::Unreachable { .. } => "unreachable",
            Outcome::NoWindows => "no-windows",
            Outcome::Healthy => "healthy",
            Outcome::Draining => "draining",
            Outcome::WouldMove { .. } => "would-move",
            Outcome::Moved { .. } => "moved",
            Outcome::Failed { .. } => "failed",
        }
    }
}

impl fmt::Display for Look {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let home = tilde(&self.home);
        let seat = match (&self.seated, self.tightest) {
            (Some(seated), Some((remaining, span))) => format!(
                "on {seated}, {:.0}% left in its {} window",
                remaining * 100.0,
                span.label()
            ),
            (None, Some((remaining, span))) => format!(
                "{:.0}% left in its {} window",
                remaining * 100.0,
                span.label()
            ),
            (Some(seated), None) => format!("on {seated}"),
            (None, None) => "account unknown".to_owned(),
        };
        match &self.outcome {
            Outcome::NoSocket => write!(f, "codex {home}: no control socket, no Codex to move"),
            Outcome::Unreachable { message } => {
                write!(f, "codex {home}: control socket did not answer: {message}")
            }
            Outcome::NoWindows => write!(f, "codex {home}: {seat}; it reports no window to judge"),
            Outcome::Healthy => write!(f, "codex {home}: {seat}; staying"),
            Outcome::Draining => write!(
                f,
                "codex {home}: {seat}; below its floor, and no other profile has the landing room"
            ),
            Outcome::WouldMove { to } => write!(f, "codex {home}: {seat}; would move to {to}"),
            Outcome::Moved { to, announced } => {
                let shown = |info: &Option<AccountInfo>| {
                    info.as_ref()
                        .map_or("unknown".to_owned(), ToString::to_string)
                };
                write!(
                    f,
                    "codex {home}: {seat}; moved to {to} ({} -> {})",
                    shown(&announced.before),
                    shown(&announced.after)
                )?;
                if let Some(error) = &announced.refresh_error {
                    write!(f, "; could not refresh {to} first: {error}")?;
                }
                if let Some(error) = &announced.record_error {
                    write!(
                        f,
                        "; the seat was not recorded, so its token will not be renewed: {error}"
                    )?;
                }
                Ok(())
            }
            Outcome::Failed { to, message } => {
                write!(f, "codex {home}: {seat}; moving to {to} failed: {message}")
            }
        }
    }
}

/// The profiles a move may land on: registered, not parked.
pub fn landings(fleet: &Fleet) -> Vec<&CodexAccount> {
    fleet
        .codex
        .iter()
        .filter(|account| !account.parked)
        .collect()
}

/// The CODEX_HOMEs a Codex may be running on: `~/.codex`, then each
/// registered profile's, once each.
pub fn homes(fleet: &Fleet) -> Vec<CodexHome> {
    homes_after(CodexHome::default_home().ok(), fleet)
}

/// [`homes`], with `default` as Codex's own fallback home.
fn homes_after(default: Option<CodexHome>, fleet: &Fleet) -> Vec<CodexHome> {
    let mut homes: Vec<CodexHome> = default.into_iter().collect();
    for account in &fleet.codex {
        let home = CodexHome::new(account.home());
        if !homes.contains(&home) {
            homes.push(home);
        }
    }
    homes
}

/// Judge `seat`, the Codex running on `home`, and move it when the policy
/// says so. The other profiles are probed only when the seat is below its
/// leaving floor. With `dry_run` nothing is announced. Nothing is written
/// here: the caller keeps [`Look::probed`] and records a move.
pub async fn consider(
    client: &Client,
    fleet: &Fleet,
    home: &CodexHome,
    seat: &Seat,
    terms: &Terms,
    dry_run: bool,
) -> Look {
    let floors = terms.floors;
    let mut look = Look {
        home: home.path().to_path_buf(),
        seated: seat.name(fleet),
        tightest: seat
            .reading
            .tightest()
            .map(|limit| (limit.window.remaining().clamp(0.0, 1.0), limit.span)),
        probed: Vec::new(),
        outcome: Outcome::Healthy,
    };
    if seat.reading.limits.is_empty() && seat.reading.allows() {
        look.outcome = Outcome::NoWindows;
        return look;
    }
    if policy::above(&seat.reading, floors.leave, None) {
        return look;
    }
    let others: Vec<CodexAccount> = landings(fleet)
        .into_iter()
        .filter(|account| !seat.is_on(account))
        .cloned()
        .collect();
    look.probed = probe::probe_all(client, &others).await;
    let landing = match policy::relocate(&seat.reading, &look.probed, floors.leave, floors.land) {
        Relocation::Stay => return look,
        Relocation::Drain => {
            look.outcome = Outcome::Draining;
            return look;
        }
        Relocation::Move(to) => to.account.clone(),
    };
    if dry_run {
        look.outcome = Outcome::WouldMove { to: landing };
        return look;
    }
    let account = others
        .iter()
        .find(|account| account.name == landing)
        .expect("the landing is one of the probed profiles");
    look.outcome = match announce(client, account, home, &terms.seats, terms.by).await {
        Ok(announced) => Outcome::Moved {
            to: landing,
            announced,
        },
        Err(error) => Outcome::Failed {
            to: landing,
            message: format!("{error:#}"),
        },
    };
    look
}

/// Look at the Codex on `home`: nothing when no socket is there, else ask it
/// and [`consider`].
pub async fn look_at(
    client: &Client,
    fleet: &Fleet,
    home: &CodexHome,
    terms: &Terms,
    dry_run: bool,
) -> Look {
    let socket = appserver::control_socket_path(home.path());
    if !socket.exists() {
        return Look::idle(home, Outcome::NoSocket);
    }
    match Seat::read_at(&socket).await {
        Ok(seat) => consider(client, fleet, home, &seat, terms, dry_run).await,
        Err(error) => Look::idle(
            home,
            Outcome::Unreachable {
                message: format!("{error:#}"),
            },
        ),
    }
}

/// One look at every home in `homes`, in order.
pub async fn sweep(
    client: &Client,
    fleet: &Fleet,
    homes: &[CodexHome],
    terms: &Terms,
    dry_run: bool,
) -> Vec<Look> {
    let mut looks = Vec::with_capacity(homes.len());
    for home in homes {
        looks.push(look_at(client, fleet, home, terms, dry_run).await);
    }
    looks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::appserver::tests::{FakeAppServer, limits_answer};
    use crate::codex::probe::tests::{Backend, ok, profile, status, usage_body};
    use axum::http::StatusCode;
    use serde_json::json;

    fn fleet_of(accounts: Vec<CodexAccount>) -> Fleet {
        Fleet {
            codex: accounts,
            ..Fleet::default()
        }
    }

    /// The default floors, pushes recorded in a seat file under `dir`.
    fn terms(dir: &Path) -> Terms {
        Terms {
            floors: MoveFloors {
                leave: Floors {
                    session: 0.20,
                    weekly: 0.20,
                },
                land: Floors {
                    session: 0.30,
                    weekly: 0.30,
                },
            },
            seats: Seats::at(dir.join("codex-seats.json")),
            by: By::Daemon,
        }
    }

    /// Three profiles and a Codex running on the first one's account.
    fn running_on_a(dir: &Path, limits: serde_json::Value) -> (FakeAppServer, Fleet) {
        let fleet = fleet_of(vec![
            profile(dir, "a", "r-a"),
            profile(dir, "b", "r-b"),
            profile(dir, "c", "r-c"),
        ]);
        let fake = FakeAppServer::start(vec![
            ("acct-a", "a@x.y", limits),
            ("acct-b", "b@x.y", limits_answer(10.0, 10.0)),
            ("acct-c", "c@x.y", limits_answer(10.0, 10.0)),
        ]);
        (fake, fleet)
    }

    #[tokio::test]
    async fn a_healthy_seat_is_left_alone_and_nothing_is_probed() {
        let dir = tempfile::tempdir().unwrap();
        let terms = terms(dir.path());
        let (fake, fleet) = running_on_a(dir.path(), limits_answer(79.0, 50.0));
        let backend = Backend::default().usage_answers(vec![ok(usage_body(0.0, 0.0))]);
        let client = backend.client().await;
        let home = CodexHome::new(fake.home.path());
        let look = look_at(&client, &fleet, &home, &terms, false).await;
        assert_eq!(look.outcome, Outcome::Healthy);
        assert_eq!(look.seated.as_deref(), Some("a"));
        let (remaining, span) = look.tightest.unwrap();
        assert!((remaining - 0.21).abs() < 1e-9);
        assert_eq!(span, Span::FiveHour);
        assert!(backend.requests("/usage").is_empty());
        assert!(fake.logins().is_empty());
        assert_eq!(look.moved(By::Daemon, 5), None);
    }

    #[tokio::test]
    async fn a_seat_below_its_floor_moves_to_the_best_profile_with_landing_room() {
        let dir = tempfile::tempdir().unwrap();
        let terms = terms(dir.path());
        let (fake, fleet) = running_on_a(dir.path(), limits_answer(85.0, 50.0));
        // b has 25% of its 5h window left: over the leaving floor, under the
        // landing floor. c has room.
        let backend = Backend::default().usage_answers(vec![
            ok(usage_body(75.0, 10.0)),
            ok(usage_body(40.0, 30.0)),
            ok(usage_body(75.0, 10.0)),
            ok(usage_body(40.0, 30.0)),
        ]);
        let client = backend.client().await;
        let home = CodexHome::new(fake.home.path());

        // A dry run decides the same and touches nothing.
        let dry = look_at(&client, &fleet, &home, &terms, true).await;
        assert_eq!(dry.outcome, Outcome::WouldMove { to: "c".into() });
        assert_eq!(dry.probed.len(), 2);
        assert!(fake.logins().is_empty());
        assert_eq!(
            backend.requests("/usage").len(),
            2,
            "the seated account is not probed"
        );

        let look = look_at(&client, &fleet, &home, &terms, false).await;
        let Outcome::Moved { to, announced } = &look.outcome else {
            panic!("{look}");
        };
        assert_eq!(to, "c");
        assert_eq!(
            announced.before.as_ref().unwrap().email.as_deref(),
            Some("a@x.y")
        );
        assert_eq!(
            announced.after.as_ref().unwrap().email.as_deref(),
            Some("c@x.y")
        );
        assert_eq!(announced.refresh_error, None);
        assert_eq!(
            fake.logins(),
            vec![json!({
                "type": "chatgptAuthTokens",
                "accessToken": "access-c",
                "chatgptAccountId": "acct-c",
                "chatgptPlanType": "plus",
            })]
        );
        assert_eq!(fake.account_id(), "acct-c");
        // The push is on record, so the daemon renews that token.
        assert_eq!(announced.record_error, None);
        let pushed = terms.seats.on(&tilde(fake.home.path())).unwrap();
        assert_eq!(pushed.account, "c");
        assert_eq!(pushed.account_id, "acct-c");
        assert_eq!(pushed.token, Fingerprint::of("access-c"));
        assert_eq!(pushed.by, By::Daemon);
        assert!(
            backend.requests("/token").is_empty(),
            "a fresh bundle is not refreshed"
        );
        let moved = look.moved(By::Daemon, 77).unwrap();
        assert_eq!(moved.from.as_deref(), Some("a"));
        assert_eq!(moved.to, "c");
        assert_eq!(moved.span, Some(Span::FiveHour));
        assert_eq!(
            moved.to_string(),
            format!(
                "codex: moved {} from a to c (15% left in its 5h window) [daemon]",
                tilde(fake.home.path())
            )
        );

        // The next look sees c's own windows and stays.
        let after = look_at(&client, &fleet, &home, &terms, false).await;
        assert_eq!(after.outcome, Outcome::Healthy);
        assert_eq!(after.seated.as_deref(), Some("c"));
        assert_eq!(fake.logins().len(), 1);
    }

    #[tokio::test]
    async fn a_low_seat_drains_when_nowhere_has_landing_room_and_a_spent_one_takes_scraps() {
        let dir = tempfile::tempdir().unwrap();
        let terms = terms(dir.path());
        let (fake, mut fleet) = running_on_a(dir.path(), limits_answer(85.0, 50.0));
        // c is parked: only b is a landing, and it has 25% left.
        fleet.codex[2].parked = true;
        let backend = Backend::default().usage_answers(vec![ok(usage_body(75.0, 10.0))]);
        let client = backend.client().await;
        let home = CodexHome::new(fake.home.path());
        let look = look_at(&client, &fleet, &home, &terms, false).await;
        assert_eq!(look.outcome, Outcome::Draining);
        assert!(fake.logins().is_empty());
        assert_eq!(
            backend.requests("/usage").len(),
            1,
            "a parked profile is not a landing"
        );

        // Refused outright, the seat takes b's 25%.
        let mut refused = limits_answer(100.0, 50.0);
        refused["rateLimits"]["rateLimitReachedType"] = json!("rate_limit_reached");
        fake.set_limits(refused);
        let look = look_at(&client, &fleet, &home, &terms, false).await;
        assert!(
            matches!(&look.outcome, Outcome::Moved { to, .. } if to == "b"),
            "{look}"
        );
        assert_eq!(fake.account_id(), "acct-b");
    }

    #[tokio::test]
    async fn a_landing_that_cannot_be_read_is_no_landing_and_a_dead_socket_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let terms = terms(dir.path());
        let (fake, mut fleet) = running_on_a(dir.path(), limits_answer(95.0, 50.0));
        fleet.codex.truncate(2);
        let backend =
            Backend::default().usage_answers(vec![status(StatusCode::UNAUTHORIZED, "{}")]);
        let client = backend.client().await;
        let home = CodexHome::new(fake.home.path());
        let look = look_at(&client, &fleet, &home, &terms, false).await;
        assert_eq!(look.outcome, Outcome::Draining);

        // No socket at all is quiet; a socket nobody answers is said.
        let empty = tempfile::Builder::new()
            .prefix("tk")
            .tempdir_in("/tmp")
            .unwrap();
        let none = look_at(
            &client,
            &fleet,
            &CodexHome::new(empty.path()),
            &terms,
            false,
        )
        .await;
        assert_eq!(none.outcome, Outcome::NoSocket);
        let socket = appserver::control_socket_path(empty.path());
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let dead = look_at(
            &client,
            &fleet,
            &CodexHome::new(empty.path()),
            &terms,
            false,
        )
        .await;
        assert!(
            matches!(dead.outcome, Outcome::Unreachable { .. }),
            "{dead}"
        );
        assert_eq!(dead.kind(), "unreachable");
    }

    #[tokio::test]
    async fn the_same_account_in_another_home_is_not_a_landing() {
        let dir = tempfile::tempdir().unwrap();
        let terms = terms(dir.path());
        let (fake, mut fleet) = running_on_a(dir.path(), limits_answer(95.0, 50.0));
        // `copy` is profile a's login in a second directory.
        let copy = dir.path().join("copy");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::copy(
            CodexHome::new(fleet.codex[0].home()).auth_path(),
            CodexHome::new(&copy).auth_path(),
        )
        .unwrap();
        fleet.codex = vec![
            fleet.codex[0].clone(),
            crate::testing::codex_account("copy", &copy.display().to_string()),
        ];
        let backend = Backend::default().usage_answers(vec![ok(usage_body(0.0, 0.0))]);
        let client = backend.client().await;
        let home = CodexHome::new(fake.home.path());
        let look = look_at(&client, &fleet, &home, &terms, false).await;
        assert_eq!(look.outcome, Outcome::Draining);
        assert!(backend.requests("/usage").is_empty());
        assert!(fake.logins().is_empty());
    }

    #[test]
    fn moves_are_appended_read_back_and_compacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex-moves.jsonl");
        assert!(moves_at(&path).is_empty());
        let moved = |at: i64| Move {
            at,
            home: "~/.codex".into(),
            from: Some("a".into()),
            to: "b".into(),
            remaining: Some(0.12),
            span: Some(Span::Weekly),
            by: By::Watch,
        };
        record_at(&path, &moved(1)).unwrap();
        record_at(&path, &moved(2)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.lines().next().unwrap(),
            r#"{"at":1,"home":"~/.codex","from":"a","to":"b","remaining":0.12,"span":"weekly","by":"watch"}"#
        );
        // A line from another build is skipped, not fatal.
        std::fs::write(&path, format!("{text}{{\"at\":\"soon\"}}\n")).unwrap();
        assert_eq!(moves_at(&path), vec![moved(1), moved(2)]);
        // Past the size bound the oldest lines go.
        let line = serde_json::to_string(&moved(0)).unwrap();
        let many = COMPACT_ABOVE_BYTES as usize / (line.len() + 1) + 2;
        std::fs::write(&path, format!("{line}\n").repeat(many)).unwrap();
        record_at(&path, &moved(9)).unwrap();
        let kept = moves_at(&path);
        assert_eq!(kept.len(), KEPT_MOVES + 1);
        assert_eq!(kept.last().unwrap().at, 9);
    }

    #[test]
    fn one_leaving_floor_raises_the_landing_floor_to_it() {
        let fleet = Fleet::default();
        let floors = MoveFloors::of(&fleet);
        assert_eq!(floors.leave.session, 0.20);
        assert_eq!(floors.land.weekly, 0.30);
        let low = floors.leaving_at(0.05);
        assert_eq!((low.leave.session, low.leave.weekly), (0.05, 0.05));
        assert_eq!(low.land, floors.land);
        let high = floors.leaving_at(0.5);
        assert_eq!((high.land.session, high.land.weekly), (0.5, 0.5));
    }

    #[test]
    fn the_default_home_comes_first_and_each_home_once() {
        let default = CodexHome::new("/homes/me/.codex");
        let fleet = fleet_of(vec![
            crate::testing::codex_account("work", "/homes/work"),
            crate::testing::codex_account("default", "/homes/me/.codex"),
            crate::testing::codex_account("again", "/homes/work"),
        ]);
        assert_eq!(
            homes_after(Some(default.clone()), &fleet),
            vec![default, CodexHome::new("/homes/work")]
        );
        // With no home directory to fall back to, the profiles' alone.
        assert_eq!(
            homes_after(None, &fleet),
            vec![
                CodexHome::new("/homes/work"),
                CodexHome::new("/homes/me/.codex")
            ]
        );
        assert_eq!(landings(&fleet).len(), 3);
    }

    #[tokio::test]
    async fn a_refused_seat_moves_with_room_in_its_windows_and_one_without_windows_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let terms = terms(dir.path());
        // 90% left in both windows, and the backend refuses ordinary work.
        let mut refused = limits_answer(10.0, 10.0);
        refused["rateLimits"]["rateLimitReachedType"] = json!("rate_limit_reached");
        let (fake, fleet) = running_on_a(dir.path(), refused);
        let backend = Backend::default().usage_answers(vec![ok(usage_body(40.0, 30.0))]);
        let client = backend.client().await;
        let home = CodexHome::new(fake.home.path());
        let look = look_at(&client, &fleet, &home, &terms, false).await;
        assert!(matches!(look.outcome, Outcome::Moved { .. }), "{look}");
        assert_eq!(fake.logins().len(), 1);

        // An answer with no account-wide window is nothing to judge: no
        // profile is probed and nothing is announced.
        fake.set_limits(json!({"rateLimits": {}, "ordinaryUsageAllowed": true}));
        let probes = backend.requests("/usage").len();
        let look = look_at(&client, &fleet, &home, &terms, false).await;
        assert_eq!(look.outcome, Outcome::NoWindows);
        assert_eq!(look.kind(), "no-windows");
        assert_eq!(backend.requests("/usage").len(), probes);
        assert_eq!(fake.logins().len(), 1);
    }

    #[test]
    fn a_seat_is_matched_by_account_id_before_email() {
        let dir = tempfile::tempdir().unwrap();
        let a = profile(dir.path(), "a", "r-a");
        let seat = |account_id: Option<&str>, email: &str| {
            Seat::from_reported(
                &ReportedLimits {
                    account_id: account_id.map(str::to_owned),
                    ..ReportedLimits::default()
                },
                Some(AccountInfo {
                    email: Some(email.to_owned()),
                    plan_type: None,
                }),
                0,
            )
        };
        assert!(seat(Some("acct-a"), "renamed@x.y").is_on(&a));
        // One login in two workspaces is two accounts.
        assert!(!seat(Some("acct-other"), "a@x.y").is_on(&a));
        // Without an id from the running Codex, the email decides.
        assert!(seat(None, "a@x.y").is_on(&a));
        assert!(!seat(None, "b@x.y").is_on(&a));
    }
}

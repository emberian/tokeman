//! Read history: one row per reading, in sqlite, for charts, stats and the
//! stored view.
//!
//! Every read of every service is one JSON [`Reading`] in the `readings`
//! table. Rows written by earlier builds stay where they are, in their own
//! tables, and are never rewritten: they are converted as they are read
//! (`legacy`), so one set of accessors serves the whole history.
//! [`Store::open_read_only`] is for surfaces that only show what was stored:
//! it creates no file, table or row.

mod legacy;

use legacy::Rows;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::time::Duration;

use crate::paths;
use crate::quota::{Limit, Reading, Scope, Service, Span, Window};

/// How long an unchanged reading goes without a row of its own.
pub const HOLD_SECS: i64 = 600;

pub struct Store {
    conn: Connection,
}

/// One limit followed through the history: the window of this length over
/// this scope, in every reading that has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Series {
    pub span: Span,
    pub scope: Scope,
}

impl Series {
    pub fn new(span: Span, scope: Scope) -> Self {
        Self { span, scope }
    }

    /// The account-wide 5-hour window.
    pub fn session() -> Self {
        Self::new(Span::FiveHour, Scope::All)
    }

    /// The account-wide weekly window.
    pub fn weekly() -> Self {
        Self::new(Span::Weekly, Scope::All)
    }

    pub fn of(limit: &Limit) -> Self {
        Self::new(limit.span, limit.scope.clone())
    }

    pub fn limit<'a>(&self, reading: &'a Reading) -> Option<&'a Limit> {
        reading.limit(self.span, &self.scope)
    }

    /// This series' window in one reading.
    pub fn window(&self, reading: &Reading) -> Option<Window> {
        reading.window(self.span, &self.scope)
    }

    /// Every series `readings` hold, in the order the newest reading that
    /// has each reports it, with that reading's limit.
    pub fn known(readings: &[Reading]) -> Vec<(Series, &Limit)> {
        let mut known: Vec<(Series, &Limit)> = Vec::new();
        for limit in readings.iter().rev().flat_map(|reading| &reading.limits) {
            if !known
                .iter()
                .any(|(series, _)| series.span == limit.span && series.scope == limit.scope)
            {
                known.push((Series::of(limit), limit));
            }
        }
        known
    }
}

impl Store {
    pub fn open() -> Result<Self> {
        Self::open_at(&paths::snapshots_db())
    }

    pub fn open_at(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        crate::fs::set_mode(path, 0o600)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    /// Open the history without creating, migrating or writing anything. A
    /// table this database does not have reads as no rows.
    pub fn open_read_only() -> Result<Self> {
        Self::open_read_only_at(&paths::snapshots_db())
    }

    pub fn open_read_only_at(path: &std::path::Path) -> Result<Self> {
        use rusqlite::OpenFlags;
        // A reader of a WAL database creates the `-shm` and `-wal` files when
        // they are missing. With no `-wal` file every row is in the main
        // file, so it is read as immutable, which creates nothing; with one,
        // the sidecars exist already and an ordinary read-only open adds none.
        let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = if wal.exists() {
            Connection::open_with_flags(path, flags)
        } else {
            let escaped = path
                .display()
                .to_string()
                .replace('%', "%25")
                .replace('?', "%3f")
                .replace('#', "%23");
            Connection::open_with_flags(
                format!("file:{escaped}?immutable=1"),
                flags | OpenFlags::SQLITE_OPEN_URI,
            )
        }
        .with_context(|| format!("failed to open database at {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { conn })
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS readings (
                id INTEGER PRIMARY KEY,
                service TEXT NOT NULL,
                account TEXT NOT NULL,
                at INTEGER NOT NULL,
                reading TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_readings_account_time
                ON readings(service, account, at);
            CREATE TABLE IF NOT EXISTS confirmed (
                service TEXT NOT NULL,
                account TEXT NOT NULL,
                at INTEGER NOT NULL,
                PRIMARY KEY (service, account)
            ) WITHOUT ROWID;",
        )?;
        Ok(())
    }

    /// Record a read. A reading that says what the account's newest row
    /// says, apart from when, adds no row until that row is [`HOLD_SECS`]
    /// old: the value is held, and `confirmed` keeps when it was last seen.
    /// A reading that differs is written, after a row that closes the hold
    /// it ends (the old value at its last confirmation), so the history
    /// shows how long each value stood and when it moved.
    pub fn insert(&self, reading: &Reading) -> Result<()> {
        let service = reading.service.name();
        // Taken for writing at once: the row to compare with must not change
        // between reading it and writing after it.
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let newest: Option<(i64, String)> = tx
            .query_row(
                "SELECT at, reading FROM readings WHERE service = ?1 AND account = ?2
                 ORDER BY at DESC, id DESC LIMIT 1",
                params![service, reading.account],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let json = serde_json::to_string(reading)?;
        let same = newest
            .as_ref()
            .is_some_and(|(_, stored)| same_but_when(stored, &json));
        let held = same
            && newest
                .as_ref()
                .is_some_and(|(at, _)| reading.at - at < HOLD_SECS);
        if !held {
            if let Some((at, stored)) = newest.as_ref().filter(|_| !same) {
                let confirmed: Option<i64> = tx
                    .query_row(
                        "SELECT at FROM confirmed WHERE service = ?1 AND account = ?2",
                        params![service, reading.account],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(confirmed) = confirmed.filter(|seen| seen > at && *seen < reading.at)
                    && let Ok(mut closing) = serde_json::from_str::<Reading>(stored)
                {
                    closing.at = confirmed;
                    tx.execute(
                        "INSERT INTO readings (service, account, at, reading) VALUES (?1, ?2, ?3, ?4)",
                        params![service, reading.account, confirmed, serde_json::to_string(&closing)?],
                    )?;
                }
            }
            tx.execute(
                "INSERT INTO readings (service, account, at, reading) VALUES (?1, ?2, ?3, ?4)",
                params![service, reading.account, reading.at, json],
            )?;
        }
        tx.execute(
            "INSERT INTO confirmed (service, account, at) VALUES (?1, ?2, ?3)
             ON CONFLICT (service, account) DO UPDATE SET at = MAX(at, excluded.at)",
            params![service, reading.account, reading.at],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The newest reading of each of `accounts`, as of its last
    /// confirmation. An account with a row in `readings` is answered from
    /// there alone; the tables of earlier builds are asked only for one
    /// without.
    pub fn latest(&self, accounts: &[(Service, &str)]) -> Result<Vec<Reading>> {
        let mut latest = Vec::new();
        for (service, account) in accounts {
            let newest = self
                .query(
                    "WHERE service = ?1 AND account = ?2 ORDER BY at DESC, id DESC LIMIT 1",
                    params![service.name(), account],
                )?
                .pop();
            let reading = match newest {
                Some(mut reading) => {
                    if let Some(held) = self.held(*service, Some(account))?.into_iter().next() {
                        reading.at = held.at;
                    }
                    Some(reading)
                }
                None => legacy::read(
                    &self.conn,
                    *service,
                    Rows::Recent {
                        account: Some(account),
                        limit: 1,
                    },
                )?
                .pop(),
            };
            latest.extend(reading);
        }
        Ok(latest)
    }

    /// The newest readings of one service, newest first, optionally of one
    /// account.
    pub fn recent(
        &self,
        service: Service,
        account: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Reading>> {
        let count = limit as i64;
        let mut readings = self.held(service, account)?;
        readings.extend(match account {
            Some(name) => self.query(
                "WHERE service = ?1 AND account = ?2 ORDER BY at DESC, id DESC LIMIT ?3",
                params![service.name(), name, count],
            )?,
            None => self.query(
                "WHERE service = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
                params![service.name(), count],
            )?,
        });
        readings.extend(legacy::read(
            &self.conn,
            service,
            Rows::Recent { account, limit },
        )?);
        // Stable: at one instant, a row of this table stays ahead of an old one.
        readings.sort_by_key(|reading| std::cmp::Reverse(reading.at));
        readings.truncate(limit);
        Ok(readings)
    }

    /// One service's readings since `since` (epoch seconds), oldest first,
    /// optionally of one account.
    pub fn since(
        &self,
        service: Service,
        account: Option<&str>,
        since: i64,
    ) -> Result<Vec<Reading>> {
        let mut readings = legacy::read(&self.conn, service, Rows::Since { account, since })?;
        readings.extend(match account {
            Some(name) => self.query(
                "WHERE service = ?1 AND account = ?2 AND at >= ?3 ORDER BY at ASC, id ASC",
                params![service.name(), name, since],
            )?,
            None => self.query(
                "WHERE service = ?1 AND at >= ?2 ORDER BY at ASC, id ASC",
                params![service.name(), since],
            )?,
        });
        readings.extend(
            self.held(service, account)?
                .into_iter()
                .filter(|reading| reading.at >= since),
        );
        readings.sort_by_key(|reading| reading.at);
        Ok(readings)
    }

    /// The value each account holds now: its newest row, dated at its last
    /// confirmation, for every account confirmed after that row was written.
    /// It ends a history the way the closing row ends an earlier hold.
    fn held(&self, service: Service, account: Option<&str>) -> Result<Vec<Reading>> {
        if !legacy::has_table(&self.conn, "confirmed") {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT r.reading, c.at FROM confirmed c JOIN readings r ON r.id = (
                 SELECT id FROM readings WHERE service = c.service AND account = c.account
                 ORDER BY at DESC, id DESC LIMIT 1)
             WHERE c.service = ?1 AND (?2 IS NULL OR c.account = ?2) AND c.at > r.at",
        )?;
        let rows = stmt
            .query_map(params![service.name(), account], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(json, at)| {
                let mut reading: Reading = serde_json::from_str(&json).ok()?;
                reading.at = at;
                Some(reading)
            })
            .collect())
    }

    /// `readings` rows. Rows that no longer parse (a field this build does
    /// not know) are skipped; a database without the table has none.
    fn query(&self, tail: &str, params: impl rusqlite::Params) -> Result<Vec<Reading>> {
        if !legacy::has_table(&self.conn, "readings") {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT reading FROM readings {tail}"))?;
        let rows = stmt
            .query_map(params, |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .iter()
            .filter_map(|json| serde_json::from_str(json).ok())
            .collect())
    }
}

/// Whether two stored readings say the same thing at different times.
fn same_but_when(stored: &str, new: &str) -> bool {
    let body = |json: &str| {
        let mut value: serde_json::Value = serde_json::from_str(json).ok()?;
        value.as_object_mut()?.remove("at");
        Some(value)
    };
    match (body(stored), body(new)) {
        (Some(stored), Some(new)) => stored == new,
        _ => false,
    }
}

/// How fast one limit of an account is being used, from its history.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Burn {
    pub series: Series,
    /// The limit's name, as its newest reading gives it.
    pub label: String,
    /// Fraction of the window used per hour, between the last two readings.
    pub latest: Option<f64>,
    /// Use per hour over the whole history, each interval weighted by its
    /// length.
    pub mean: Option<f64>,
    pub stddev: Option<f64>,
    pub peak: Option<f64>,
    /// Hours until the window is spent at the latest rate.
    pub hours_left: Option<f64>,
}

/// Burn rates over an account's history, between consecutive readings of
/// the same reset window. A value held between two rows counts as no use
/// over that time, so the rates do not depend on how many rows a hold left.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stats {
    pub account: String,
    pub readings: usize,
    /// One entry per limit the history holds: account-wide ones first.
    pub limits: Vec<Burn>,
}

impl Stats {
    /// `readings` are one account's, oldest first.
    pub fn compute(account: &str, readings: &[Reading]) -> Self {
        let mut known = Series::known(readings);
        known.sort_by_key(|(series, _)| series.scope != Scope::All);
        Self {
            account: account.to_owned(),
            readings: readings.len(),
            limits: known
                .into_iter()
                .map(|(series, limit)| burn(series, limit.name(), readings))
                .collect(),
        }
    }
}

fn burn(series: Series, label: String, readings: &[Reading]) -> Burn {
    // (hours, use per hour) of each interval inside one reset window.
    let intervals: Vec<(f64, f64)> = readings
        .windows(2)
        .filter_map(|pair| {
            let hours = (pair[1].at - pair[0].at) as f64 / 3600.0;
            let (a, b) = (series.window(&pair[0])?, series.window(&pair[1])?);
            let rate = (b.used - a.used) / hours;
            (hours > 0.0 && a.resets_at == b.resets_at && b.used >= a.used && rate.is_finite())
                .then_some((hours, rate))
        })
        .collect();
    let hours: f64 = intervals.iter().map(|(hours, _)| hours).sum();
    let mean = (hours > 0.0).then(|| {
        intervals
            .iter()
            .map(|(hours, rate)| hours * rate)
            .sum::<f64>()
            / hours
    });
    let stddev = mean.map(|mean| {
        (intervals
            .iter()
            .map(|(hours, rate)| hours * (rate - mean).powi(2))
            .sum::<f64>()
            / hours)
            .sqrt()
    });
    let latest = intervals.last().map(|(_, rate)| *rate);
    let hours_left = latest.filter(|rate| *rate > 0.0).and_then(|rate| {
        let window = readings.iter().rev().find_map(|r| series.window(r))?;
        Some(window.remaining() / rate)
    });
    Burn {
        series,
        label,
        latest,
        mean,
        stddev,
        peak: intervals.iter().map(|(_, rate)| *rate).reduce(f64::max),
        hours_left,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{
        Limit, ModelKey, Overage, Scope, SessionLimit, Signals, Source, Span, Verdict, WindowStatus,
    };

    fn reading() -> Reading {
        let mut reading = Reading::empty(Service::Claude, "acct", 1_700_000_000);
        reading.limits = vec![
            Limit::new(
                Span::FiveHour,
                Scope::All,
                Window::new(0.25, 10),
                Source::Headers,
            ),
            Limit::new(
                Span::Weekly,
                Scope::All,
                Window::new(0.5, 20),
                Source::Headers,
            ),
            Limit::model(
                ModelKey::new("Opus 4.8"),
                "Opus 4.8",
                Window::new(0.75, 30),
                Source::Profile,
            ),
            Limit::extra_usage(Window::new(0.1, 40), Source::Headers),
        ];
        let claude = reading.claude_mut();
        claude.verdict = Verdict::Allowed;
        claude.claim = Some("five_hour".into());
        claude.overage = Some(Overage {
            status: Some("ok".into()),
            ..Overage::default()
        });
        claude.session_limit = SessionLimit::Absent;
        claude.signals = Signals {
            claims: vec!["seven_day".into()],
            upgrade_paths: vec!["overage".into()],
            ..Signals::default()
        };
        reading
    }

    fn overage() -> Series {
        Series::new(Span::Unknown, Scope::extra_usage())
    }

    /// The newest reading of every account named, sorted.
    fn latest(store: &Store, accounts: &[(Service, &str)]) -> Vec<Reading> {
        let mut latest = store.latest(accounts).unwrap();
        latest.sort_by(|a, b| (a.service, &a.account).cmp(&(b.service, &b.account)));
        latest
    }

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(&dir.path().join("s.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn a_reading_round_trips_through_the_store() {
        let (_dir, store) = open();
        store.insert(&reading()).unwrap();
        let rows = store.recent(Service::Claude, Some("acct"), 5).unwrap();
        assert_eq!(rows, [reading()]);
        let row = &rows[0];
        assert_eq!(Series::session().window(row), Some(Window::new(0.25, 10)));
        assert_eq!(overage().window(row), Some(Window::new(0.1, 40)));
        assert_eq!(
            Series::new(Span::Weekly, Scope::Model(ModelKey::new("opus48"))).window(row),
            Some(Window::new(0.75, 30))
        );
        let known: Vec<String> = Series::known(&rows)
            .into_iter()
            .map(|(_, limit)| limit.name())
            .collect();
        assert_eq!(known, ["5h", "weekly", "Opus 4.8", "extra usage"]);
        assert!(
            store
                .recent(Service::Claude, Some("other"), 5)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .recent(Service::Codex, Some("acct"), 5)
                .unwrap()
                .is_empty()
        );
    }

    /// The `snapshots` table as 0.3.1 created it, with three rows as 0.3.1
    /// wrote them: a full reading, a later one, and a refusal.
    const OLD_SNAPSHOTS: &str = "CREATE TABLE snapshots (
            id INTEGER PRIMARY KEY, token_name TEXT NOT NULL, probed_at TEXT NOT NULL,
            unified_status TEXT, utilization_5h REAL, reset_5h INTEGER,
            utilization_7d REAL, reset_7d INTEGER, representative_claim TEXT,
            overage_status TEXT, utilization_overage REAL, reset_overage INTEGER,
            error TEXT, utilization_opus_7d REAL, reset_opus_7d INTEGER,
            utilization_sonnet_7d REAL, reset_sonnet_7d INTEGER,
            model_usage_error TEXT, model_usage_buckets TEXT, session_limit TEXT, signals TEXT);
         CREATE INDEX idx_snapshots_token_time ON snapshots(token_name, probed_at);
         INSERT INTO snapshots (token_name, probed_at, unified_status, utilization_5h, reset_5h,
            utilization_7d, reset_7d, representative_claim, overage_status, utilization_overage,
            reset_overage, model_usage_buckets, session_limit, signals)
         VALUES ('acct', '2023-11-14T22:13:20+00:00', 'allowed', 0.25, 10, 0.5, 20, 'five_hour',
            'ok', 0.1, 40,
            '[{\"key\":\"claudeopus48\",\"label\":\"Opus 4.8\",\"window\":{\"utilization\":0.75,\"reset\":30},\"source\":\"profile\"},{\"key\":\"fable\",\"label\":\"Fable\",\"window\":{\"utilization\":0.2,\"reset\":9},\"source\":\"headers\",\"share\":0.5},{\"key\":\"odd\",\"source\":\"a source this build does not know\"}]',
            'absent',
            '{\"weekly_status\":{\"verdict\":\"allowed_warning\",\"surpassed_threshold\":0.5},\"model_weekly_status\":{\"verdict\":\"allowed\"},\"claims\":[\"seven_day\"],\"upgrade_paths\":[\"overage\"]}');
         INSERT INTO snapshots (token_name, probed_at, unified_status, utilization_7d, reset_7d,
            model_usage_error)
         VALUES ('acct', '2023-11-14T22:14:20+00:00', 'rejected', 1.0, 20, 'usage nope');
         INSERT INTO snapshots (token_name, probed_at, unified_status, error)
         VALUES ('dead', '2023-11-14T22:15:00+00:00', 'unknown', 'HTTP 401: revoked');";

    fn old_database(dir: &std::path::Path, schema: &str) -> std::path::PathBuf {
        let path = dir.join("old.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(schema)
            .unwrap();
        path
    }

    #[test]
    fn a_database_holding_only_old_snapshots_yields_the_same_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_database(dir.path(), OLD_SNAPSHOTS);
        let bytes = std::fs::read(&path).unwrap();
        let store = Store::open_read_only_at(&path).unwrap();

        let history = store.since(Service::Claude, Some("acct"), 0).unwrap();
        assert_eq!(history.len(), 2);
        let first = &history[0];
        assert_eq!((first.service, first.at), (Service::Claude, 1_700_000_000));
        assert_eq!(first.session(), Some(Window::new(0.25, 10)));
        assert_eq!(first.weekly(), Some(Window::new(0.5, 20)));
        assert_eq!(
            first.limit(Span::Weekly, &Scope::All).unwrap().status,
            Some(WindowStatus {
                verdict: Verdict::AllowedWarning,
                surpassed_threshold: Some(0.5)
            }),
            "the window status the signals column held rides on its limit"
        );
        let models: Vec<_> = first
            .models()
            .map(|limit| (limit.key().unwrap().as_str(), limit.window, limit.share))
            .collect();
        assert_eq!(
            models,
            [
                ("opus48", Window::new(0.75, 30), None),
                ("fable", Window::new(0.2, 9), Some(0.5))
            ],
            "an uncanonical key is read as its model, and an unreadable bucket is dropped alone"
        );
        assert_eq!(first.models().nth(1).unwrap().source, Source::Headers);
        let claude = first.claude().unwrap();
        assert_eq!(claude.verdict, Verdict::Allowed);
        assert_eq!(claude.claim.as_deref(), Some("five_hour"));
        assert_eq!(claude.session_limit, SessionLimit::Absent);
        assert_eq!(claude.signals.claims, ["seven_day"]);
        assert_eq!(claude.signals.upgrade_paths, ["overage"]);
        assert_eq!(overage().window(first), Some(Window::new(0.1, 40)));
        assert_eq!(
            claude.overage.as_ref().unwrap().status.as_deref(),
            Some("ok")
        );
        assert!(first.has_quota() && first.above(0.05, 0.05, None));

        let second = &history[1];
        assert_eq!(second.at, 1_700_000_060);
        assert_eq!(second.session(), None);
        assert_eq!(second.verdict(), Verdict::Rejected);
        assert_eq!(
            second.claude().unwrap().profile_error.as_deref(),
            Some("usage nope")
        );
        assert_eq!(second.session_limit(), SessionLimit::Unknown);

        // The same rows through every accessor.
        assert_eq!(
            store
                .since(Service::Claude, Some("acct"), 1_700_000_030)
                .unwrap(),
            std::slice::from_ref(second)
        );
        let recent = store.recent(Service::Claude, None, 2).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|r| r.account.as_str())
                .collect::<Vec<_>>(),
            ["dead", "acct"]
        );
        assert_eq!(
            recent[0].error,
            Some(crate::quota::ReadError::Http {
                status: 401,
                body: "revoked".into()
            })
        );
        assert!(recent[0].is_refused());
        assert_eq!(store.since(Service::Claude, None, 0).unwrap().len(), 3);
        let fleet = [
            (Service::Claude, "acct"),
            (Service::Claude, "dead"),
            (Service::Claude, "never read"),
            (Service::Codex, "acct"),
        ];
        assert_eq!(latest(&store, &fleet), [second.clone(), recent[0].clone()]);
        drop(store);
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "nothing is rewritten");

        // Opened for writing, the old rows stay where they are and new
        // readings join the same history.
        let store = Store::open_at(&path).unwrap();
        let mut new = reading();
        new.at = 1_700_000_120;
        store.insert(&new).unwrap();
        let history = store.since(Service::Claude, Some("acct"), 0).unwrap();
        assert_eq!(
            history.iter().map(|r| r.at).collect::<Vec<_>>(),
            [1_700_000_000, 1_700_000_060, 1_700_000_120]
        );
        assert_eq!(history[..2], [first.clone(), second.clone()]);
        assert_eq!(
            store.recent(Service::Claude, Some("acct"), 2).unwrap(),
            [new.clone(), second.clone()]
        );
        assert_eq!(latest(&store, &fleet), [new.clone(), recent[0].clone()]);
        let old_rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(old_rows, 3);
    }

    #[test]
    fn a_table_from_before_a_column_existed_reads_without_it() {
        // 0.2: no session_limit or signals column, a naive timestamp, and the
        // per-family columns, which are no longer read.
        let dir = tempfile::tempdir().unwrap();
        let path = old_database(
            dir.path(),
            "CREATE TABLE snapshots (
                id INTEGER PRIMARY KEY, token_name TEXT NOT NULL, probed_at TEXT NOT NULL,
                unified_status TEXT, utilization_5h REAL, reset_5h INTEGER,
                utilization_7d REAL, reset_7d INTEGER, representative_claim TEXT,
                overage_status TEXT, utilization_overage REAL, reset_overage INTEGER,
                error TEXT, utilization_opus_7d REAL, reset_opus_7d INTEGER);
             INSERT INTO snapshots (token_name, probed_at, utilization_7d, reset_7d,
                utilization_opus_7d, reset_opus_7d)
             VALUES ('old', '2026-01-01T00:00:00.5', 0.5, 9, 0.4, 7);
             INSERT INTO snapshots (token_name, probed_at, utilization_7d)
             VALUES ('old', 'not a time', 0.6);",
        );
        let store = Store::open_read_only_at(&path).unwrap();
        let rows = store.recent(Service::Claude, Some("old"), 5).unwrap();
        assert_eq!(rows.len(), 1, "a row without a readable time is left out");
        assert_eq!(rows[0].weekly(), Some(Window::new(0.5, 9)));
        assert_eq!(rows[0].models().count(), 0);
        assert_eq!(rows[0].verdict(), Verdict::Unknown);
        assert_eq!(rows[0].at, 1_767_225_600);
    }

    fn usage(service: Service, account: &str, at: i64, used: f64) -> Reading {
        Reading {
            plan: Some("max".into()),
            limits: vec![crate::providers::limit(
                None,
                "5h",
                Some(18_000),
                Window::new(used, at + 3600),
                None,
            )],
            ..Reading::empty(service, account, at)
        }
    }

    #[test]
    fn every_service_shares_the_table_and_the_latest_is_per_account() {
        let (_dir, store) = open();
        let failed = crate::providers::failed(Service::Kimi, "kimi", 300, "HTTP 401");
        for reading in [
            usage(Service::Zai, "glm", 100, 0.1),
            usage(Service::Zai, "glm", 200, 0.2),
            usage(Service::Kimi, "glm", 150, 0.5),
            failed.clone(),
            reading(),
        ] {
            store.insert(&reading).unwrap();
        }
        let glm = store.since(Service::Zai, Some("glm"), 150).unwrap();
        assert_eq!(glm, [usage(Service::Zai, "glm", 200, 0.2)]);
        let recent = store.recent(Service::Zai, Some("glm"), 5).unwrap();
        assert_eq!(recent.iter().map(|u| u.at).collect::<Vec<_>>(), [200, 100]);
        let newest: Vec<_> = latest(
            &store,
            &[
                (Service::Claude, "acct"),
                (Service::Zai, "glm"),
                (Service::Kimi, "glm"),
                (Service::Kimi, "kimi"),
                (Service::Antigravity, "glm"),
            ],
        )
        .into_iter()
        .map(|u| (u.service, u.account, u.at))
        .collect();
        assert_eq!(
            newest,
            [
                (Service::Claude, "acct".to_owned(), 1_700_000_000),
                (Service::Zai, "glm".to_owned(), 200),
                (Service::Kimi, "glm".to_owned(), 150),
                (Service::Kimi, "kimi".to_owned(), 300)
            ]
        );
        assert_eq!(store.recent(Service::Kimi, None, 1).unwrap(), [failed]);
        assert_eq!(store.since(Service::Kimi, None, 0).unwrap().len(), 2);
    }

    /// `service_readings` and `codex_readings` as 0.3.x created them, with
    /// rows in the shapes 0.3.x wrote (the service rows are today's, with
    /// the account names changed).
    const OLD_DOCUMENTS: &str = r#"
        CREATE TABLE service_readings (id INTEGER PRIMARY KEY, service TEXT NOT NULL,
            account TEXT NOT NULL, at INTEGER NOT NULL, usage TEXT NOT NULL);
        CREATE INDEX idx_service_readings_account_time ON service_readings(service, account, at);
        CREATE TABLE codex_readings (id INTEGER PRIMARY KEY, account TEXT NOT NULL,
            at INTEGER NOT NULL, probe TEXT NOT NULL);
        INSERT INTO service_readings (service, account, at, usage) VALUES
        ('zai', 'glm', 100, '{"service":"zai","account":"glm","at":100,"plan":"max","meters":[{"label":"5h","window":{"utilization":0.005,"reset":1794448052},"span_secs":18000,"amount":{"used":159.0,"limit":28000.0,"unit":"credits"}},{"label":"weekly","window":{"utilization":0.01435,"reset":1794879119},"span_secs":604800,"amount":{"used":2009.0,"limit":140000.0,"unit":"credits"}},{"group":"MCP tools","label":"monthly","window":{"utilization":0.25,"reset":0},"span_secs":2592000}]}'),
        ('kimi', 'kimi', 110, '{"service":"kimi","account":"kimi","at":110,"plan":"Vivace","meters":[{"label":"5h","window":{"utilization":0.0093,"reset":1794458418},"span_secs":18000},{"label":"monthly (Kimi Code share)","window":{"utilization":0.2258,"reset":1794519618}}],"balances":[{"label":"extra usage","left_minor":2196,"total_minor":5000,"currency":"USD"}]}'),
        ('antigravity', 'a@example.com', 120, '{"service":"antigravity","account":"a@example.com","at":120,"plan":"g1-ultra-tier","meters":[{"group":"Gemini Models","label":"weekly","window":{"utilization":0.0,"reset":0},"span_secs":604800},{"group":"Gemini Models","label":"5h","window":{"utilization":0.1,"reset":0},"span_secs":18000},{"group":"Claude and GPT models","label":"weekly","window":{"utilization":0.0,"reset":0},"span_secs":604800}]}'),
        ('antigravity', 'a@example.com', 130, '{"service":"antigravity","account":"a@example.com","at":130,"meters":[],"error":"sign in again: the grant was revoked"}'),
        ('kimi', 'kimi', 140, '{"service":"a service this build does not know","account":"kimi","at":140}');
        INSERT INTO codex_readings (account, at, probe) VALUES
        ('work', 200, '{"account":"work","codex_home":"/homes/work","at":200,"usage":{"user_id":"u1","account_id":"acct-1","email":"w@example.com","plan":"plus","limit":{"allowed":true,"limit_reached":false,"windows":[{"span":"weekly","utilization":0.42,"reset":1794607293},{"span":"five_hour","utilization":0.1,"reset":4600},{"span":{"seconds":86400},"utilization":0.5,"reset":99}]},"credits":{"has_credits":false,"unlimited":false,"overage_limit_reached":null,"balance":"0"},"limit_reason":null,"extra":[{"name":"codex_spark","feature":"gpt-5.3-codex-spark","model":null,"limit":{"allowed":false,"limit_reached":true,"windows":[{"span":"unknown","utilization":1.0,"reset":100}]}}],"models":[{"slug":"gpt-6-astra","available":true,"available_at":null,"credits_would_enable":false}],"spend_control":{"reached":false,"limit":null},"reset_credits":{"available":2,"applicable":0}},"error":null,"last_refresh":"2026-11-10T00:00:00Z","email_hint":"hint@example.com"}'),
        ('work', 210, '{"account":"work","codex_home":"/homes/work","at":210,"usage":null,"error":{"kind":"challenged","status":403,"attempts":4},"last_refresh":null,"email_hint":"hint@example.com"}'),
        ('gone', 220, '{"account":"gone","codex_home":"/homes/gone","at":220,"usage":null,"error":{"kind":"auth_file_has_no_access_token"},"last_refresh":null,"email_hint":null}');
    "#;

    #[test]
    fn old_service_and_codex_rows_read_as_readings() {
        use crate::quota::{CodexDetail, ReadError, ResetCreditCount};
        let dir = tempfile::tempdir().unwrap();
        let path = old_database(dir.path(), OLD_DOCUMENTS);
        let bytes = std::fs::read(&path).unwrap();
        let store = Store::open_read_only_at(&path).unwrap();

        let glm = store
            .recent(Service::Zai, Some("glm"), 5)
            .unwrap()
            .remove(0);
        assert_eq!((glm.at, glm.plan.as_deref()), (100, Some("max")));
        assert_eq!(glm.session(), Some(Window::new(0.005, 1_794_448_052)));
        assert_eq!(glm.weekly(), Some(Window::new(0.01435, 1_794_879_119)));
        assert_eq!(glm.limits[0].label, "5h");
        assert_eq!(glm.limits[0].source, Source::Poll);
        assert_eq!(glm.limits[0].amount.as_ref().unwrap().unit, "credits");
        let mcp = &glm.limits[2];
        assert_eq!((mcp.span, mcp.group()), (Span::Monthly, Some("MCP tools")));
        assert_eq!(glm.detail, crate::quota::Detail::None);

        let kimi = store.since(Service::Kimi, None, 0).unwrap();
        assert_eq!(kimi.len(), 1, "a row this build cannot read is left out");
        assert_eq!(kimi[0].limits[1].span, Span::Unknown);
        assert_eq!(kimi[0].limits[1].label, "monthly (Kimi Code share)");
        assert_eq!(kimi[0].balances[0].left_minor, 2196);

        let agy = store
            .since(Service::Antigravity, Some("a@example.com"), 0)
            .unwrap();
        assert_eq!(agy.iter().map(|r| r.at).collect::<Vec<_>>(), [120, 130]);
        assert_eq!(agy[0].scoped().len(), 2);
        assert!(!agy[0].has_quota());
        assert!(
            agy[0]
                .scoped_to(Some("gemini"))
                .unwrap()
                .above(0.05, 0.05, None)
        );
        assert_eq!(
            agy[1].error,
            Some(ReadError::Other {
                message: "sign in again: the grant was revoked".into()
            })
        );

        let work = store.since(Service::Codex, Some("work"), 0).unwrap();
        assert_eq!(work.len(), 2);
        let read = &work[0];
        assert_eq!((read.at, read.plan.as_deref()), (200, Some("plus")));
        assert_eq!(read.weekly(), Some(Window::new(0.42, 1_794_607_293)));
        assert_eq!(read.session(), Some(Window::new(0.1, 4_600)));
        assert_eq!(
            read.window(Span::Other(86_400), &Scope::All),
            Some(Window::new(0.5, 99))
        );
        let spark = read
            .limit(Span::Unknown, &Scope::Group("gpt-5.3-codex-spark".into()))
            .unwrap();
        assert_eq!(spark.label, "codex_spark");
        assert!(spark.is_rejected());
        let codex: &CodexDetail = read.codex().unwrap();
        assert_eq!(codex.email.as_deref(), Some("w@example.com"));
        assert_eq!(codex.allowed, Some(true));
        assert_eq!(codex.models[0].slug, "gpt-6-astra");
        assert_eq!(
            codex.reset_credits,
            Some(ResetCreditCount {
                available: 2,
                applicable: Some(0),
                expires_at: None,
            })
        );
        assert!(read.above(0.0, 0.0, None));
        assert_eq!(
            work[1].error,
            Some(ReadError::Edge {
                status: 403,
                attempts: 4
            })
        );
        assert_eq!(
            work[1].codex().unwrap().email.as_deref(),
            Some("hint@example.com"),
            "the stored id token's email stands in"
        );

        let fleet = [
            (Service::Codex, "gone"),
            (Service::Codex, "work"),
            (Service::Antigravity, "a@example.com"),
            (Service::Zai, "glm"),
            (Service::Kimi, "kimi"),
        ];
        let newest: Vec<_> = latest(&store, &fleet)
            .into_iter()
            .map(|r| (r.service, r.account, r.at))
            .collect();
        assert_eq!(
            newest,
            [
                (Service::Codex, "gone".to_owned(), 220),
                (Service::Codex, "work".to_owned(), 210),
                (Service::Antigravity, "a@example.com".to_owned(), 130),
                (Service::Zai, "glm".to_owned(), 100),
            ],
            "kimi's newest row is unreadable, so it has no latest"
        );
        let gone = store.recent(Service::Codex, Some("gone"), 1).unwrap();
        assert!(gone[0].is_refused());
        assert_eq!(
            gone[0].error.as_ref().unwrap().to_string(),
            "auth.json has no access_token"
        );
        drop(store);
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "nothing is rewritten");

        // A new read of an old account is the latest, and the history is one.
        let store = Store::open_at(&path).unwrap();
        store.insert(&usage(Service::Zai, "glm", 300, 0.3)).unwrap();
        let history = store.since(Service::Zai, Some("glm"), 0).unwrap();
        assert_eq!(history.iter().map(|r| r.at).collect::<Vec<_>>(), [100, 300]);
        assert_eq!(
            store.latest(&[(Service::Zai, "glm")]).unwrap(),
            [usage(Service::Zai, "glm", 300, 0.3)]
        );
    }

    #[test]
    fn a_read_only_open_sees_the_rows_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.db");
        assert!(Store::open_read_only_at(&missing).is_err());
        assert!(!missing.exists());

        let path = dir.path().join("s.db");
        {
            let store = Store::open_at(&path).unwrap();
            store.insert(&reading()).unwrap();
            let mut later = reading();
            later.at += 60;
            later.set_weekly(Some(Window::new(0.6, 20)));
            store.insert(&later).unwrap();
            // Another service's account of the same name is its own.
            let probe = Reading::empty(Service::Codex, "acct", 5);
            store.insert(&probe).unwrap();
            store.insert(&Reading { at: 9, ..probe }).unwrap();
        }
        let files = |dir: &std::path::Path| {
            let mut names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        // The writer checkpointed and removed its sidecars when it closed.
        assert_eq!(files(dir.path()), ["s.db"]);
        let store = Store::open_read_only_at(&path).unwrap();
        let of = |store: &Store, service| -> Reading {
            let mut latest = store.latest(&[(service, "acct")]).unwrap();
            assert_eq!(latest.len(), 1);
            latest.remove(0)
        };
        assert_eq!(
            of(&store, Service::Claude).weekly(),
            Some(Window::new(0.6, 20))
        );
        assert_eq!(of(&store, Service::Codex).at, 9);
        assert!(
            store.insert(&reading()).is_err(),
            "read-only refuses a write"
        );
        drop(store);
        assert_eq!(files(dir.path()), ["s.db"], "no sidecar files appear");

        // While a writer holds the database open, its rows in the WAL are seen.
        let writer = Store::open_at(&path).unwrap();
        let mut newest = reading();
        newest.at += 120;
        newest.set_weekly(Some(Window::new(0.7, 20)));
        writer.insert(&newest).unwrap();
        let reader = Store::open_read_only_at(&path).unwrap();
        assert_eq!(
            of(&reader, Service::Claude).weekly(),
            Some(Window::new(0.7, 20))
        );
        drop(reader);
        drop(writer);

        // A database from before a table existed has no rows there.
        let old = dir.path().join("old.db");
        Connection::open(&old)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated (id INTEGER PRIMARY KEY);")
            .unwrap();
        let store = Store::open_read_only_at(&old).unwrap();
        assert!(
            store
                .latest(&[(Service::Codex, "acct")])
                .unwrap()
                .is_empty()
        );
        assert!(store.since(Service::Codex, None, 0).unwrap().is_empty());
        assert!(store.recent(Service::Kimi, None, 9).unwrap().is_empty());
    }

    fn weekly_at(at: i64, used: f64) -> Reading {
        let mut reading = Reading::empty(Service::Claude, "acct", at);
        reading.limits = vec![
            Limit::new(
                Span::Weekly,
                Scope::All,
                Window::new(used, 9_000_000),
                Source::Headers,
            ),
            Limit::model(
                ModelKey::new("fable"),
                "Fable",
                Window::new(used / 2.0, 9_000_000),
                Source::Headers,
            ),
        ];
        reading.claude_mut().verdict = Verdict::Allowed;
        reading
    }

    fn rows(store: &Store) -> Vec<(i64, f64)> {
        let mut stmt = store
            .conn
            .prepare("SELECT reading FROM readings ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|json| {
                let reading: Reading = serde_json::from_str(&json.unwrap()).unwrap();
                (reading.at, reading.weekly().unwrap().used)
            })
            .collect()
    }

    #[test]
    fn an_unchanged_reading_adds_no_row_until_the_hold_is_ten_minutes_old() {
        let (_dir, store) = open();
        // Probed every 20 s: 0.10 for 25 minutes, then a step to 0.12.
        for tick in 0..=75 {
            store.insert(&weekly_at(1_000 + tick * 20, 0.10)).unwrap();
        }
        assert_eq!(
            rows(&store),
            [(1_000, 0.10), (1_600, 0.10), (2_200, 0.10)],
            "one row when the value appears, then one every ten minutes"
        );
        // The stored view is as fresh as the last read that confirmed it.
        let newest = store.latest(&[(Service::Claude, "acct")]).unwrap();
        assert_eq!(newest, [weekly_at(2_500, 0.10)]);
        let history = store.since(Service::Claude, Some("acct"), 0).unwrap();
        assert_eq!(
            history.iter().map(|r| r.at).collect::<Vec<_>>(),
            [1_000, 1_600, 2_200, 2_500],
            "the held value ends the history at its last confirmation"
        );
        assert_eq!(
            store.recent(Service::Claude, None, 2).unwrap(),
            [weekly_at(2_500, 0.10), weekly_at(2_200, 0.10)]
        );

        store.insert(&weekly_at(2_520, 0.12)).unwrap();
        assert_eq!(
            rows(&store),
            [
                (1_000, 0.10),
                (1_600, 0.10),
                (2_200, 0.10),
                (2_500, 0.10),
                (2_520, 0.12)
            ],
            "a change is written with the row that closes the hold before it"
        );
        // A change straight after a written row needs no closing row, and a
        // failed read is a change like any other.
        store.insert(&weekly_at(2_540, 0.13)).unwrap();
        let failed = Reading::failed(
            Service::Claude,
            "acct",
            2_560,
            crate::quota::ReadError::Network {
                message: "down".into(),
            },
        );
        store.insert(&failed).unwrap();
        store
            .insert(&Reading {
                at: 2_580,
                ..failed.clone()
            })
            .unwrap();
        let history = store.since(Service::Claude, Some("acct"), 2_530).unwrap();
        assert_eq!(
            history.iter().map(|r| r.at).collect::<Vec<_>>(),
            [2_540, 2_560, 2_580]
        );
        assert_eq!(history[1].error, failed.error);
        // Another account's hold is its own.
        let mut other = weekly_at(2_590, 0.13);
        other.account = "other".into();
        store.insert(&other).unwrap();
        assert_eq!(
            store.latest(&[(Service::Claude, "other")]).unwrap(),
            [other]
        );
    }

    #[test]
    fn burn_rates_are_the_same_from_dense_and_sparse_rows() {
        // One account's use, read every 20 s for an hour: idle for 20
        // minutes, burning 0.001 per read for 10, idle for 15, a burst, then
        // a reset to a new window.
        let mut used = 0.10;
        let mut dense = Vec::new();
        for tick in 0..=180i64 {
            let at = 10_000 + tick * 20;
            if (60..90).contains(&tick) {
                used += 0.001;
            }
            if tick == 135 {
                used += 0.05;
            }
            let mut reading = weekly_at(at, used);
            if tick >= 170 {
                reading.limits[0].window = Window::new(0.0, 9_600_000);
            }
            dense.push(reading);
        }
        // The rows 0.3 wrote: one per read.
        let all = Stats::compute("acct", &dense);
        // The rows this store keeps of the same reads.
        let (_dir, store) = open();
        for reading in &dense {
            store.insert(reading).unwrap();
        }
        let kept = store.since(Service::Claude, Some("acct"), 0).unwrap();
        assert!(
            kept.len() < dense.len() / 2,
            "{} of {} rows",
            kept.len(),
            dense.len()
        );
        let sparse = Stats::compute("acct", &kept);

        assert_eq!(all.limits.len(), 2);
        assert_eq!(sparse.limits.len(), 2);
        for (dense, sparse) in all.limits.iter().zip(&sparse.limits) {
            assert_eq!(dense.series, sparse.series);
            assert_eq!(dense.label, sparse.label);
            let close = |a: Option<f64>, b: Option<f64>, what: &str| {
                let (a, b) = (a.unwrap(), b.unwrap());
                assert!((a - b).abs() < 1e-9, "{} {what}: {a} vs {b}", dense.label);
            };
            close(dense.latest, sparse.latest, "latest");
            close(dense.mean, sparse.mean, "mean");
            close(dense.stddev, sparse.stddev, "stddev");
            close(dense.peak, sparse.peak, "peak");
            assert_eq!(dense.hours_left, sparse.hours_left);
        }
        let weekly = &all.limits[0];
        assert_eq!(weekly.label, "weekly");
        // 0.08 used over the 179 intervals that do not span the reset.
        let hours = 179.0 * 20.0 / 3600.0;
        assert!((weekly.mean.unwrap() - 0.08 / hours).abs() < 1e-9);
        assert!((weekly.peak.unwrap() - 0.05 / (20.0 / 3600.0)).abs() < 1e-9);
        assert_eq!(weekly.latest, Some(0.0));
        assert_eq!(weekly.hours_left, None, "nothing is burning now");
        assert_eq!(all.limits[1].label, "Fable");
    }

    #[test]
    fn an_account_with_a_new_row_is_not_looked_up_in_the_old_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_database(dir.path(), OLD_SNAPSHOTS);
        let store = Store::open_at(&path).unwrap();
        let mut new = reading();
        new.at = 1_600_000_000;
        store.insert(&new).unwrap();
        // The old rows of `acct` are newer than its row in `readings`; the
        // row in `readings` is still the answer, and `dead`, which has none
        // there, is read from the old table.
        let newest = latest(
            &store,
            &[(Service::Claude, "acct"), (Service::Claude, "dead")],
        );
        assert_eq!(newest[0], new);
        assert_eq!(newest[1].account, "dead");
    }
}

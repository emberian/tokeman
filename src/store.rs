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
use rusqlite::{Connection, params};
use serde::Serialize;
use std::time::Duration;

use crate::paths;
use crate::providers::Usage;
use crate::quota::{ModelKey, Reading, Service, Window};

pub struct Store {
    conn: Connection,
}

/// A series a chart can draw.
#[derive(Clone, Copy)]
pub enum Series<'a> {
    Session,
    Weekly,
    Overage,
    Model(&'a ModelKey),
}

impl Series<'_> {
    /// This series' window in one reading.
    pub fn window(self, reading: &Reading) -> Option<Window> {
        match self {
            Self::Session => reading.session(),
            Self::Weekly => reading.weekly(),
            Self::Overage => reading.claude()?.overage.as_ref()?.window,
            Self::Model(key) => reading.model_limit(key).map(|limit| limit.window),
        }
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
            CREATE TABLE IF NOT EXISTS service_readings (
                id INTEGER PRIMARY KEY,
                service TEXT NOT NULL,
                account TEXT NOT NULL,
                at INTEGER NOT NULL,
                usage TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_service_readings_account_time
                ON service_readings(service, account, at);",
        )?;
        Ok(())
    }

    pub fn insert(&self, reading: &Reading) -> Result<()> {
        self.conn.execute(
            "INSERT INTO readings (service, account, at, reading) VALUES (?1, ?2, ?3, ?4)",
            params![
                reading.service.name(),
                reading.account,
                reading.at,
                serde_json::to_string(reading)?
            ],
        )?;
        Ok(())
    }

    /// The newest reading of each account of each service, in no particular
    /// order.
    pub fn latest(&self) -> Result<Vec<Reading>> {
        let mut latest = self.query(
            "WHERE id IN (SELECT MAX(id) FROM readings GROUP BY service, account)",
            [],
        )?;
        for service in [Service::Claude, Service::Codex] {
            for reading in legacy::read(&self.conn, service, Rows::Latest)? {
                let newer = latest
                    .iter()
                    .any(|new| new.service == reading.service && new.account == reading.account);
                if !newer {
                    latest.push(reading);
                }
            }
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
        let mut readings = match account {
            Some(name) => self.query(
                "WHERE service = ?1 AND account = ?2 ORDER BY at DESC, id DESC LIMIT ?3",
                params![service.name(), name, count],
            )?,
            None => self.query(
                "WHERE service = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
                params![service.name(), count],
            )?,
        };
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
        readings.sort_by_key(|reading| reading.at);
        Ok(readings)
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

    pub fn insert_usage(&self, usage: &Usage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO service_readings (service, account, at, usage) VALUES (?1, ?2, ?3, ?4)",
            params![
                usage.service.name(),
                usage.account,
                usage.at,
                serde_json::to_string(usage)?
            ],
        )?;
        Ok(())
    }

    /// The newest reads, newest first, optionally of one service account.
    pub fn recent_usage(
        &self,
        account: Option<(Service, &str)>,
        limit: usize,
    ) -> Result<Vec<Usage>> {
        let limit = limit as i64;
        match account {
            Some((service, name)) => self.query_usage(
                "WHERE service = ?1 AND account = ?2 ORDER BY at DESC, id DESC LIMIT ?3",
                params![service.name(), name, limit],
            ),
            None => self.query_usage("ORDER BY at DESC, id DESC LIMIT ?1", params![limit]),
        }
    }

    /// The newest read of each service account, in no particular order.
    pub fn latest_usage(&self) -> Result<Vec<Usage>> {
        self.query_usage(
            "WHERE id IN (SELECT MAX(id) FROM service_readings GROUP BY service, account)",
            [],
        )
    }

    /// One service account's reads since `since` (epoch seconds), oldest first.
    pub fn usage_since(&self, service: Service, account: &str, since: i64) -> Result<Vec<Usage>> {
        self.query_usage(
            "WHERE service = ?1 AND account = ?2 AND at >= ?3 ORDER BY at ASC, id ASC",
            params![service.name(), account, since],
        )
    }

    /// Rows that no longer parse (a field this build does not know) are skipped.
    fn query_usage(&self, tail: &str, params: impl rusqlite::Params) -> Result<Vec<Usage>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT usage FROM service_readings {tail}"))?;
        let rows = stmt
            .query_map(params, |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .iter()
            .filter_map(|json| serde_json::from_str(json).ok())
            .collect())
    }
}

/// Burn rates over an account's history, between consecutive readings of
/// the same reset window.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub account: String,
    pub snapshots: usize,
    /// Fraction of the window used per hour, from the latest pair.
    pub session_rate: Option<f64>,
    pub weekly_rate: Option<f64>,
    pub weekly_rate_mean: Option<f64>,
    pub weekly_rate_stddev: Option<f64>,
    pub weekly_rate_peak: Option<f64>,
    pub session_hours_left: Option<f64>,
    pub weekly_hours_left: Option<f64>,
}

impl Stats {
    pub fn compute(account: &str, snapshots: &[Reading]) -> Self {
        let rates = |series: Series| -> Vec<f64> {
            snapshots
                .windows(2)
                .filter_map(|pair| {
                    let hours = (pair[1].at - pair[0].at) as f64 / 3600.0;
                    let (a, b) = (series.window(&pair[0])?, series.window(&pair[1])?);
                    let rate = (b.used - a.used) / hours;
                    (hours > 0.0
                        && a.resets_at == b.resets_at
                        && b.used >= a.used
                        && rate.is_finite())
                    .then_some(rate)
                })
                .collect()
        };
        let session = rates(Series::Session);
        let weekly = rates(Series::Weekly);
        let mean = (!weekly.is_empty()).then(|| weekly.iter().sum::<f64>() / weekly.len() as f64);
        let stddev = mean.map(|mean| {
            (weekly.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / weekly.len() as f64).sqrt()
        });
        let hours_left = |latest: Option<f64>, series: Series| {
            let rate = latest.filter(|&r| r > 0.0)?;
            Some(series.window(snapshots.last()?)?.remaining() / rate)
        };
        Self {
            account: account.to_owned(),
            snapshots: snapshots.len(),
            session_rate: session.last().copied(),
            weekly_rate: weekly.last().copied(),
            weekly_rate_mean: mean,
            weekly_rate_stddev: stddev,
            weekly_rate_peak: weekly.iter().copied().reduce(f64::max),
            session_hours_left: hours_left(session.last().copied(), Series::Session),
            weekly_hours_left: hours_left(weekly.last().copied(), Series::Weekly),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{
        Limit, Overage, Scope, SessionLimit, Signals, Source, Span, Verdict, WindowStatus,
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
        ];
        let claude = reading.claude_mut();
        claude.verdict = Verdict::Allowed;
        claude.claim = Some("five_hour".into());
        claude.overage = Some(Overage {
            status: Some("ok".into()),
            window: Some(Window::new(0.1, 40)),
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
        assert_eq!(Series::Session.window(row), Some(Window::new(0.25, 10)));
        assert_eq!(Series::Overage.window(row), Some(Window::new(0.1, 40)));
        assert_eq!(
            Series::Model(&ModelKey::new("opus48")).window(row),
            Some(Window::new(0.75, 30))
        );
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
        assert_eq!(Series::Overage.window(first), Some(Window::new(0.1, 40)));
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
            [second.clone()]
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
        let mut latest = store.latest().unwrap();
        latest.sort_by(|a, b| a.account.cmp(&b.account));
        assert_eq!(latest, [second.clone(), recent[0].clone()]);
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
        let latest = store.latest().unwrap();
        assert_eq!(latest.len(), 2);
        assert!(latest.contains(&new));
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

    fn usage(service: Service, account: &str, at: i64, used: f64) -> Usage {
        Usage {
            service,
            account: account.into(),
            at,
            plan: Some("max".into()),
            meters: vec![crate::providers::Meter {
                group: None,
                label: "5h".into(),
                window: Window::new(used, at + 3600),
                span_secs: Some(18_000),
                amount: None,
            }],
            balances: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn service_reads_round_trip_and_the_latest_is_per_account() {
        let (_dir, store) = open();
        store
            .insert_usage(&usage(Service::Zai, "glm", 100, 0.1))
            .unwrap();
        store
            .insert_usage(&usage(Service::Zai, "glm", 200, 0.2))
            .unwrap();
        store
            .insert_usage(&usage(Service::Kimi, "glm", 150, 0.5))
            .unwrap();
        store
            .insert_usage(&Usage::failed(Service::Kimi, "kimi", 300, "HTTP 401"))
            .unwrap();
        let glm = store.usage_since(Service::Zai, "glm", 150).unwrap();
        assert_eq!(glm, [usage(Service::Zai, "glm", 200, 0.2)]);
        let recent = store.recent_usage(Some((Service::Zai, "glm")), 5).unwrap();
        assert_eq!(recent.iter().map(|u| u.at).collect::<Vec<_>>(), [200, 100]);
        let mut latest: Vec<_> = store
            .latest_usage()
            .unwrap()
            .into_iter()
            .map(|u| (u.service, u.account, u.at))
            .collect();
        latest.sort();
        assert_eq!(
            latest,
            [
                (Service::Zai, "glm".to_owned(), 200),
                (Service::Kimi, "glm".to_owned(), 150),
                (Service::Kimi, "kimi".to_owned(), 300)
            ]
        );
        assert_eq!(
            store.recent_usage(None, 1).unwrap()[0].error.as_deref(),
            Some("HTTP 401")
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
            let mut latest = store.latest().unwrap();
            latest.retain(|reading| reading.service == service);
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
        assert!(store.latest().unwrap().is_empty());
        assert!(store.latest_usage().is_err());
    }
}

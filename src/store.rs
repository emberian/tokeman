//! Probe history: one row per reading, in sqlite, for charts and stats.
//!
//! The schema is the one every row since 0.1.0 was written with, plus
//! columns added at the end (older rows read them as empty); legacy
//! Opus/Sonnet columns are read into buckets and no longer written. Service
//! reads (GLM, Kimi, Antigravity) and Codex probes live in their own tables,
//! one JSON document per row. [`Store::open_read_only`] is for surfaces that
//! only show what was stored: it creates no file, table or row.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::time::Duration;

use crate::codex::probe::Probe;
use crate::paths;
use crate::providers::{Service, Usage};
use crate::quota::{Bucket, ModelKey, Reading, SessionLimit, Signals, Source, Window};

pub struct Store {
    conn: Connection,
}

/// One stored reading.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub account: String,
    pub at: DateTime<Utc>,
    pub status: Option<String>,
    pub claim: Option<String>,
    pub session: Option<Window>,
    pub weekly: Option<Window>,
    pub overage_status: Option<String>,
    pub overage: Option<Window>,
    pub error: Option<String>,
    pub profile_error: Option<String>,
    pub buckets: Vec<Bucket>,
    /// `Unknown` for rows written before 0.4.
    pub session_limit: SessionLimit,
    /// Empty for rows written before 0.4.
    pub signals: Signals,
}

/// A series a chart can draw.
#[derive(Clone, Copy)]
pub enum Series<'a> {
    Session,
    Weekly,
    Overage,
    Model(&'a ModelKey),
}

impl Snapshot {
    pub fn window(&self, series: Series) -> Option<Window> {
        match series {
            Series::Session => self.session,
            Series::Weekly => self.weekly,
            Series::Overage => self.overage,
            Series::Model(key) => self
                .buckets
                .iter()
                .find(|b| &b.key == key)
                .map(|b| b.window),
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
    /// table this database does not have yet reads as an error, which the
    /// caller treats as no rows.
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
            "CREATE TABLE IF NOT EXISTS snapshots (
                id INTEGER PRIMARY KEY,
                token_name TEXT NOT NULL,
                probed_at TEXT NOT NULL,
                unified_status TEXT,
                utilization_5h REAL,
                reset_5h INTEGER,
                utilization_7d REAL,
                reset_7d INTEGER,
                representative_claim TEXT,
                overage_status TEXT,
                utilization_overage REAL,
                reset_overage INTEGER,
                error TEXT,
                utilization_opus_7d REAL,
                reset_opus_7d INTEGER,
                utilization_sonnet_7d REAL,
                reset_sonnet_7d INTEGER,
                model_usage_error TEXT,
                model_usage_buckets TEXT,
                session_limit TEXT,
                signals TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_snapshots_token_time
                ON snapshots(token_name, probed_at);
            CREATE TABLE IF NOT EXISTS service_readings (
                id INTEGER PRIMARY KEY,
                service TEXT NOT NULL,
                account TEXT NOT NULL,
                at INTEGER NOT NULL,
                usage TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_service_readings_account_time
                ON service_readings(service, account, at);
            CREATE TABLE IF NOT EXISTS codex_readings (
                id INTEGER PRIMARY KEY,
                account TEXT NOT NULL,
                at INTEGER NOT NULL,
                probe TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_codex_readings_account_time
                ON codex_readings(account, at);",
        )?;
        // Databases from 0.1.x predate these columns; ALTER fails harmlessly when present.
        for column in [
            "error TEXT",
            "utilization_opus_7d REAL",
            "reset_opus_7d INTEGER",
            "utilization_sonnet_7d REAL",
            "reset_sonnet_7d INTEGER",
            "model_usage_error TEXT",
            "model_usage_buckets TEXT",
            "session_limit TEXT",
            "signals TEXT",
        ] {
            let _ = self
                .conn
                .execute(&format!("ALTER TABLE snapshots ADD COLUMN {column}"), []);
        }
        Ok(())
    }

    pub fn insert(&self, reading: &Reading) -> Result<()> {
        let split = |window: Option<Window>| (window.map(|w| w.used), window.map(|w| w.resets_at));
        let (u5h, r5h) = split(reading.session);
        let (u7d, r7d) = split(reading.weekly);
        let overage = reading.overage.as_ref();
        let (u_ov, r_ov) = split(overage.and_then(|o| o.window));
        let buckets = (!reading.buckets.is_empty())
            .then(|| serde_json::to_string(&reading.buckets))
            .transpose()?;
        let session_limit = (!reading.session_limit.is_unknown())
            .then(|| serde_json::to_value(reading.session_limit))
            .transpose()?
            .and_then(|value| value.as_str().map(str::to_owned));
        let signals = (!reading.signals.is_empty())
            .then(|| serde_json::to_string(&reading.signals))
            .transpose()?;
        self.conn.execute(
            "INSERT INTO snapshots (
                token_name, probed_at, unified_status, utilization_5h, reset_5h,
                utilization_7d, reset_7d, representative_claim,
                overage_status, utilization_overage, reset_overage, error,
                model_usage_error, model_usage_buckets, session_limit, signals
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                reading.account,
                DateTime::from_timestamp(reading.at, 0)
                    .unwrap_or_default()
                    .to_rfc3339(),
                reading.verdict.as_str(),
                u5h,
                r5h,
                u7d,
                r7d,
                reading.claim.as_deref(),
                overage.and_then(|o| o.status.as_deref()),
                u_ov,
                r_ov,
                reading.error.as_ref().map(ToString::to_string),
                reading.profile_error.as_deref(),
                buckets,
                session_limit,
                signals,
            ],
        )?;
        Ok(())
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

    pub fn insert_codex(&self, probe: &Probe) -> Result<()> {
        self.conn.execute(
            "INSERT INTO codex_readings (account, at, probe) VALUES (?1, ?2, ?3)",
            params![probe.account, probe.at, serde_json::to_string(probe)?],
        )?;
        Ok(())
    }

    /// The newest probe of each Codex account, in no particular order. Rows
    /// that no longer parse are skipped.
    pub fn latest_codex(&self) -> Result<Vec<Probe>> {
        let mut stmt = self.conn.prepare(
            "SELECT probe FROM codex_readings
             WHERE id IN (SELECT MAX(id) FROM codex_readings GROUP BY account)",
        )?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .iter()
            .filter_map(|json| serde_json::from_str(json).ok())
            .collect())
    }

    /// The newest reading of each account, in no particular order.
    pub fn latest_snapshots(&self) -> Result<Vec<Snapshot>> {
        self.query(
            "WHERE id IN (SELECT MAX(id) FROM snapshots GROUP BY token_name)",
            [],
        )
    }

    pub fn recent(&self, account: Option<&str>, limit: usize) -> Result<Vec<Snapshot>> {
        let limit = limit as i64;
        match account {
            Some(name) => self.query(
                "WHERE token_name = ?1 ORDER BY probed_at DESC LIMIT ?2",
                params![name, limit],
            ),
            None => self.query("ORDER BY probed_at DESC LIMIT ?1", params![limit]),
        }
    }

    pub fn for_account_since(&self, account: &str, since: DateTime<Utc>) -> Result<Vec<Snapshot>> {
        self.query(
            "WHERE token_name = ?1 AND probed_at >= ?2 ORDER BY probed_at ASC",
            params![account, since.to_rfc3339()],
        )
    }

    pub fn all_since(&self, since: DateTime<Utc>) -> Result<Vec<Snapshot>> {
        self.query(
            "WHERE probed_at >= ?1 ORDER BY probed_at ASC",
            params![since.to_rfc3339()],
        )
    }

    fn query(&self, tail: &str, params: impl rusqlite::Params) -> Result<Vec<Snapshot>> {
        const SELECT: &str =
            "SELECT token_name, probed_at, unified_status, utilization_5h, reset_5h,
                    utilization_7d, reset_7d, representative_claim,
                    overage_status, utilization_overage, reset_overage, error,
                    utilization_opus_7d, reset_opus_7d,
                    utilization_sonnet_7d, reset_sonnet_7d, model_usage_error,
                    model_usage_buckets, session_limit, signals
             FROM snapshots";
        let mut stmt = self.conn.prepare(&format!("{SELECT} {tail}"))?;
        let snapshots = stmt
            .query_map(params, map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(snapshots)
    }
}

fn map_row(row: &rusqlite::Row) -> rusqlite::Result<Snapshot> {
    let window = |used: Option<f64>, reset: Option<i64>| {
        used.map(|used| Window::new(used, reset.unwrap_or(0)))
    };
    let at: String = row.get(1)?;
    let at = DateTime::parse_from_rfc3339(&at)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(&at, "%Y-%m-%dT%H:%M:%S%.f").map(|ndt| ndt.and_utc())
        })
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    let mut buckets: Vec<Bucket> = row
        .get::<_, Option<String>>(17)?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    // Rows written before keys were canonical say `claudeopus48` for `opus48`.
    for bucket in &mut buckets {
        bucket.key = ModelKey::new(bucket.key.as_str());
    }
    // Legacy Opus/Sonnet columns, only when no bucket carries the family.
    for (family, used, reset) in [
        (
            "opus",
            row.get::<_, Option<f64>>(12)?,
            row.get::<_, Option<i64>>(13)?,
        ),
        (
            "sonnet",
            row.get::<_, Option<f64>>(14)?,
            row.get::<_, Option<i64>>(15)?,
        ),
    ] {
        let key = ModelKey::new(family);
        if let Some(window) = window(used, reset)
            && !buckets.iter().any(|bucket| bucket.key == key)
        {
            buckets.push(Bucket {
                label: family[..1].to_ascii_uppercase() + &family[1..],
                key,
                window,
                source: Source::Profile,
                share: None,
            });
        }
    }
    Ok(Snapshot {
        account: row.get(0)?,
        at,
        status: row.get(2)?,
        session: window(row.get(3)?, row.get(4)?),
        weekly: window(row.get(5)?, row.get(6)?),
        claim: row.get(7)?,
        overage_status: row.get(8)?,
        overage: window(row.get(9)?, row.get(10)?),
        error: row.get(11)?,
        profile_error: row.get(16)?,
        buckets,
        session_limit: row
            .get::<_, Option<String>>(18)?
            .and_then(|text| crate::quota::parse_wire(&text))
            .unwrap_or_default(),
        signals: row
            .get::<_, Option<String>>(19)?
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
    })
}

/// Burn rates over an account's snapshots, between consecutive readings of
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
    pub fn compute(account: &str, snapshots: &[Snapshot]) -> Self {
        let rates = |series: Series| -> Vec<f64> {
            snapshots
                .windows(2)
                .filter_map(|pair| {
                    let hours = (pair[1].at - pair[0].at).num_seconds() as f64 / 3600.0;
                    let (a, b) = (pair[0].window(series)?, pair[1].window(series)?);
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
            Some(snapshots.last()?.window(series)?.remaining() / rate)
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
    use crate::quota::{Overage, Verdict};

    fn reading() -> Reading {
        Reading {
            account: "acct".into(),
            at: 1_700_000_000,
            verdict: Verdict::Allowed,
            claim: Some("five_hour".into()),
            session: Some(Window::new(0.25, 10)),
            weekly: Some(Window::new(0.5, 20)),
            buckets: vec![Bucket {
                key: ModelKey::new("Opus 4.8"),
                label: "Opus 4.8".into(),
                window: Window::new(0.75, 30),
                source: Source::Profile,
                share: None,
            }],
            overage: Some(Overage {
                status: Some("ok".into()),
                window: Some(Window::new(0.1, 40)),
                ..Overage::default()
            }),
            error: None,
            profile_error: None,
            session_limit: SessionLimit::Absent,
            signals: Signals {
                claims: vec!["seven_day".into()],
                upgrade_paths: vec!["overage".into()],
                ..Signals::default()
            },
        }
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
        let rows = store.recent(Some("acct"), 5).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.at.timestamp(), 1_700_000_000);
        assert_eq!(row.status.as_deref(), Some("allowed"));
        assert_eq!(row.window(Series::Session), Some(Window::new(0.25, 10)));
        assert_eq!(row.window(Series::Overage), Some(Window::new(0.1, 40)));
        assert_eq!(
            row.window(Series::Model(&ModelKey::new("opus48"))),
            Some(Window::new(0.75, 30))
        );
        assert!(store.recent(Some("other"), 5).unwrap().is_empty());
        assert_eq!(row.session_limit, SessionLimit::Absent);
        assert_eq!(row.signals.claims, ["seven_day"]);
        assert_eq!(row.signals.upgrade_paths, ["overage"]);
    }

    #[test]
    fn a_database_from_0_3_gains_the_new_columns_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE snapshots (
                    id INTEGER PRIMARY KEY, token_name TEXT NOT NULL, probed_at TEXT NOT NULL,
                    unified_status TEXT, utilization_5h REAL, reset_5h INTEGER,
                    utilization_7d REAL, reset_7d INTEGER, representative_claim TEXT,
                    overage_status TEXT, utilization_overage REAL, reset_overage INTEGER,
                    error TEXT, utilization_opus_7d REAL, reset_opus_7d INTEGER,
                    utilization_sonnet_7d REAL, reset_sonnet_7d INTEGER,
                    model_usage_error TEXT, model_usage_buckets TEXT);
                 INSERT INTO snapshots (token_name, probed_at, utilization_7d, reset_7d,
                    model_usage_buckets)
                 VALUES ('old', '2026-10-01T00:00:00Z', 0.5, 9,
                    '[{\"key\":\"fable\",\"label\":\"Fable\",\"window\":{\"utilization\":0.2,\"reset\":9},\"source\":\"profile\"}]');",
            )
            .unwrap();
        }
        let store = Store::open_at(&path).unwrap();
        let old = store.recent(Some("old"), 1).unwrap().remove(0);
        assert_eq!(old.session_limit, SessionLimit::Unknown);
        assert!(old.signals.is_empty());
        assert_eq!(old.buckets[0].share, None);
        let mut new = reading();
        new.account = "old".into();
        new.at = 1_795_000_000;
        new.buckets[0].share = Some(0.5);
        store.insert(&new).unwrap();
        let rows = store.recent(Some("old"), 2).unwrap();
        assert_eq!(rows[0].buckets[0].share, Some(0.5));
        assert_eq!(rows[0].session_limit, SessionLimit::Absent);
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
            later.weekly = Some(Window::new(0.6, 20));
            store.insert(&later).unwrap();
            let probe = crate::codex::probe::Probe {
                account: "work".into(),
                codex_home: "/tmp/work".into(),
                at: 5,
                usage: None,
                error: None,
                last_refresh: None,
                email_hint: Some("a@b.c".into()),
            };
            store.insert_codex(&probe).unwrap();
            store
                .insert_codex(&crate::codex::probe::Probe { at: 9, ..probe })
                .unwrap();
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
        let latest = store.latest_snapshots().unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].weekly, Some(Window::new(0.6, 20)));
        let codex = store.latest_codex().unwrap();
        assert_eq!(codex.len(), 1);
        assert_eq!((codex[0].at, codex[0].email()), (9, Some("a@b.c")));
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
        newest.weekly = Some(Window::new(0.7, 20));
        writer.insert(&newest).unwrap();
        let reader = Store::open_read_only_at(&path).unwrap();
        assert_eq!(
            reader.latest_snapshots().unwrap()[0].weekly,
            Some(Window::new(0.7, 20))
        );
        drop(reader);
        drop(writer);

        // A database from before a table existed reads as an error, not a panic.
        let old = dir.path().join("old.db");
        Connection::open(&old)
            .unwrap()
            .execute_batch("CREATE TABLE snapshots (id INTEGER PRIMARY KEY);")
            .unwrap();
        let store = Store::open_read_only_at(&old).unwrap();
        assert!(store.latest_codex().is_err());
        assert!(store.latest_usage().is_err());
    }

    #[test]
    fn legacy_model_columns_fold_in_only_when_no_bucket_has_the_family() {
        let (_dir, store) = open();
        store
            .conn
            .execute(
                "INSERT INTO snapshots (token_name, probed_at, utilization_opus_7d, reset_opus_7d,
                    utilization_sonnet_7d, model_usage_buckets)
                 VALUES ('old', '2026-01-01T00:00:00Z', 0.4, 7, 0.2,
                    '[{\"key\":\"claudeopus48\",\"label\":\"Opus 4.8\",\"window\":{\"utilization\":0.9,\"reset\":1},\"source\":\"profile\"}]')",
                [],
            )
            .unwrap();
        let row = store.recent(None, 1).unwrap().remove(0);
        let keys: Vec<_> = row
            .buckets
            .iter()
            .map(|b| b.key.as_str().to_owned())
            .collect();
        assert_eq!(keys, ["opus48", "opus", "sonnet"]);
        assert_eq!(
            row.window(Series::Model(&ModelKey::new("sonnet"))),
            Some(Window::new(0.2, 0))
        );
    }
}

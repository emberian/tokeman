//! Probe history: one row per reading, in sqlite, for charts and stats.
//!
//! The schema is the one every row since 0.1.0 was written with; legacy
//! Opus/Sonnet columns are read into buckets and no longer written.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::time::Duration;

use crate::paths;
use crate::quota::{Bucket, ModelKey, Reading, Source, Window};

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
                model_usage_buckets TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_snapshots_token_time
                ON snapshots(token_name, probed_at);",
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
        self.conn.execute(
            "INSERT INTO snapshots (
                token_name, probed_at, unified_status, utilization_5h, reset_5h,
                utilization_7d, reset_7d, representative_claim,
                overage_status, utilization_overage, reset_overage, error,
                model_usage_error, model_usage_buckets
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            ],
        )?;
        Ok(())
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
                    model_usage_buckets
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
    })
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
            }],
            overage: Some(Overage {
                status: Some("ok".into()),
                window: Some(Window::new(0.1, 40)),
                ..Overage::default()
            }),
            error: None,
            profile_error: None,
        }
    }

    #[test]
    fn a_reading_round_trips_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(&dir.path().join("s.db")).unwrap();
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
    }

    #[test]
    fn legacy_model_columns_fold_in_only_when_no_bucket_has_the_family() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(&dir.path().join("s.db")).unwrap();
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

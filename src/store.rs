use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use crate::probe::{ModelQuotaBucket, ModelUsageSource, ProbeResult, Window};

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)] // fields are populated from DB and available for queries
pub struct Snapshot {
    pub token_name: String,
    pub probed_at: DateTime<Utc>,
    pub unified_status: Option<String>,
    pub utilization_5h: Option<f64>,
    pub reset_5h: Option<i64>,
    pub utilization_7d: Option<f64>,
    pub reset_7d: Option<i64>,
    pub representative_claim: Option<String>,
    pub overage_status: Option<String>,
    pub utilization_overage: Option<f64>,
    pub reset_overage: Option<i64>,
    pub error: Option<String>,
    pub utilization_opus_7d: Option<f64>,
    pub reset_opus_7d: Option<i64>,
    pub utilization_sonnet_7d: Option<f64>,
    pub reset_sonnet_7d: Option<i64>,
    pub model_usage_error: Option<String>,
    pub model_usage_buckets: Vec<ModelQuotaBucket>,
}

/// A plottable utilization series stored on a snapshot.
#[derive(Clone, Copy)]
pub enum Series<'a> {
    FiveHour,
    SevenDay,
    Overage,
    Model(&'a str),
}

impl Snapshot {
    /// Model buckets, with legacy Opus/Sonnet columns folded in by the same
    /// rule as `ModelUsage::buckets` (only when no bucket already has the key).
    pub fn model_buckets(&self) -> Vec<ModelQuotaBucket> {
        let mut buckets = self.model_usage_buckets.clone();
        for (key, label, utilization, reset) in [
            ("opus", "Opus", self.utilization_opus_7d, self.reset_opus_7d),
            (
                "sonnet",
                "Sonnet",
                self.utilization_sonnet_7d,
                self.reset_sonnet_7d,
            ),
        ] {
            if let Some(utilization) = utilization
                && !buckets.iter().any(|bucket| bucket.key == key)
            {
                buckets.push(ModelQuotaBucket {
                    key: key.into(),
                    label: label.into(),
                    window: Window {
                        utilization,
                        reset: reset.unwrap_or(0),
                    },
                    source: ModelUsageSource::Profile,
                });
            }
        }
        buckets
    }

    /// (utilization, reset) for one series, if this snapshot recorded it.
    pub fn window(&self, series: Series) -> Option<(f64, Option<i64>)> {
        match series {
            Series::FiveHour => Some((self.utilization_5h?, self.reset_5h)),
            Series::SevenDay => Some((self.utilization_7d?, self.reset_7d)),
            Series::Overage => Some((self.utilization_overage?, self.reset_overage)),
            Series::Model(key) => {
                if let Some(bucket) = self.model_usage_buckets.iter().find(|b| b.key == key) {
                    return Some((bucket.window.utilization, Some(bucket.window.reset)));
                }
                match key {
                    "opus" => Some((self.utilization_opus_7d?, self.reset_opus_7d)),
                    "sonnet" => Some((self.utilization_sonnet_7d?, self.reset_sonnet_7d)),
                    _ => None,
                }
            }
        }
    }
}

impl Store {
    pub fn open() -> Result<Self> {
        let path = Self::db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        crate::private_fs::set_mode(&path, 0o600)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    fn db_path() -> Result<PathBuf> {
        let base = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".local")
                    .join("share")
            });
        Ok(base.join("tokeman").join("snapshots.db"))
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
        // Existing databases predate these columns; ALTER fails harmlessly when present.
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

    pub fn insert(&self, result: &ProbeResult) -> Result<()> {
        fn split(window: Option<&Window>) -> (Option<f64>, Option<i64>) {
            (window.map(|w| w.utilization), window.map(|w| w.reset))
        }
        let quota = result.quota.as_ref();
        let (u5h, r5h) = split(quota.and_then(|q| q.session.as_ref()));
        let (u7d, r7d) = split(quota.and_then(|q| q.weekly.as_ref()));
        let (u_ov, r_ov) = split(quota.and_then(|q| q.overage.as_ref()));
        let usage = result.model_usage.as_ref();
        let (u_opus, r_opus) = split(usage.and_then(|u| u.opus_weekly.as_ref()));
        let (u_sonnet, r_sonnet) = split(usage.and_then(|u| u.sonnet_weekly.as_ref()));
        let model_usage_buckets = usage
            .map(|usage| serde_json::to_string(&usage.buckets()))
            .transpose()?;

        self.conn.execute(
            "INSERT INTO snapshots (
                token_name, probed_at,
                unified_status, utilization_5h, reset_5h,
                utilization_7d, reset_7d, representative_claim,
                overage_status, utilization_overage, reset_overage, error,
                utilization_opus_7d, reset_opus_7d,
                utilization_sonnet_7d, reset_sonnet_7d, model_usage_error,
                model_usage_buckets
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17, ?18
            )",
            params![
                result.token_name,
                result.probed_at.to_rfc3339(),
                quota.map(|q| q.status.as_str()),
                u5h,
                r5h,
                u7d,
                r7d,
                quota.map(|q| q.representative_claim.as_str()),
                quota.and_then(|q| q.overage_status.as_deref()),
                u_ov,
                r_ov,
                result.error.as_deref(),
                u_opus,
                r_opus,
                u_sonnet,
                r_sonnet,
                result.model_usage_error.as_deref(),
                model_usage_buckets,
            ],
        )?;
        Ok(())
    }

    pub fn recent(&self, token_name: Option<&str>, limit: usize) -> Result<Vec<Snapshot>> {
        let limit = limit as i64;
        match token_name {
            Some(name) => self.query(
                "WHERE token_name = ?1 ORDER BY probed_at DESC LIMIT ?2",
                params![name, limit],
            ),
            None => self.query("ORDER BY probed_at DESC LIMIT ?1", params![limit]),
        }
    }

    pub fn for_token_since(&self, token_name: &str, since: DateTime<Utc>) -> Result<Vec<Snapshot>> {
        self.query(
            "WHERE token_name = ?1 AND probed_at >= ?2 ORDER BY probed_at ASC",
            params![token_name, since.to_rfc3339()],
        )
    }

    pub fn all(&self) -> Result<Vec<Snapshot>> {
        self.query("ORDER BY probed_at ASC", [])
    }

    pub fn all_since(&self, since: DateTime<Utc>) -> Result<Vec<Snapshot>> {
        self.query(
            "WHERE probed_at >= ?1 ORDER BY probed_at ASC",
            params![since.to_rfc3339()],
        )
    }

    fn query(&self, tail: &str, params: impl rusqlite::Params) -> Result<Vec<Snapshot>> {
        const SELECT_COLS: &str =
            "SELECT token_name, probed_at, unified_status, utilization_5h, reset_5h,
                    utilization_7d, reset_7d, representative_claim,
                    overage_status, utilization_overage, reset_overage, error,
                    utilization_opus_7d, reset_opus_7d,
                    utilization_sonnet_7d, reset_sonnet_7d, model_usage_error,
                    model_usage_buckets
             FROM snapshots";
        let mut stmt = self.conn.prepare(&format!("{SELECT_COLS} {tail}"))?;
        let snapshots = stmt
            .query_map(params, Self::map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(snapshots)
    }

    fn map_row(row: &rusqlite::Row) -> rusqlite::Result<Snapshot> {
        let probed_at_str: String = row.get(1)?;
        let mut model_usage_buckets: Vec<ModelQuotaBucket> = row
            .get::<_, Option<String>>(17)?
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        // Rows written before keys were canonicalized say `claudeopus48`
        // where newer ones say `opus48`; one key keeps one history line.
        for bucket in &mut model_usage_buckets {
            bucket.key = crate::probe::model_key(&bucket.key);
        }
        let probed_at = DateTime::parse_from_rfc3339(&probed_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .or_else(|_| {
                NaiveDateTime::parse_from_str(&probed_at_str, "%Y-%m-%dT%H:%M:%S%.f")
                    .map(|ndt| ndt.and_utc())
            })
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;

        Ok(Snapshot {
            token_name: row.get(0)?,
            probed_at,
            unified_status: row.get(2)?,
            utilization_5h: row.get(3)?,
            reset_5h: row.get(4)?,
            utilization_7d: row.get(5)?,
            reset_7d: row.get(6)?,
            representative_claim: row.get(7)?,
            overage_status: row.get(8)?,
            utilization_overage: row.get(9)?,
            reset_overage: row.get(10)?,
            error: row.get(11)?,
            utilization_opus_7d: row.get(12)?,
            reset_opus_7d: row.get(13)?,
            utilization_sonnet_7d: row.get(14)?,
            reset_sonnet_7d: row.get(15)?,
            model_usage_error: row.get(16)?,
            model_usage_buckets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_model_columns_fold_in_only_when_bucket_missing() {
        let bucket = |key: &str, utilization: f64| ModelQuotaBucket {
            key: key.into(),
            label: key.into(),
            window: Window {
                utilization,
                reset: 7,
            },
            source: ModelUsageSource::ObservedRejection,
        };
        let snapshot = Snapshot {
            token_name: "t".into(),
            probed_at: Utc::now(),
            unified_status: None,
            utilization_5h: Some(0.1),
            reset_5h: None,
            utilization_7d: None,
            reset_7d: None,
            representative_claim: None,
            overage_status: None,
            utilization_overage: None,
            reset_overage: None,
            error: None,
            utilization_opus_7d: Some(0.2),
            reset_opus_7d: Some(1),
            utilization_sonnet_7d: Some(0.3),
            reset_sonnet_7d: Some(2),
            model_usage_error: None,
            model_usage_buckets: vec![bucket("opus", 0.9)],
        };
        let keys: Vec<_> = snapshot
            .model_buckets()
            .into_iter()
            .map(|b| (b.key, b.window.utilization))
            .collect();
        assert_eq!(keys, [("opus".into(), 0.9), ("sonnet".into(), 0.3)]);
        assert_eq!(snapshot.window(Series::Model("opus")), Some((0.9, Some(7))));
        assert_eq!(
            snapshot.window(Series::Model("sonnet")),
            Some((0.3, Some(2)))
        );
        assert_eq!(snapshot.window(Series::Model("fable")), None);
        assert_eq!(snapshot.window(Series::FiveHour), Some((0.1, None)));
        assert_eq!(snapshot.window(Series::SevenDay), None);
    }
}

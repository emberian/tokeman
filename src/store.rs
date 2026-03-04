use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{params, Connection};
use std::path::PathBuf;

use serde::Serialize;

use crate::probe::ProbeResult;

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
}

impl Store {
    pub fn open() -> Result<Self> {
        let path = Self::db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;
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
                reset_overage INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_snapshots_token_time
                ON snapshots(token_name, probed_at);",
        )?;
        Ok(())
    }

    pub fn insert(&self, result: &ProbeResult) -> Result<()> {
        let (unified_status, u5h, r5h, u7d, r7d, rep, ov_status, u_ov, r_ov) = match &result.quota {
            Some(q) => (
                Some(q.status.as_str()),
                q.session.as_ref().map(|w| w.utilization),
                q.session.as_ref().map(|w| w.reset),
                q.weekly.as_ref().map(|w| w.utilization),
                q.weekly.as_ref().map(|w| w.reset),
                Some(q.representative_claim.as_str()),
                q.overage_status.as_deref(),
                q.overage.as_ref().map(|w| w.utilization),
                q.overage.as_ref().map(|w| w.reset),
            ),
            None => (None, None, None, None, None, None, None, None, None),
        };

        self.conn.execute(
            "INSERT INTO snapshots (
                token_name, probed_at,
                unified_status, utilization_5h, reset_5h,
                utilization_7d, reset_7d, representative_claim,
                overage_status, utilization_overage, reset_overage
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                result.token_name,
                result.probed_at.to_rfc3339(),
                unified_status,
                u5h, r5h,
                u7d, r7d, rep,
                ov_status, u_ov, r_ov,
            ],
        )?;
        Ok(())
    }

    pub fn recent(&self, token_name: Option<&str>, limit: usize) -> Result<Vec<Snapshot>> {
        let (sql, has_filter) = match token_name {
            Some(_) => (
                "SELECT token_name, probed_at, unified_status, utilization_5h, reset_5h,
                        utilization_7d, reset_7d, representative_claim,
                        overage_status, utilization_overage, reset_overage
                 FROM snapshots WHERE token_name = ?1 ORDER BY probed_at DESC LIMIT ?2",
                true,
            ),
            None => (
                "SELECT token_name, probed_at, unified_status, utilization_5h, reset_5h,
                        utilization_7d, reset_7d, representative_claim,
                        overage_status, utilization_overage, reset_overage
                 FROM snapshots ORDER BY probed_at DESC LIMIT ?1",
                false,
            ),
        };

        let mut stmt = self.conn.prepare(sql)?;
        let rows = if has_filter {
            stmt.query_map(params![token_name.unwrap(), limit as i64], Self::map_row)?
        } else {
            stmt.query_map(params![limit as i64], Self::map_row)?
        };

        let mut snapshots = Vec::new();
        for row in rows {
            snapshots.push(row?);
        }
        Ok(snapshots)
    }

    pub fn for_token_since(
        &self,
        token_name: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<Snapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT token_name, probed_at, unified_status, utilization_5h, reset_5h,
                    utilization_7d, reset_7d, representative_claim,
                    overage_status, utilization_overage, reset_overage
             FROM snapshots WHERE token_name = ?1 AND probed_at >= ?2 ORDER BY probed_at ASC",
        )?;
        let rows = stmt.query_map(params![token_name, since.to_rfc3339()], Self::map_row)?;
        let mut snapshots = Vec::new();
        for row in rows {
            snapshots.push(row?);
        }
        Ok(snapshots)
    }

    pub fn all_since(&self, since: DateTime<Utc>) -> Result<Vec<Snapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT token_name, probed_at, unified_status, utilization_5h, reset_5h,
                    utilization_7d, reset_7d, representative_claim,
                    overage_status, utilization_overage, reset_overage
             FROM snapshots WHERE probed_at >= ?1 ORDER BY probed_at ASC",
        )?;
        let rows = stmt.query_map(params![since.to_rfc3339()], Self::map_row)?;
        let mut snapshots = Vec::new();
        for row in rows {
            snapshots.push(row?);
        }
        Ok(snapshots)
    }

    fn map_row(row: &rusqlite::Row) -> rusqlite::Result<Snapshot> {
        let probed_at_str: String = row.get(1)?;
        let probed_at = DateTime::parse_from_rfc3339(&probed_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .or_else(|_| {
                NaiveDateTime::parse_from_str(&probed_at_str, "%Y-%m-%dT%H:%M:%S%.f")
                    .map(|ndt| ndt.and_utc())
            })
            .unwrap_or_default();

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
        })
    }
}

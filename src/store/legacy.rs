//! Rows written before the `readings` table: the `snapshots` wide table
//! (Claude), read as it is and turned into a [`Reading`]. Nothing here
//! writes, creates or alters a table.

use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, Row, types::FromSql};
use serde::Deserialize;

use crate::quota::{
    ClaudeDetail, Detail, Limit, ModelKey, Overage, ReadError, Reading, Scope, Service, Signals,
    Source, Span, Verdict, Window, WindowStatus, parse_wire,
};

/// Whether this database has the table at all.
pub(super) fn has_table(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

/// A column by name: `None` when the row is NULL there, holds another type,
/// or the table predates the column.
fn get<T: FromSql>(row: &Row, name: &str) -> Option<T> {
    let index = row.as_ref().column_index(name).ok()?;
    row.get::<_, Option<T>>(index).ok().flatten()
}

/// `snapshots` rows matching `tail`, each as a Claude reading. A row whose
/// time does not parse is left out.
pub(super) fn snapshots(
    conn: &Connection,
    tail: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<Reading>> {
    if !has_table(conn, "snapshots") {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!("SELECT * FROM snapshots {tail}"))?;
    let rows = stmt
        .query_map(params, |row| Ok(snapshot(row)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().flatten().collect())
}

/// The time column's format, for comparing against it in SQL.
pub(super) fn snapshot_time(at: i64) -> String {
    DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

/// A model limit as the `model_usage_buckets` column holds it.
#[derive(Deserialize)]
struct StoredBucket {
    key: String,
    label: String,
    window: Window,
    source: Source,
    #[serde(default)]
    share: Option<f64>,
}

/// The per-window statuses the `signals` column carried beside the rest.
#[derive(Deserialize, Default)]
struct StoredStatuses {
    #[serde(default)]
    session_status: Option<WindowStatus>,
    #[serde(default)]
    weekly_status: Option<WindowStatus>,
}

fn snapshot(row: &Row) -> Option<Reading> {
    let at: String = get(row, "probed_at")?;
    let at = DateTime::parse_from_rfc3339(&at)
        .map(|time| time.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(&at, "%Y-%m-%dT%H:%M:%S%.f").map(|time| time.and_utc())
        })
        .ok()?
        .timestamp();
    let window = |used: &str, reset: &str| {
        get::<f64>(row, used).map(|used| Window::new(used, get(row, reset).unwrap_or(0)))
    };
    let signals: Option<String> = get(row, "signals");
    let statuses: StoredStatuses = signals
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default();
    let mut limits = Vec::new();
    for (span, window, status) in [
        (
            Span::FiveHour,
            window("utilization_5h", "reset_5h"),
            statuses.session_status,
        ),
        (
            Span::Weekly,
            window("utilization_7d", "reset_7d"),
            statuses.weekly_status,
        ),
    ] {
        if let Some(window) = window {
            limits.push(Limit {
                status,
                ..Limit::new(span, Scope::All, window, Source::Headers)
            });
        }
    }
    let buckets: Vec<StoredBucket> = get::<String>(row, "model_usage_buckets")
        .and_then(|json| serde_json::from_str::<Vec<serde_json::Value>>(&json).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|bucket| serde_json::from_value(bucket).ok())
        .collect();
    // Rows written before keys were canonical say `claudeopus48` for `opus48`.
    limits.extend(buckets.into_iter().map(|bucket| Limit {
        share: bucket.share,
        ..Limit::model(
            ModelKey::new(&bucket.key),
            &bucket.label,
            bucket.window,
            bucket.source,
        )
    }));
    let overage_status: Option<String> = get(row, "overage_status");
    let overage_window = window("utilization_overage", "reset_overage");
    Some(Reading {
        limits,
        error: get::<String>(row, "error").map(|text| claude_error(&text)),
        detail: Detail::Claude(ClaudeDetail {
            verdict: get::<String>(row, "unified_status")
                .map_or(Verdict::Unknown, |status| Verdict::parse(&status)),
            claim: get(row, "representative_claim"),
            session_limit: get::<String>(row, "session_limit")
                .and_then(|text| parse_wire(&text))
                .unwrap_or_default(),
            signals: signals
                .and_then(|json| serde_json::from_str::<Signals>(&json).ok())
                .unwrap_or_default(),
            overage: (overage_status.is_some() || overage_window.is_some()).then(|| Overage {
                status: overage_status,
                window: overage_window,
                ..Overage::default()
            }),
            profile_error: get(row, "model_usage_error"),
        }),
        ..Reading::empty(Service::Claude, &get::<String>(row, "token_name")?, at)
    })
}

/// The `error` column held the error's display text; read it back.
fn claude_error(text: &str) -> ReadError {
    if text == "no usable credential" {
        return ReadError::NoCredential;
    }
    if let Some(message) = text.strip_prefix("network: ") {
        return ReadError::Network {
            message: message.to_owned(),
        };
    }
    let http = text.strip_prefix("HTTP ").and_then(|rest| {
        let (status, body) = rest.split_once(": ")?;
        Some(ReadError::Http {
            status: status.parse().ok()?,
            body: body.to_owned(),
        })
    });
    http.unwrap_or_else(|| ReadError::Other {
        message: text.to_owned(),
    })
}

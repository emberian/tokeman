//! Rows written before the `readings` table, read as they are and turned
//! into [`Reading`]s: the `snapshots` wide table (Claude), `codex_readings`
//! (one JSON probe per row) and `service_readings` (one JSON usage per row).
//! Only `compact` reads them, once, on its way to dropping them. The shapes
//! below are the ones those builds stored; they exist only to be read.

use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, Row, types::FromSql};
use serde::Deserialize;

use crate::codex::probe::{Block, Extra, limits_of};
use crate::quota::{
    Amount, Balance, ClaudeDetail, CodexDetail, Credits, Detail, Limit, ModelKey, ModelUsage,
    Overage, ReadError, Reading, ResetCreditCount, Scope, Service, Signals, Source, Span,
    SpendControl, Verdict, Window, WindowStatus, parse_wire,
};

/// One old table, and the columns its rows are walked by: the ones that
/// name the account, then the time, in the order of the index it was created
/// with. `id` breaks ties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OldTable {
    pub table: &'static str,
    key: &'static [&'static str],
    index: &'static str,
}

const OLD_TABLES: [OldTable; 3] = [
    OldTable {
        table: "snapshots",
        key: &["token_name", "probed_at"],
        index: "idx_snapshots_token_time",
    },
    OldTable {
        table: "codex_readings",
        key: &["account", "at"],
        index: "idx_codex_readings_account_time",
    },
    OldTable {
        table: "service_readings",
        key: &["service", "account", "at"],
        index: "idx_service_readings_account_time",
    },
];

/// One row of an old table: where it stands in the walk, and what it says.
pub(super) struct OldRow {
    /// The key columns and the id, to continue after this row.
    pub place: Vec<Sql>,
    /// `None` for a row that does not read: an unparseable time, a document
    /// in a shape this build does not know, or one that names another
    /// service, account or time than the row that holds it.
    pub reading: Option<Reading>,
}

/// The old tables this database has.
pub(super) fn present(conn: &Connection) -> rusqlite::Result<Vec<&'static OldTable>> {
    let mut present = Vec::new();
    for table in &OLD_TABLES {
        if super::has_table(conn, table.table)? {
            present.push(table);
        }
    }
    Ok(present)
}

impl OldTable {
    pub fn count(&self, conn: &Connection) -> rusqlite::Result<u64> {
        conn.query_row(&format!("SELECT COUNT(*) FROM {}", self.table), [], |row| {
            row.get(0)
        })
    }

    /// The index the walk follows, for a table created without it.
    pub fn ensure_index(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(&format!(
            "CREATE INDEX IF NOT EXISTS {} ON {}({})",
            self.index,
            self.table,
            self.key.join(", ")
        ))
    }

    /// Up to `limit` rows after `place` (from the first row when `None`), in
    /// key order: one account's rows together, oldest first.
    pub fn rows(
        &self,
        conn: &Connection,
        place: Option<&[Sql]>,
        limit: usize,
    ) -> rusqlite::Result<Vec<OldRow>> {
        let columns = format!("{}, id", self.key.join(", "));
        let after = match place {
            Some(place) => {
                let holes: Vec<String> = (1..=place.len()).map(|n| format!("?{n}")).collect();
                format!("WHERE ({columns}) > ({})", holes.join(", "))
            }
            None => String::new(),
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT * FROM {} {after} ORDER BY {columns} LIMIT {limit}",
            self.table
        ))?;
        let params = rusqlite::params_from_iter(place.unwrap_or_default().iter());
        let rows = stmt.query_map(params, |row| {
            let mut place = Vec::with_capacity(self.key.len() + 1);
            for column in self.key.iter().chain(&["id"]) {
                place.push(row.get::<_, Sql>(*column)?);
            }
            Ok(OldRow {
                place,
                reading: self.reading(row),
            })
        })?;
        rows.collect()
    }

    fn reading(&self, row: &Row) -> Option<Reading> {
        let document = |column: &str| get::<String>(row, column);
        let (reading, service) = match self.table {
            "snapshots" => return snapshot(row),
            "codex_readings" => (
                serde_json::from_str::<StoredProbe>(&document("probe")?)
                    .ok()?
                    .reading(),
                Service::Codex,
            ),
            _ => {
                let service: Service = parse_wire(&document("service")?)?;
                if !matches!(service, Service::Antigravity | Service::Zai | Service::Kimi) {
                    return None;
                }
                (
                    serde_json::from_str::<StoredUsage>(&document("usage")?)
                        .ok()?
                        .reading(),
                    service,
                )
            }
        };
        (reading.service == service
            && Some(&reading.account) == get::<String>(row, "account").as_ref()
            && Some(reading.at) == get::<i64>(row, "at"))
        .then_some(reading)
    }
}

/// A column by name: `None` when the row is NULL there, holds another type,
/// or the table predates the column.
fn get<T: FromSql>(row: &Row, name: &str) -> Option<T> {
    let index = row.as_ref().column_index(name).ok()?;
    row.get::<_, Option<T>>(index).ok().flatten()
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
    if let Some(window) = window("utilization_overage", "reset_overage") {
        limits.push(Limit::extra_usage(window, Source::Headers));
    }
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
            overage: overage_status.map(|status| Overage {
                status: Some(status),
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

// `service_readings.usage`: a 0.3.x `providers::Usage`.

#[derive(Deserialize)]
struct StoredMeter {
    #[serde(default)]
    group: Option<String>,
    label: String,
    window: Window,
    #[serde(default)]
    span_secs: Option<i64>,
    #[serde(default)]
    amount: Option<Amount>,
}

#[derive(Deserialize)]
struct StoredUsage {
    service: Service,
    account: String,
    at: i64,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    meters: Vec<StoredMeter>,
    #[serde(default)]
    balances: Vec<Balance>,
    #[serde(default)]
    error: Option<String>,
}

impl StoredUsage {
    fn reading(self) -> Reading {
        Reading {
            plan: self.plan,
            limits: self
                .meters
                .into_iter()
                .map(|meter| Limit {
                    label: meter.label,
                    amount: meter.amount,
                    ..Limit::new(
                        Span::from_seconds(meter.span_secs),
                        meter.group.map_or(Scope::All, Scope::Group),
                        meter.window,
                        Source::Poll,
                    )
                })
                .collect(),
            balances: self.balances,
            error: self.error.map(|message| ReadError::Other { message }),
            ..Reading::empty(self.service, &self.account, self.at)
        }
    }
}

// `codex_readings.probe`: a 0.3.x `codex::probe::Probe`.

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredSpan {
    FiveHour,
    Weekly,
    Seconds(i64),
    Unknown,
}

#[derive(Deserialize)]
struct StoredSpanWindow {
    span: StoredSpan,
    #[serde(flatten)]
    window: Window,
}

#[derive(Deserialize)]
struct StoredRateLimit {
    #[serde(default)]
    allowed: Option<bool>,
    #[serde(default)]
    limit_reached: Option<bool>,
    #[serde(default)]
    windows: Vec<StoredSpanWindow>,
}

impl StoredRateLimit {
    fn block(self) -> Block {
        Block {
            allowed: self.allowed,
            limit_reached: self.limit_reached,
            windows: self
                .windows
                .into_iter()
                .map(|window| {
                    let span = match window.span {
                        StoredSpan::FiveHour => Span::FiveHour,
                        StoredSpan::Weekly => Span::Weekly,
                        StoredSpan::Seconds(seconds) => Span::from_seconds(Some(seconds)),
                        StoredSpan::Unknown => Span::Unknown,
                    };
                    (span, window.window)
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct StoredExtra {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    feature: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    limit: Option<StoredRateLimit>,
}

#[derive(Deserialize, Default)]
struct StoredCodexUsage {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    limit: Option<StoredRateLimit>,
    #[serde(default)]
    credits: Option<Credits>,
    #[serde(default)]
    limit_reason: Option<String>,
    #[serde(default)]
    extra: Vec<StoredExtra>,
    #[serde(default)]
    models: Vec<ModelUsage>,
    #[serde(default)]
    spend_control: Option<SpendControl>,
    #[serde(default)]
    reset_credits: Option<ResetCreditCount>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredCodexError {
    AuthFile { message: String },
    AuthFileHasNoAccessToken,
    Network { message: String },
    Challenged { status: u16, attempts: u32 },
    Http { status: u16, body: String },
    Decode { message: String },
}

#[derive(Deserialize)]
struct StoredProbe {
    account: String,
    at: i64,
    #[serde(default)]
    usage: Option<StoredCodexUsage>,
    #[serde(default)]
    error: Option<StoredCodexError>,
    #[serde(default)]
    email_hint: Option<String>,
}

impl StoredProbe {
    fn reading(self) -> Reading {
        let usage = self.usage.unwrap_or_default();
        let shared = usage.limit.map(StoredRateLimit::block);
        let extras: Vec<Extra> = usage
            .extra
            .into_iter()
            .map(|extra| Extra {
                name: extra.name,
                feature: extra.feature,
                model: extra.model,
                block: extra.limit.map(StoredRateLimit::block),
            })
            .collect();
        Reading {
            plan: usage.plan,
            limits: limits_of(shared.as_ref(), &extras),
            error: self.error.map(|error| match error {
                StoredCodexError::AuthFile { message } => ReadError::Credential { message },
                StoredCodexError::AuthFileHasNoAccessToken => ReadError::Credential {
                    message: "auth.json has no access_token".to_owned(),
                },
                StoredCodexError::Network { message } => ReadError::Network { message },
                StoredCodexError::Challenged { status, attempts } => {
                    ReadError::Edge { status, attempts }
                }
                StoredCodexError::Http { status, body } => ReadError::Http { status, body },
                StoredCodexError::Decode { message } => ReadError::Decode { message },
            }),
            detail: Detail::Codex(CodexDetail {
                email: usage.email.or(self.email_hint),
                allowed: shared.as_ref().map(Block::allows),
                limit_reason: usage.limit_reason,
                credits: usage.credits,
                models: usage.models,
                spend_control: usage.spend_control,
                reset_credits: usage.reset_credits,
            }),
            ..Reading::empty(Service::Codex, &self.account, self.at)
        }
    }
}

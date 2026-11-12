//! Rows written before the `readings` table, read as they are and turned
//! into [`Reading`]s: the `snapshots` wide table (Claude), `codex_readings`
//! (one JSON probe per row) and `service_readings` (one JSON usage per row).
//! Nothing here writes, creates or alters a table. The shapes below are the
//! ones those builds stored; they exist only to be read.

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

/// Which rows to read.
#[derive(Debug, Clone, Copy)]
pub(super) enum Rows<'a> {
    /// The newest row of each account.
    Latest,
    /// The newest rows, newest first.
    Recent {
        account: Option<&'a str>,
        limit: usize,
    },
    /// Rows at or after `since` (epoch seconds), oldest first.
    Since {
        account: Option<&'a str>,
        since: i64,
    },
}

/// Whether this database has the table at all.
pub(super) fn has_table(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

/// One old table: its name, how it names the account and the time, and
/// whether it holds several services.
struct Table {
    name: &'static str,
    account: &'static str,
    time: &'static str,
    /// The time column holds RFC 3339 text rather than epoch seconds.
    text_time: bool,
    /// The table has a `service` column, and these rows are this one's.
    service: Option<Service>,
}

impl Table {
    fn of(service: Service) -> Self {
        match service {
            Service::Claude => Self {
                name: "snapshots",
                account: "token_name",
                time: "probed_at",
                text_time: true,
                service: None,
            },
            Service::Codex => Self {
                name: "codex_readings",
                account: "account",
                time: "at",
                text_time: false,
                service: None,
            },
            Service::Antigravity | Service::Zai | Service::Kimi => Self {
                name: "service_readings",
                account: "account",
                time: "at",
                text_time: false,
                service: Some(service),
            },
        }
    }

    /// `WHERE ... ORDER BY ... LIMIT ...` for `rows`, and its parameters.
    fn tail(&self, rows: Rows) -> (String, Vec<Sql>) {
        let Self {
            name,
            account,
            time,
            ..
        } = self;
        let mut conditions = Vec::new();
        let mut args = Vec::new();
        if let Some(service) = self.service {
            args.push(Sql::Text(service.name().into()));
            conditions.push(format!("service = ?{}", args.len()));
        }
        let named = match rows {
            Rows::Latest => None,
            Rows::Recent { account, .. } | Rows::Since { account, .. } => account,
        };
        if let Some(named) = named {
            args.push(Sql::Text(named.into()));
            conditions.push(format!("{account} = ?{}", args.len()));
        }
        let order = match rows {
            Rows::Latest => {
                let group = match self.service {
                    Some(_) => format!("service, {account}"),
                    None => (*account).to_owned(),
                };
                conditions.push(format!(
                    "id IN (SELECT MAX(id) FROM {name} GROUP BY {group})"
                ));
                String::new()
            }
            Rows::Recent { limit, .. } => {
                args.push(Sql::Integer(limit as i64));
                format!("ORDER BY {time} DESC, id DESC LIMIT ?{}", args.len())
            }
            Rows::Since { since, .. } => {
                args.push(if self.text_time {
                    Sql::Text(
                        DateTime::from_timestamp(since, 0)
                            .unwrap_or_default()
                            .to_rfc3339(),
                    )
                } else {
                    Sql::Integer(since)
                });
                conditions.push(format!("{time} >= ?{}", args.len()));
                format!("ORDER BY {time} ASC, id ASC")
            }
        };
        let filter = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        (format!("{filter} {order}"), args)
    }
}

/// The rows of `service` written before the `readings` table. A row that
/// no longer reads (an unparseable time, a document in a shape this build
/// does not know) is left out; a database without the table has none.
pub(super) fn read(
    conn: &Connection,
    service: Service,
    rows: Rows,
) -> rusqlite::Result<Vec<Reading>> {
    let table = Table::of(service);
    if !has_table(conn, table.name) {
        return Ok(Vec::new());
    }
    let (tail, args) = table.tail(rows);
    let params = rusqlite::params_from_iter(args);
    match service {
        Service::Claude => {
            let mut stmt = conn.prepare(&format!("SELECT * FROM snapshots {tail}"))?;
            let rows = stmt
                .query_map(params, |row| Ok(snapshot(row)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().flatten().collect())
        }
        Service::Codex => documents(conn, "probe", "codex_readings", &tail, params, |json| {
            serde_json::from_str::<StoredProbe>(json)
                .ok()
                .map(StoredProbe::reading)
        }),
        Service::Antigravity | Service::Zai | Service::Kimi => {
            documents(conn, "usage", "service_readings", &tail, params, |json| {
                serde_json::from_str::<StoredUsage>(json)
                    .ok()
                    .map(StoredUsage::reading)
            })
        }
    }
}

/// One JSON document per row, converted.
fn documents(
    conn: &Connection,
    column: &str,
    table: &str,
    tail: &str,
    params: impl rusqlite::Params,
    convert: impl Fn(&str) -> Option<Reading>,
) -> rusqlite::Result<Vec<Reading>> {
    let mut stmt = conn.prepare(&format!("SELECT {column} FROM {table} {tail}"))?;
    let rows = stmt
        .query_map(params, |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.iter().filter_map(|json| convert(json)).collect())
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

use anyhow::Result as AnyResult;
use axum::{
    Json,
    extract::Query,
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse},
    routing::get,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::display::truncate_chars;
use crate::store::{Snapshot, Store};

async fn index() -> impl IntoResponse {
    (
        [(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self' https://d3js.org 'unsafe-inline'; \
                 style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
                 object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
            ),
        )],
        Html(include_str!("web_ui.html")),
    )
}

type ApiError = (StatusCode, String);

#[derive(Deserialize)]
struct SnapshotQuery {
    since_hours: Option<f64>,
}

async fn api_snapshots(Query(q): Query<SnapshotQuery>) -> Result<Json<Vec<Snapshot>>, ApiError> {
    if q.since_hours
        .is_some_and(|hours| !hours.is_finite() || hours <= 0.0 || hours > 24.0 * 3650.0)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "since_hours must be between 0 and 87600".into(),
        ));
    }
    let db = Store::open().map_err(internal_error)?;
    let snapshots = match q.since_hours {
        Some(hours) => {
            let cutoff = Utc::now() - Duration::milliseconds((hours * 3_600_000.0) as i64);
            db.all_since(cutoff).map_err(internal_error)?
        }
        None => db.all().map_err(internal_error)?,
    };
    Ok(Json(snapshots))
}

fn internal_error(error: impl std::fmt::Display) -> ApiError {
    eprintln!("tokeman web API error: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal dashboard error".into(),
    )
}

// ── Session timeline API ──

fn claude_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

#[derive(Serialize)]
struct SessionInfo {
    session_id: String,
    project: String,
    project_short: String,
    start_time: Option<String>,
    duration_minutes: Option<f64>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    lines_added: Option<u64>,
    lines_removed: Option<u64>,
    files_modified: Option<u64>,
    git_commits: Option<u64>,
    assistant_message_count: Option<u64>,
    user_message_count: Option<u64>,
    tool_counts: Option<HashMap<String, u64>>,
    tool_errors: Option<u64>,
    user_interruptions: Option<u64>,
    uses_task_agent: Option<bool>,
    languages: Option<HashMap<String, u64>>,
    // facets
    brief_summary: Option<String>,
    underlying_goal: Option<String>,
    outcome: Option<String>,
    session_type: Option<String>,
    claude_helpfulness: Option<String>,
    primary_success: Option<String>,
    friction_detail: Option<String>,
    first_prompt: Option<String>,
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn u64_field(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(Value::as_u64)
}

fn u64_map(v: &Value, k: &str) -> Option<HashMap<String, u64>> {
    v.get(k).and_then(Value::as_object).map(|obj| {
        obj.iter()
            .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
            .collect()
    })
}

async fn api_sessions() -> Json<Vec<SessionInfo>> {
    let Some(base) = claude_dir() else {
        return Json(vec![]);
    };

    let meta_dir = base.join("usage-data").join("session-meta");
    let facets_dir = base.join("usage-data").join("facets");

    let mut sessions = Vec::new();

    let Ok(entries) = std::fs::read_dir(&meta_dir) else {
        return Json(sessions);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let sid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&content) else {
            continue;
        };

        let project = meta
            .get("project_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let project_short = project.split('/').next_back().unwrap_or("").to_string();

        let first_prompt_raw = meta
            .get("first_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let first_prompt = truncate_chars(first_prompt_raw, 200).to_string();

        // Load facets if available
        let facets = std::fs::read_to_string(facets_dir.join(format!("{sid}.json")))
            .ok()
            .and_then(|c| serde_json::from_str::<Value>(&c).ok());
        let facet = |k: &str| facets.as_ref().and_then(|v| str_field(v, k));

        sessions.push(SessionInfo {
            session_id: sid,
            project,
            project_short,
            start_time: str_field(&meta, "start_time"),
            duration_minutes: meta.get("duration_minutes").and_then(Value::as_f64),
            input_tokens: u64_field(&meta, "input_tokens"),
            output_tokens: u64_field(&meta, "output_tokens"),
            lines_added: u64_field(&meta, "lines_added"),
            lines_removed: u64_field(&meta, "lines_removed"),
            files_modified: u64_field(&meta, "files_modified"),
            git_commits: u64_field(&meta, "git_commits"),
            assistant_message_count: u64_field(&meta, "assistant_message_count"),
            user_message_count: u64_field(&meta, "user_message_count"),
            tool_counts: u64_map(&meta, "tool_counts"),
            tool_errors: u64_field(&meta, "tool_errors"),
            user_interruptions: u64_field(&meta, "user_interruptions"),
            uses_task_agent: meta.get("uses_task_agent").and_then(Value::as_bool),
            languages: u64_map(&meta, "languages"),
            brief_summary: facet("brief_summary"),
            underlying_goal: facet("underlying_goal"),
            outcome: facet("outcome"),
            session_type: facet("session_type"),
            claude_helpfulness: facet("claude_helpfulness"),
            primary_success: facet("primary_success"),
            friction_detail: facet("friction_detail"),
            first_prompt: Some(first_prompt),
        });
    }

    // Sort by start_time
    sessions.sort_by(|a, b| a.start_time.cmp(&b.start_time));

    Json(sessions)
}

#[derive(Deserialize)]
struct SessionDetailQuery {
    session_id: String,
}

#[derive(Serialize, Default)]
struct SessionEvent {
    timestamp: String,
    #[serde(rename = "type")]
    event_type: String,
    // For user/assistant messages
    role: Option<String>,
    text_preview: Option<String>,
    // For tool uses
    tool_name: Option<String>,
    // For agent progress
    agent_id: Option<String>,
    agent_prompt: Option<String>,
    // For system events
    subtype: Option<String>,
    duration_ms: Option<u64>,
}

async fn api_session_detail(
    Query(q): Query<SessionDetailQuery>,
) -> Result<Json<Vec<SessionEvent>>, ApiError> {
    if !valid_session_id(&q.session_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid session_id".into()));
    }
    let Some(base) = claude_dir() else {
        return Ok(Json(vec![]));
    };

    // Find the conversation JSONL for this session
    let projects_dir = base.join("projects");
    let mut jsonl_path = None;

    if let Ok(projects) = std::fs::read_dir(&projects_dir) {
        for proj in projects.flatten() {
            let candidate = proj.path().join(format!("{}.jsonl", q.session_id));
            if candidate.exists() {
                jsonl_path = Some(candidate);
                break;
            }
        }
    }

    let Some(path) = jsonl_path else {
        return Ok(Json(vec![]));
    };

    let Ok(content) = std::fs::read_to_string(&path) else {
        return Ok(Json(vec![]));
    };

    let mut events = Vec::new();

    for line in content.lines() {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        let msg_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let timestamp = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if timestamp.is_empty() {
            continue;
        }

        match msg_type {
            "user" => {
                events.push(SessionEvent {
                    timestamp,
                    event_type: "user".into(),
                    role: Some("user".into()),
                    text_preview: Some(extract_text_preview(&obj)),
                    ..Default::default()
                });
            }
            "assistant" => {
                let content = obj.pointer("/message/content").and_then(Value::as_array);

                if let Some(blocks) = content {
                    // Emit text blocks
                    let mut text_parts = Vec::new();
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    text_parts.push(truncate_chars(t, 200));
                                }
                            }
                            "tool_use" => {
                                let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                                events.push(SessionEvent {
                                    timestamp: timestamp.clone(),
                                    event_type: "tool_use".into(),
                                    tool_name: Some(name.to_string()),
                                    ..Default::default()
                                });
                            }
                            _ => {}
                        }
                    }
                    if !text_parts.is_empty() {
                        events.push(SessionEvent {
                            timestamp,
                            event_type: "assistant".into(),
                            role: Some("assistant".into()),
                            text_preview: Some(truncate_chars(&text_parts.join(" "), 300).into()),
                            ..Default::default()
                        });
                    }
                }
            }
            "progress" => {
                if let Some(data) = obj
                    .get("data")
                    .filter(|d| d.get("type").and_then(Value::as_str) == Some("agent_progress"))
                {
                    events.push(SessionEvent {
                        timestamp,
                        event_type: "agent".into(),
                        agent_id: str_field(data, "agentId"),
                        agent_prompt: data
                            .get("prompt")
                            .and_then(Value::as_str)
                            .map(|s| truncate_chars(s, 200).to_string()),
                        ..Default::default()
                    });
                }
                // Skip bash_progress / hook_progress (too noisy)
            }
            "system" => {
                events.push(SessionEvent {
                    timestamp,
                    event_type: "system".into(),
                    subtype: Some(str_field(&obj, "subtype").unwrap_or_default()),
                    duration_ms: u64_field(&obj, "durationMs"),
                    ..Default::default()
                });
            }
            _ => {}
        }
    }

    Ok(Json(events))
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn extract_text_preview(obj: &Value) -> String {
    match obj.pointer("/message/content") {
        Some(Value::String(s)) => truncate_chars(s, 200).to_string(),
        Some(Value::Array(arr)) => {
            for block in arr {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = block.get("text").and_then(Value::as_str)
                {
                    return truncate_chars(t, 200).to_string();
                }
                // tool_result content
                if let Some(c) = block.get("content").and_then(Value::as_str) {
                    return truncate_chars(c, 200).to_string();
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

pub async fn serve(port: u16, open_browser: bool) -> AnyResult<()> {
    let app = axum::Router::new()
        .route("/", get(index))
        .route("/api/snapshots", get(api_snapshots))
        .route("/api/sessions", get(api_sessions))
        .route("/api/session-detail", get(api_session_detail));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");

    println!("Serving tokeman dashboard at {url}");

    if open_browser {
        open_url(&url);
    }

    axum::serve(listener, app).await?;
    Ok(())
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let command = ("open", Vec::<&str>::new());
    #[cfg(target_os = "linux")]
    let command = ("xdg-open", Vec::<&str>::new());
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start"]);

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        let _ = std::process::Command::new(command.0)
            .args(command.1)
            .arg(url)
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::valid_session_id;

    #[test]
    fn session_id_rejects_path_traversal() {
        assert!(valid_session_id("a7d06cb1-5593-4cb0-aec6-a3f24831ee7f"));
        assert!(!valid_session_id("../../settings"));
        assert!(!valid_session_id("slash/name"));
        assert!(!valid_session_id(""));
    }
}

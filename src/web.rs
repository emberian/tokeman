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
use std::collections::HashMap;
use std::path::PathBuf;

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
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) else {
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
        let first_prompt = first_prompt_raw.chars().take(200).collect::<String>();

        // Load facets if available
        let facets = std::fs::read_to_string(facets_dir.join(format!("{sid}.json")))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok());

        let f = |v: &serde_json::Value, k: &str| -> Option<String> {
            v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string())
        };
        let fu64 =
            |v: &serde_json::Value, k: &str| -> Option<u64> { v.get(k).and_then(|x| x.as_u64()) };
        let ff64 =
            |v: &serde_json::Value, k: &str| -> Option<f64> { v.get(k).and_then(|x| x.as_f64()) };
        let fbool =
            |v: &serde_json::Value, k: &str| -> Option<bool> { v.get(k).and_then(|x| x.as_bool()) };

        let tool_counts: Option<HashMap<String, u64>> = meta
            .get("tool_counts")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                    .collect()
            });

        let languages: Option<HashMap<String, u64>> = meta
            .get("languages")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                    .collect()
            });

        sessions.push(SessionInfo {
            session_id: sid,
            project: project.clone(),
            project_short,
            start_time: f(&meta, "start_time"),
            duration_minutes: ff64(&meta, "duration_minutes"),
            input_tokens: fu64(&meta, "input_tokens"),
            output_tokens: fu64(&meta, "output_tokens"),
            lines_added: fu64(&meta, "lines_added"),
            lines_removed: fu64(&meta, "lines_removed"),
            files_modified: fu64(&meta, "files_modified"),
            git_commits: fu64(&meta, "git_commits"),
            assistant_message_count: fu64(&meta, "assistant_message_count"),
            user_message_count: fu64(&meta, "user_message_count"),
            tool_counts,
            tool_errors: fu64(&meta, "tool_errors"),
            user_interruptions: fu64(&meta, "user_interruptions"),
            uses_task_agent: fbool(&meta, "uses_task_agent"),
            languages,
            brief_summary: facets.as_ref().and_then(|f_val| f(f_val, "brief_summary")),
            underlying_goal: facets
                .as_ref()
                .and_then(|f_val| f(f_val, "underlying_goal")),
            outcome: facets.as_ref().and_then(|f_val| f(f_val, "outcome")),
            session_type: facets.as_ref().and_then(|f_val| f(f_val, "session_type")),
            claude_helpfulness: facets
                .as_ref()
                .and_then(|f_val| f(f_val, "claude_helpfulness")),
            primary_success: facets
                .as_ref()
                .and_then(|f_val| f(f_val, "primary_success")),
            friction_detail: facets
                .as_ref()
                .and_then(|f_val| f(f_val, "friction_detail")),
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

#[derive(Serialize)]
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
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        let msg_type = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let timestamp = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if timestamp.is_empty() {
            continue;
        }

        match msg_type.as_str() {
            "user" => {
                let text = extract_text_preview(&obj);
                events.push(SessionEvent {
                    timestamp,
                    event_type: "user".into(),
                    role: Some("user".into()),
                    text_preview: Some(text),
                    tool_name: None,
                    agent_id: None,
                    agent_prompt: None,
                    subtype: None,
                    duration_ms: None,
                });
            }
            "assistant" => {
                let msg = obj.get("message").cloned().unwrap_or_default();
                let content = msg.get("content").and_then(|c| c.as_array());

                if let Some(blocks) = content {
                    // Emit text blocks
                    let mut text_parts = Vec::new();
                    for block in blocks {
                        let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match bt {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                    text_parts.push(t.chars().take(200).collect::<String>());
                                }
                            }
                            "tool_use" => {
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                events.push(SessionEvent {
                                    timestamp: timestamp.clone(),
                                    event_type: "tool_use".into(),
                                    role: None,
                                    text_preview: None,
                                    tool_name: Some(name),
                                    agent_id: None,
                                    agent_prompt: None,
                                    subtype: None,
                                    duration_ms: None,
                                });
                            }
                            _ => {}
                        }
                    }
                    if !text_parts.is_empty() {
                        events.push(SessionEvent {
                            timestamp: timestamp.clone(),
                            event_type: "assistant".into(),
                            role: Some("assistant".into()),
                            text_preview: Some(text_parts.join(" ").chars().take(300).collect()),
                            tool_name: None,
                            agent_id: None,
                            agent_prompt: None,
                            subtype: None,
                            duration_ms: None,
                        });
                    }
                }
            }
            "progress" => {
                let data = obj.get("data").cloned().unwrap_or_default();
                let dt = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if dt == "agent_progress" {
                    let agent_id = data
                        .get("agentId")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let prompt = data
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .map(|s| s.chars().take(200).collect());
                    events.push(SessionEvent {
                        timestamp,
                        event_type: "agent".into(),
                        role: None,
                        text_preview: None,
                        tool_name: None,
                        agent_id,
                        agent_prompt: prompt,
                        subtype: None,
                        duration_ms: None,
                    });
                }
                // Skip bash_progress / hook_progress (too noisy)
            }
            "system" => {
                let subtype = obj
                    .get("subtype")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let dur = obj.get("durationMs").and_then(|v| v.as_u64());
                events.push(SessionEvent {
                    timestamp,
                    event_type: "system".into(),
                    role: None,
                    text_preview: None,
                    tool_name: None,
                    agent_id: None,
                    agent_prompt: None,
                    subtype: Some(subtype),
                    duration_ms: dur,
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

fn extract_text_preview(obj: &serde_json::Value) -> String {
    let msg = obj.get("message").cloned().unwrap_or_default();
    let content = msg.get("content");
    match content {
        Some(serde_json::Value::String(s)) => s.chars().take(200).collect(),
        Some(serde_json::Value::Array(arr)) => {
            for block in arr {
                if block.get("type").and_then(|v| v.as_str()) == Some("text")
                    && let Some(t) = block.get("text").and_then(|v| v.as_str())
                {
                    return t.chars().take(200).collect();
                }
                // tool_result content
                if let Some(c) = block.get("content").and_then(|v| v.as_str()) {
                    return c.chars().take(200).collect();
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

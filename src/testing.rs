//! Builders every test module shares. Test-only.
#![allow(dead_code)]

use crate::account::{Account, CodexAccount, Login};
use crate::quota::{Bucket, ModelKey, ProbeError, Reading, Source, Verdict, Window};
use crate::ui::chart::Point;

pub const HOUR_MS: i64 = 3_600_000;
/// An expiry no test clock reaches.
pub const FAR_FUTURE_MS: i64 = i64::MAX / 2;

/// An allowed reading with both shared windows and no resets.
pub fn reading(name: &str, session_used: f64, weekly_used: f64) -> Reading {
    reading_with_resets(name, session_used, weekly_used, 0, 0)
}

pub fn reading_with_resets(
    name: &str,
    session_used: f64,
    weekly_used: f64,
    session_reset: i64,
    weekly_reset: i64,
) -> Reading {
    Reading {
        account: name.into(),
        at: 0,
        verdict: Verdict::Allowed,
        claim: Some("five_hour".into()),
        session: Some(Window::new(session_used, session_reset)),
        weekly: Some(Window::new(weekly_used, weekly_reset)),
        buckets: Vec::new(),
        overage: None,
        error: None,
        profile_error: None,
    }
}

/// A per-model weekly bucket keyed by any spelling of the model.
pub fn bucket(model: &str, label: &str, used: f64, source: Source) -> Bucket {
    Bucket {
        key: ModelKey::new(model),
        label: label.into(),
        window: Window::new(used, 0),
        source,
    }
}

/// `reading` plus one profile bucket.
pub fn reading_with_bucket(
    name: &str,
    session_used: f64,
    weekly_used: f64,
    model: &str,
    label: &str,
    used: f64,
) -> Reading {
    let mut r = reading(name, session_used, weekly_used);
    r.buckets.push(bucket(model, label, used, Source::Profile));
    r
}

/// A definitive refusal.
pub fn refused(name: &str) -> Reading {
    Reading::failed(
        name,
        0,
        ProbeError::Http {
            status: 401,
            body: "nope".into(),
        },
    )
}

/// An account with a setup key and a live, refreshable, full-scope login.
pub fn login_account(name: &str, expires_at_ms: i64) -> Account {
    Account {
        name: name.into(),
        setup_key: Some(format!("sk-ant-oat01-setup-{name}")),
        login: Some(Login {
            access_token: Some(format!("sk-ant-oat01-access-{name}")),
            refresh_token: Some("sk-ant-ort01-refresh".into()),
            expires_at: Some(expires_at_ms),
            obtained_at: Some(expires_at_ms - 8 * HOUR_MS),
            scopes: vec!["user:inference".into(), "user:profile".into()],
            error: None,
        }),
    }
}

/// An account with a setup key only.
pub fn setup_account(name: &str) -> Account {
    Account {
        name: name.into(),
        setup_key: Some(format!("sk-ant-oat01-setup-{name}")),
        login: None,
    }
}

/// An account holding a captured access token with no refresh material: good
/// for profile reads, never installed. Old account files can hold these.
pub fn profile_only_account(name: &str, token: &str) -> Account {
    Account {
        name: name.into(),
        setup_key: Some(format!("sk-ant-oat01-setup-{name}")),
        login: Some(Login {
            access_token: Some(token.into()),
            ..Login::default()
        }),
    }
}

/// A Codex profile registered at `codex_home`, neither parked nor access-only.
pub fn codex_account(name: &str, codex_home: &str) -> CodexAccount {
    CodexAccount {
        name: name.into(),
        codex_home: codex_home.into(),
        ..CodexAccount::default()
    }
}

/// One chart point.
pub fn point(at: i64, remaining: f64, resets_at: Option<i64>) -> Point {
    Point {
        at,
        remaining,
        resets_at,
    }
}

/// Serve `router` on a free loopback port for the rest of the test; returns
/// its base URL.
pub async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("local address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

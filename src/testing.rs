//! Builders every test module shares. Test-only.
#![allow(dead_code)]

use crate::account::{Account, CodexAccount, Login};
use crate::quota::{
    ClaudeDetail, Detail, Limit, ModelKey, ReadError, Reading, Scope, Service, SessionLimit,
    Source, Span, Verdict, Window, WindowStatus,
};
use crate::ui::chart::Point;

pub const HOUR_MS: i64 = 3_600_000;
/// An expiry no test clock reaches.
pub const FAR_FUTURE_MS: i64 = i64::MAX / 2;

/// An allowed Claude reading with both shared windows and no resets.
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
    let shared =
        |span, used, reset| Limit::new(span, Scope::All, Window::new(used, reset), Source::Headers);
    Reading {
        limits: vec![
            shared(Span::FiveHour, session_used, session_reset),
            shared(Span::Weekly, weekly_used, weekly_reset),
        ],
        detail: Detail::Claude(ClaudeDetail {
            verdict: Verdict::Allowed,
            claim: Some("five_hour".into()),
            session_limit: SessionLimit::Present,
            ..ClaudeDetail::default()
        }),
        ..Reading::empty(Service::Claude, name, 0)
    }
}

/// A model's weekly limit, keyed by any spelling of the model.
pub fn model_limit(model: &str, label: &str, used: f64, source: Source) -> Limit {
    Limit::model(ModelKey::new(model), label, Window::new(used, 0), source)
}

/// `reading` plus one model limit from the profile.
pub fn reading_with_model(
    name: &str,
    session_used: f64,
    weekly_used: f64,
    model: &str,
    label: &str,
    used: f64,
) -> Reading {
    let mut r = reading(name, session_used, weekly_used);
    r.limits
        .push(model_limit(model, label, used, Source::Profile));
    r
}

/// A definitive refusal.
pub fn refused(name: &str) -> Reading {
    Reading::failed(
        Service::Claude,
        name,
        0,
        ReadError::Http {
            status: 401,
            body: "nope".into(),
        },
    )
}

impl Reading {
    /// Replace the account-wide window of one length; `None` removes it.
    fn set_shared(&mut self, span: Span, window: Option<Window>) {
        match window {
            Some(window) => self.set_window(span, Scope::All, window, Source::Headers),
            None => self.remove_limit(span, &Scope::All),
        }
    }

    pub fn set_session(&mut self, window: Option<Window>) {
        self.set_shared(Span::FiveHour, window);
    }

    pub fn set_weekly(&mut self, window: Option<Window>) {
        self.set_shared(Span::Weekly, window);
    }

    /// Set the status of a limit that exists.
    pub fn set_status(&mut self, span: Span, scope: &Scope, status: WindowStatus) {
        self.limits
            .iter_mut()
            .find(|limit| limit.span == span && &limit.scope == scope)
            .expect("the limit exists")
            .status = Some(status);
    }
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
            logged_in_at: Some(expires_at_ms - 8 * HOUR_MS),
            refresh_expires_at: None,
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

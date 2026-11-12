//! The stored view: everything tokeman last read about every account, put
//! together without a request and without a write. `tokeman list --json`
//! prints it, and both trays draw from it; the daemon (or an explicit probe)
//! is what keeps the stores it reads fresh.

use serde::Serialize;

use crate::account::{Account, CodexAccount, Fleet};
use crate::anthropic::profile::Cache;
use crate::anthropic::resets;
use crate::claude::settings as claude_settings;
use crate::codex::probe::{self as codex_probe, State};
use crate::daemon::{lock, service};
use crate::providers::ServiceAccount;
use crate::quota::{Limit, Reading, ResetCreditCount, Service, SessionLimit, Source, Window};
use crate::store::{Series, Store};
use crate::ui::text;

/// Whether anything is keeping the stored reads fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Monitor {
    Running,
    Paused,
    /// No background service: only explicit probes add readings.
    NotInstalled,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Overview {
    /// Epoch seconds when this view was put together.
    pub at: i64,
    pub monitor: Monitor,
    pub default_account: Option<String>,
    pub target_model: Option<String>,
    pub claude: Vec<ClaudeView>,
    pub resets_on_offer: Vec<ResetOffer>,
    pub codex: Vec<CodexView>,
    pub services: Vec<ServiceView>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaudeView {
    pub name: String,
    pub is_default: bool,
    /// Which credential the account offers Claude, without secrets.
    pub credential: String,
    /// The login's state and when it ends, as `tokeman list` words it.
    pub login: String,
    /// The refusal or the end date alone, for a small surface.
    pub login_note: Option<String>,
    /// The login is refused, ended, or close to ending.
    pub login_warning: bool,
    /// Epoch seconds when the refresh token dies, when reported.
    pub login_ends_at: Option<i64>,
    pub reading: Option<ClaudeReading>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaudeReading {
    /// Epoch seconds.
    pub at: i64,
    pub status: Option<String>,
    pub error: Option<String>,
    pub session: Option<Window>,
    /// `absent` is an account with no 5h limit; `unknown` is one whose
    /// reading does not say.
    pub session_limit: SessionLimit,
    pub weekly: Option<Window>,
    pub overage: Option<Window>,
    pub buckets: Vec<BucketView>,
    /// What the headers said beyond the windows, one line each.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BucketView {
    pub label: String,
    /// Three characters for a gauge label.
    pub short: String,
    pub window: Window,
    pub source: Source,
    /// The limit's size as a fraction of the weekly limit, when it is one.
    pub share: Option<f64>,
    /// Room for the model as a fraction of the weekly limit.
    pub room: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResetOffer {
    pub account: String,
    pub offer: String,
    /// Epoch seconds of the read the offer comes from.
    pub read_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexView {
    pub name: String,
    pub label: Option<String>,
    pub parked: bool,
    pub reading: Option<CodexReading>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NamedWindow {
    /// `5H`, `WEEKLY`, or another span.
    pub label: String,
    pub window: Window,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexExtra {
    pub name: String,
    pub limited: bool,
    pub windows: Vec<NamedWindow>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodexReading {
    /// Epoch seconds.
    pub at: i64,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// `ok`, `limited (...)` or the error, as `tokeman codex list` words it.
    pub state: String,
    pub viable: bool,
    pub windows: Vec<NamedWindow>,
    pub extra: Vec<CodexExtra>,
    pub reset_credits: Option<ResetCreditCount>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServiceView {
    pub name: String,
    pub service: Service,
    pub service_label: &'static str,
    /// The read on one line, as `tokeman list` words it.
    pub summary: Option<String>,
    pub usage: Option<Reading>,
}

/// What the stores hold, gathered before the view is built.
#[derive(Debug, Clone, Default)]
pub struct Stored {
    /// The newest reading of each account of each service.
    pub readings: Vec<Reading>,
    /// Per account: when its reset offers were read, and the usable ones.
    pub offers: Vec<(String, i64, Vec<String>)>,
    pub default_account: Option<String>,
    pub target_model: Option<String>,
}

impl Stored {
    fn reading(&self, service: Service, account: &str) -> Option<&Reading> {
        self.readings
            .iter()
            .find(|reading| reading.service == service && reading.account == account)
    }

    /// Read the history, the reset cache and Claude's settings. Nothing is
    /// created or written; a store that does not exist yet reads as empty.
    pub fn load(fleet: &Fleet, now: i64) -> Self {
        let store = Store::open_read_only().ok();
        let cache = Cache::default();
        let settings = claude_settings::load().ok();
        Self {
            readings: store
                .as_ref()
                .and_then(|store| store.latest().ok())
                .unwrap_or_default(),
            offers: fleet
                .accounts
                .iter()
                .filter_map(|account| {
                    let (read_at, offers) = resets::stored_offers(&cache, &account.name, now)?;
                    Some((account.name.clone(), read_at, offers))
                })
                .collect(),
            default_account: settings
                .as_ref()
                .and_then(claude_settings::configured_token)
                .and_then(|value| fleet.account_holding(value))
                .map(|account| account.name.clone()),
            target_model: settings
                .as_ref()
                .and_then(claude_settings::target_model)
                .map(str::to_owned),
        }
    }
}

impl Overview {
    /// The view of `fleet` from what is on disk now.
    pub fn stored(fleet: &Fleet, now: i64) -> Self {
        let monitor = if !service::is_installed() {
            Monitor::NotInstalled
        } else if lock::is_paused() {
            Monitor::Paused
        } else {
            Monitor::Running
        };
        Self::build(fleet, Stored::load(fleet, now), monitor, now)
    }

    pub fn build(fleet: &Fleet, stored: Stored, monitor: Monitor, now: i64) -> Self {
        let now_ms = now * 1000;
        Self {
            at: now,
            monitor,
            claude: fleet
                .accounts
                .iter()
                .map(|account| {
                    claude_view(
                        account,
                        stored.reading(Service::Claude, &account.name),
                        stored.default_account.as_deref() == Some(account.name.as_str()),
                        now_ms,
                    )
                })
                .collect(),
            resets_on_offer: stored
                .offers
                .iter()
                .flat_map(|(account, read_at, offers)| {
                    offers.iter().map(|offer| ResetOffer {
                        account: account.clone(),
                        offer: offer.clone(),
                        read_at: *read_at,
                    })
                })
                .collect(),
            codex: fleet
                .codex
                .iter()
                .map(|account| codex_view(account, stored.reading(Service::Codex, &account.name)))
                .collect(),
            services: fleet
                .services
                .iter()
                .map(|account| {
                    service_view(account, stored.reading(account.service, &account.name))
                })
                .collect(),
            default_account: stored.default_account,
            target_model: stored.target_model,
        }
    }

    /// The Claude account with the most weekly room among those whose last
    /// reading allows work, for a status dot.
    pub fn best_weekly_remaining(&self) -> Option<f64> {
        self.claude
            .iter()
            .filter_map(|account| account.reading.as_ref())
            .filter(|reading| {
                reading.error.is_none() && reading.status.as_deref() != Some("rejected")
            })
            .filter_map(|reading| reading.weekly)
            .map(|weekly| weekly.remaining())
            .reduce(f64::max)
    }
}

fn claude_view(
    account: &Account,
    snapshot: Option<&Reading>,
    is_default: bool,
    now_ms: i64,
) -> ClaudeView {
    let login = account.login.as_ref();
    ClaudeView {
        name: account.name.clone(),
        is_default,
        credential: claude_settings::describe_credential(account, now_ms),
        login: text::describe_login(account, now_ms),
        login_note: text::login_note(account, now_ms),
        login_warning: login.is_some_and(|login| login.error.is_some() || login.is_ageing(now_ms)),
        login_ends_at: login
            .and_then(|login| login.refresh_expires_at)
            .map(|at| at / 1000),
        reading: snapshot.map(|snapshot| {
            let claude = snapshot.claude();
            let weekly = snapshot.weekly();
            ClaudeReading {
                at: snapshot.at,
                status: claude.map(|claude| claude.verdict.as_str().to_owned()),
                error: snapshot
                    .error
                    .as_ref()
                    .map(|error| brief_error(&error.to_string())),
                session: snapshot.session(),
                session_limit: snapshot.session_limit(),
                weekly,
                overage: Series::Overage.window(snapshot),
                buckets: snapshot
                    .models()
                    .map(|limit| bucket_view(limit, weekly.as_ref()))
                    .collect(),
                notes: claude
                    .map(|claude| text::signal_lines(&claude.signals))
                    .unwrap_or_default(),
            }
        }),
    }
}

/// `HTTP 403: OAuth authentication is currently not allowed ...` for an
/// error that carries the API's JSON body; any other error as it is.
fn brief_error(error: &str) -> String {
    let message = error.find('{').and_then(|start| {
        let body: serde_json::Value = serde_json::from_str(&error[start..]).ok()?;
        let message = body["error"]["message"].as_str()?;
        Some(format!("{}{message}", &error[..start]))
    });
    message.unwrap_or_else(|| crate::util::one_line(error, 200))
}

pub fn bucket_view(bucket: &Limit, weekly: Option<&Window>) -> BucketView {
    BucketView {
        label: bucket.label.clone(),
        short: text::short_bucket_label(&bucket.label),
        window: bucket.window,
        source: bucket.source,
        share: bucket.share,
        room: bucket.room(weekly),
    }
}

fn named(limit: &Limit) -> NamedWindow {
    NamedWindow {
        label: limit.span.label().to_uppercase(),
        window: limit.window,
    }
}

fn codex_view(account: &CodexAccount, probe: Option<&Reading>) -> CodexView {
    CodexView {
        name: account.name.clone(),
        label: account.label.clone(),
        parked: account.parked,
        reading: probe.map(|probe| {
            let codex = probe.codex();
            CodexReading {
                at: probe.at,
                email: codex.and_then(|codex| codex.email.clone()),
                plan: probe.plan.clone(),
                state: State::of(probe).to_string(),
                viable: codex_probe::is_viable(probe),
                windows: probe.shared().map(named).collect(),
                extra: probe
                    .scoped()
                    .into_iter()
                    .map(|(_, limits)| CodexExtra {
                        name: limits[0].label.clone(),
                        limited: limits.iter().any(|limit| limit.is_rejected()),
                        windows: limits.into_iter().map(named).collect(),
                    })
                    .collect(),
                reset_credits: codex.and_then(|codex| codex.reset_credits),
            }
        }),
    }
}

fn service_view(account: &ServiceAccount, usage: Option<&Reading>) -> ServiceView {
    ServiceView {
        name: account.name.clone(),
        service: account.service,
        service_label: account.service.label(),
        summary: usage.map(text::usage_summary),
        usage: usage.cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::Span;
    use crate::testing::{codex_account, login_account, setup_account};

    const NOW: i64 = 1_794_000_000;
    const DAY_MS: i64 = 86_400_000;

    fn snapshot(account: &str, weekly: f64) -> Reading {
        let mut reading = crate::testing::reading(account, 0.0, 0.0);
        reading.at = NOW - 60;
        reading.set_session(None);
        reading.set_weekly(Some(Window::new(weekly, NOW + 3600)));
        reading.claude_mut().session_limit = SessionLimit::Unknown;
        reading
    }

    fn fleet() -> Fleet {
        let mut ageing = login_account("ultra", NOW * 1000 + 3_600_000);
        let login = ageing.login.as_mut().unwrap();
        login.refresh_expires_at = Some(NOW * 1000 + 2 * DAY_MS);
        Fleet {
            accounts: vec![ageing, setup_account("plain")],
            codex: vec![codex_account("work", "~/.codex-homes/work")],
            services: vec![ServiceAccount::with_key_file(
                Service::Kimi,
                "kimi",
                "~/.kimikey",
            )],
            ..Fleet::default()
        }
    }

    #[test]
    fn an_account_without_a_5h_limit_differs_from_one_that_did_not_say() {
        let mut absent = snapshot("ultra", 0.4);
        absent.claude_mut().session_limit = SessionLimit::Absent;
        let mut fable = crate::testing::model_limit("fable", "Fable", 0.5, Source::Profile);
        fable.share = Some(0.5);
        absent.limits.push(fable);
        let stored = Stored {
            readings: vec![absent, snapshot("plain", 0.9)],
            default_account: Some("ultra".into()),
            ..Stored::default()
        };
        let view = Overview::build(&fleet(), stored, Monitor::Running, NOW);
        let ultra = &view.claude[0];
        assert!(ultra.is_default);
        let reading = ultra.reading.as_ref().unwrap();
        assert_eq!(reading.session, None);
        assert_eq!(reading.session_limit, SessionLimit::Absent);
        // Half the bucket left, the bucket is half the weekly: a quarter of it.
        assert_eq!(reading.buckets[0].short, "Fab");
        assert!((reading.buckets[0].room - 0.25).abs() < 1e-12);
        assert_eq!(reading.at, NOW - 60);
        let plain = view.claude[1].reading.as_ref().unwrap();
        assert_eq!(plain.session_limit, SessionLimit::Unknown);
        assert!((view.best_weekly_remaining().unwrap() - 0.6).abs() < 1e-12);

        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["monitor"], "running");
        assert_eq!(json["claude"][0]["reading"]["session_limit"], "absent");
        assert_eq!(json["claude"][1]["reading"]["session_limit"], "unknown");
        assert_eq!(json["claude"][0]["reading"]["weekly"]["utilization"], 0.4);
    }

    #[test]
    fn a_login_near_its_end_warns_and_an_unread_account_has_no_reading() {
        let view = Overview::build(&fleet(), Stored::default(), Monitor::NotInstalled, NOW);
        let ultra = &view.claude[0];
        assert!(ultra.login_warning);
        assert_eq!(ultra.login_ends_at, Some(NOW + 2 * 86_400));
        assert!(ultra.login.contains("login ends"), "{}", ultra.login);
        let note = ultra.login_note.as_deref().unwrap();
        assert!(
            note.starts_with("login ends") && note.ends_with("soon"),
            "{note}"
        );
        assert_eq!(ultra.reading, None);
        let plain = &view.claude[1];
        assert!(!plain.login_warning);
        assert_eq!(plain.login_ends_at, None);
        assert_eq!(plain.login_note, None);
        assert_eq!(view.best_weekly_remaining(), None);
        assert_eq!(view.codex[0].reading, None);
        assert_eq!(view.services[0].usage, None);
        assert_eq!(
            serde_json::to_value(&view).unwrap()["monitor"],
            "not-installed"
        );
    }

    #[test]
    fn an_api_error_body_is_shown_as_its_message() {
        assert_eq!(
            brief_error(
                r#"HTTP 403: {"type":"error","error":{"type":"permission_error","message":"OAuth authentication is currently not allowed for this organization."},"request_id":"req_1"}"#
            ),
            "HTTP 403: OAuth authentication is currently not allowed for this organization."
        );
        assert_eq!(brief_error("network: timed out"), "network: timed out");
        assert_eq!(brief_error("HTTP 502: {not json"), "HTTP 502: {not json");
    }

    #[test]
    fn codex_windows_are_named_by_length_and_carry_the_credit_count() {
        let shared = |span, used, reset| {
            Limit::new(
                span,
                crate::quota::Scope::All,
                Window::new(used, reset),
                Source::Poll,
            )
        };
        let mut probe = Reading::empty(Service::Codex, "work", NOW - 600);
        probe.limits = vec![
            shared(Span::Weekly, 0.42, NOW + 86_400),
            shared(Span::FiveHour, 0.1, NOW + 3_600),
        ];
        probe.detail = crate::quota::Detail::Codex(crate::quota::CodexDetail {
            email: Some("a@b.c".into()),
            allowed: Some(true),
            reset_credits: Some(ResetCreditCount {
                available: 2,
                applicable: Some(0),
            }),
            ..Default::default()
        });
        // A Claude account of the same name is another account.
        let namesake = snapshot("work", 0.5);
        let stored = Stored {
            readings: vec![namesake, probe],
            ..Stored::default()
        };
        let view = Overview::build(&fleet(), stored, Monitor::Paused, NOW);
        let reading = view.codex[0].reading.as_ref().unwrap();
        let labels: Vec<_> = reading.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["WEEKLY", "5H"]);
        assert_eq!(reading.state, "ok");
        assert!(reading.viable);
        assert_eq!(reading.reset_credits.unwrap().available, 2);
        assert_eq!(reading.email.as_deref(), Some("a@b.c"));
    }

    #[test]
    fn services_and_reset_offers_come_from_what_was_stored() {
        let usage = Reading {
            plan: Some("Vivace".into()),
            limits: vec![crate::providers::limit(
                None,
                "weekly",
                Some(Span::WEEK_SECS),
                Window::new(0.21, 0),
                None,
            )],
            ..Reading::empty(Service::Kimi, "kimi", NOW - 120)
        };
        let stored = Stored {
            // A read of another service under the same name is not this account's.
            readings: vec![
                crate::providers::failed(Service::Zai, "kimi", NOW, "wrong service"),
                usage.clone(),
            ],
            offers: vec![
                (
                    "ultra".into(),
                    NOW - 900,
                    vec!["weekly reset, usable now".into()],
                ),
                ("plain".into(), NOW - 900, Vec::new()),
            ],
            ..Stored::default()
        };
        let view = Overview::build(&fleet(), stored, Monitor::Running, NOW);
        assert_eq!(view.services[0].usage.as_ref(), Some(&usage));
        assert_eq!(view.services[0].summary.as_deref(), Some("weekly 79%"));
        assert_eq!(view.services[0].service_label, "Kimi Code");
        assert_eq!(
            view.resets_on_offer,
            [ResetOffer {
                account: "ultra".into(),
                offer: "weekly reset, usable now".into(),
                read_at: NOW - 900,
            }]
        );
    }
}

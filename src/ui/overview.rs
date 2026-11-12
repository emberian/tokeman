//! The fleet view: everything known about every account of every service,
//! in one structure. `tokeman list --json` is its serialization, and every
//! surface (the text reports, `--watch`, the tray) draws it.
//!
//! A view is built from [`Facts`]. [`Facts::on_disk`] gathers them without
//! a request or a write, which is what a surface shows while the background
//! service keeps the stores fresh; a caller that has just read the accounts
//! itself puts its readings on top with [`Facts::with_fresh`].

use serde::Serialize;

use crate::account::{Account, Fleet};
use crate::anthropic::profile::Cache;
use crate::anthropic::resets;
use crate::claude::sessions::Session;
use crate::claude::settings as claude_settings;
use crate::daemon::cadence::Cadence;
use crate::daemon::{lock, service};
use crate::observe::{self, ObservedLimit};
use crate::policy::{self, Mode};
use crate::quota::{Level, ModelKey, Reading, Service, Verdict};
use crate::store::Store;
use crate::ui::text;

/// Whether anything is keeping the stored reads fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Monitor {
    Running,
    /// The service is installed and rotation is paused: Claude is not read.
    Paused,
    /// The service is installed and no daemon is running: nothing reads.
    Stopped,
    /// No background service: only explicit reads add readings.
    NotInstalled,
}

impl Monitor {
    pub fn now() -> Self {
        Self::of(
            lock::daemon_alive(),
            lock::is_paused(),
            service::is_installed(),
        )
    }

    /// A daemon that is alive reads, however it was started (launchd, or a
    /// supervisor of the user's own where there is no launchd).
    pub fn of(alive: bool, paused: bool, installed: bool) -> Self {
        match (alive, paused, installed) {
            (true, false, _) => Self::Running,
            (true, true, _) => Self::Paused,
            (false, _, true) => Self::Stopped,
            (false, _, false) => Self::NotInstalled,
        }
    }

    /// Whether the background service reads every account by itself, so a
    /// surface only has to show what is stored.
    pub fn keeps_fresh(self) -> bool {
        self == Self::Running
    }
}

/// Where a view's readings come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// The history: what the last reads said, each as old as its `at`.
    Stored,
    /// Read for this view.
    Fresh,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Overview {
    /// Epoch seconds when this view was put together.
    pub at: i64,
    pub origin: Origin,
    /// Epoch seconds of the newest reading in the view.
    pub read_at: Option<i64>,
    pub monitor: Monitor,
    /// The account Claude Code's settings offer now, when tokeman manages it.
    pub default_account: Option<String>,
    /// The model Claude Code starts on, as its settings spell it.
    pub target_model: Option<String>,
    /// The most weekly room among the Claude accounts whose last reading
    /// allows work: what a status dot shows.
    pub best_weekly_remaining: Option<f64>,
    pub rotation: Rotation,
    /// Every configured account: Claude, then Codex, then the other
    /// services, each in configured order.
    pub accounts: Vec<AccountView>,
    pub resets_on_offer: Vec<ResetOffer>,
}

/// The state of Claude's rotation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rotation {
    pub mode: Mode,
    pub probe_interval_secs: u64,
    /// The floors of `mode`: the default changes at or below them.
    pub min_session_remaining: f64,
    pub min_weekly_remaining: f64,
    /// Claude's settings offer a credential that is no configured account's.
    pub default_is_unmanaged: bool,
    /// How many accounts have profile limits and how many rejections stand
    /// (`tokeman rotate status`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<String>,
    /// Whether and how the startup model's own limit is sampled
    /// (`tokeman rotate status`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling: Option<String>,
    /// Live Claude processes and the account each spends. `None` when no
    /// census was taken for this view, or it failed.
    pub live_sessions: Option<Vec<Session>>,
}

/// One account of one service.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountView {
    pub service: Service,
    pub service_label: &'static str,
    pub name: String,
    pub label: Option<String>,
    /// The tool holds this account now: Claude Code's default, agy's login.
    pub held: bool,
    /// A Codex profile kept out of `codex best` and `codex use`.
    pub parked: bool,
    /// Claude: the credential and the login's state.
    pub login: Option<LoginView>,
    /// The reading's state in a word, and the level to colour it with.
    pub state: Option<State>,
    /// Claude: whether the account is above the rotation's floors for the
    /// startup model. Codex: whether the backend accepts work on it.
    pub viable: Option<bool>,
    /// Why the last read produced no quota, on one line.
    pub error: Option<String>,
    /// What the reading says beyond its limits, one line each.
    pub notes: Vec<String>,
    /// The last reading, stored or fresh; `None` for an account never read.
    pub reading: Option<Reading>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct State {
    pub word: String,
    pub level: Option<Level>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoginView {
    /// Which credential the account offers Claude, without secrets.
    pub credential: String,
    /// The login's state and when it ends, as `tokeman list` words it.
    pub text: String,
    /// The refusal or the end date alone, for a small surface.
    pub note: Option<String>,
    /// The login is refused, ended, or close to ending.
    pub warning: bool,
    /// Epoch seconds when the refresh token dies, when reported.
    pub ends_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResetOffer {
    pub account: String,
    pub offer: String,
    /// Epoch seconds of the read the offer comes from.
    pub read_at: i64,
}

/// What a view is built from.
#[derive(Debug, Clone)]
pub struct Facts {
    pub origin: Origin,
    pub monitor: Monitor,
    /// The newest reading of each account.
    pub readings: Vec<Reading>,
    pub offers: Vec<ResetOffer>,
    pub default_account: Option<String>,
    pub default_is_unmanaged: bool,
    pub target_model: Option<String>,
    /// The Antigravity account agy is signed in as.
    pub agy_account: Option<String>,
    pub mode: Mode,
    pub probe_interval_secs: Option<u64>,
    /// Rejections on record whose reset is still ahead.
    pub observed: Vec<ObservedLimit>,
    pub coverage: Option<String>,
    pub sampling: Option<String>,
    pub live_sessions: Option<Vec<Session>>,
}

impl Default for Facts {
    fn default() -> Self {
        Self {
            origin: Origin::Stored,
            monitor: Monitor::NotInstalled,
            readings: Vec::new(),
            offers: Vec::new(),
            default_account: None,
            default_is_unmanaged: false,
            target_model: None,
            agy_account: None,
            mode: Mode::Normal,
            probe_interval_secs: None,
            observed: Vec::new(),
            coverage: None,
            sampling: None,
            live_sessions: None,
        }
    }
}

impl Facts {
    /// What is on disk now: the history, the reset cache, Claude's settings,
    /// the daemon's cycle state and the rejections on record. Nothing is
    /// requested, created or written; a store that does not exist yet reads
    /// as empty.
    pub fn on_disk(fleet: &Fleet, now: i64) -> Self {
        let cache = Cache::default();
        let settings = claude_settings::load().ok();
        let token = settings
            .as_ref()
            .and_then(claude_settings::configured_token);
        let default_account = token
            .and_then(|value| fleet.account_holding(value))
            .map(|account| account.name.clone());
        let cadence = Cadence::load();
        Self {
            origin: Origin::Stored,
            monitor: Monitor::now(),
            readings: Store::open_read_only()
                .and_then(|store| store.latest(&fleet.account_keys()))
                .unwrap_or_default(),
            offers: fleet
                .accounts
                .iter()
                .filter_map(|account| {
                    let (read_at, offers) = resets::stored_offers(&cache, &account.name, now)?;
                    Some(offers.into_iter().map(move |offer| ResetOffer {
                        account: account.name.clone(),
                        offer,
                        read_at,
                    }))
                })
                .flatten()
                .collect(),
            default_is_unmanaged: token.is_some() && default_account.is_none(),
            default_account,
            target_model: settings
                .as_ref()
                .and_then(claude_settings::target_model)
                .map(str::to_owned),
            agy_account: None,
            mode: cadence.mode.unwrap_or(Mode::Normal),
            probe_interval_secs: cadence.probe_interval_secs,
            observed: observe::active_limits(),
            coverage: None,
            sampling: None,
            live_sessions: None,
        }
    }

    /// Put readings made just now over the stored ones of the same accounts.
    pub fn with_fresh(mut self, fresh: Vec<Reading>) -> Self {
        for reading in fresh {
            match self.readings.iter_mut().find(|stored| {
                stored.service == reading.service && stored.account == reading.account
            }) {
                Some(stored) => *stored = reading,
                None => self.readings.push(reading),
            }
        }
        self.origin = Origin::Fresh;
        self
    }

    fn reading(&self, service: Service, account: &str) -> Option<&Reading> {
        self.readings
            .iter()
            .find(|reading| reading.service == service && reading.account == account)
    }
}

impl Overview {
    /// The view of `fleet` that `facts` describe; of one service's accounts
    /// with `only`.
    pub fn build(fleet: &Fleet, facts: Facts, only: Option<Service>, now: i64) -> Self {
        let now_ms = now * 1000;
        let model = facts.target_model.as_deref().map(ModelKey::new);
        let wants = |service: Service| only.is_none_or(|only| only == service);
        let mut accounts = Vec::new();
        for account in fleet.accounts.iter().filter(|_| wants(Service::Claude)) {
            let reading = facts.reading(Service::Claude, &account.name);
            let mut view = AccountView::of(Service::Claude, &account.name, reading);
            view.held = facts.default_account.as_deref() == Some(account.name.as_str());
            view.login = Some(login_view(account, now_ms));
            view.viable = reading
                .map(|reading| policy::viable(reading, facts.mode, &fleet.policy, model.as_ref()));
            view.notes.extend(
                facts
                    .observed
                    .iter()
                    .filter(|limit| limit.account == account.name)
                    .map(|limit| format!("! {}", limit.summary())),
            );
            accounts.push(view);
        }
        for account in fleet.codex.iter().filter(|_| wants(Service::Codex)) {
            let reading = facts.reading(Service::Codex, &account.name);
            let mut view = AccountView::of(Service::Codex, &account.name, reading);
            view.label = account.label.clone();
            view.parked = account.parked;
            view.viable = reading.map(Reading::accepts_work);
            accounts.push(view);
        }
        for account in fleet
            .services
            .iter()
            .filter(|account| wants(account.service))
        {
            let reading = facts.reading(account.service, &account.name);
            let mut view = AccountView::of(account.service, &account.name, reading);
            view.label = account.label.clone();
            view.held = account.service == Service::Antigravity
                && facts.agy_account.as_deref() == Some(account.name.as_str());
            accounts.push(view);
        }
        let floors = fleet.policy.floors(facts.mode);
        Self {
            at: now,
            origin: facts.origin,
            read_at: accounts
                .iter()
                .filter_map(|account| account.reading.as_ref())
                .map(|reading| reading.at)
                .max(),
            monitor: facts.monitor,
            best_weekly_remaining: accounts
                .iter()
                .filter(|account| account.service == Service::Claude)
                .filter_map(|account| account.reading.as_ref())
                .filter(|reading| reading.error.is_none() && reading.verdict() != Verdict::Rejected)
                .filter_map(Reading::weekly_remaining)
                .reduce(f64::max),
            rotation: Rotation {
                mode: facts.mode,
                probe_interval_secs: facts
                    .probe_interval_secs
                    .unwrap_or_else(|| fleet.policy.interval_secs(facts.mode)),
                min_session_remaining: floors.session,
                min_weekly_remaining: floors.weekly,
                default_is_unmanaged: facts.default_is_unmanaged,
                coverage: facts.coverage,
                sampling: facts.sampling,
                live_sessions: facts.live_sessions,
            },
            accounts,
            resets_on_offer: facts
                .offers
                .into_iter()
                .filter(|_| wants(Service::Claude))
                .collect(),
            default_account: facts.default_account,
            target_model: facts.target_model,
        }
    }

    /// The accounts of one service, in configured order.
    pub fn of(&self, service: Service) -> impl Iterator<Item = &AccountView> {
        self.accounts
            .iter()
            .filter(move |account| account.service == service)
    }

    /// The services that have an account, in the view's order.
    pub fn services(&self) -> Vec<Service> {
        let mut services = Vec::new();
        for account in &self.accounts {
            if !services.contains(&account.service) {
                services.push(account.service);
            }
        }
        services
    }

    /// The last reading of every account that has one.
    pub fn readings(&self) -> Vec<Reading> {
        self.accounts
            .iter()
            .filter_map(|account| account.reading.clone())
            .collect()
    }

    /// How many accounts' last read produced no error.
    pub fn answered(&self) -> usize {
        self.accounts
            .iter()
            .filter_map(|account| account.reading.as_ref())
            .filter(|reading| reading.error.is_none())
            .count()
    }
}

impl AccountView {
    /// An account as its last reading alone describes it.
    pub fn of(service: Service, name: &str, reading: Option<&Reading>) -> Self {
        Self {
            service,
            service_label: service.label(),
            name: name.to_owned(),
            label: None,
            held: false,
            parked: false,
            login: None,
            state: reading.and_then(|reading| {
                let (word, level) = text::badge(reading)?;
                Some(State { word, level })
            }),
            viable: None,
            error: reading.and_then(text::error_text),
            notes: reading.map(text::notes).unwrap_or_default(),
            reading: reading.cloned(),
        }
    }
}

fn login_view(account: &Account, now_ms: i64) -> LoginView {
    let login = account.login.as_ref();
    LoginView {
        credential: claude_settings::describe_credential(account, now_ms),
        text: text::describe_login(account, now_ms),
        note: text::login_note(account, now_ms),
        warning: login.is_some_and(|login| login.error.is_some() || login.is_ageing(now_ms)),
        ends_at: login
            .and_then(|login| login.refresh_expires_at)
            .map(|at| at / 1000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ServiceAccount;
    use crate::quota::{
        CodexDetail, Detail, Limit, ResetCreditCount, Scope, SessionLimit, Source, Span, Window,
    };
    use crate::testing::{codex_account, login_account, setup_account};

    const NOW: i64 = 1_794_000_000;
    const DAY_MS: i64 = 86_400_000;

    fn claude(account: &str, weekly: f64) -> Reading {
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
            services: vec![
                ServiceAccount::with_key_file(Service::Kimi, "kimi", "~/.kimikey"),
                ServiceAccount::with_key_file(Service::Antigravity, "g@x.y", "~/unused"),
            ],
            ..Fleet::default()
        }
    }

    fn build(facts: Facts) -> Overview {
        Overview::build(&fleet(), facts, None, NOW)
    }

    #[test]
    fn a_live_daemon_keeps_views_fresh_however_it_was_started() {
        assert_eq!(Monitor::of(true, false, true), Monitor::Running);
        // Under the user's own supervisor there is no launchd plist.
        assert_eq!(Monitor::of(true, false, false), Monitor::Running);
        assert_eq!(Monitor::of(true, true, true), Monitor::Paused);
        // An installed service whose daemon died reads nothing.
        assert_eq!(Monitor::of(false, false, true), Monitor::Stopped);
        assert_eq!(Monitor::of(false, false, false), Monitor::NotInstalled);
        assert!(Monitor::Running.keeps_fresh());
        assert!(!Monitor::Stopped.keeps_fresh());
        assert!(!Monitor::Paused.keeps_fresh());
    }

    #[test]
    fn every_account_of_every_service_is_one_kind_of_entry() {
        let mut absent = claude("ultra", 0.4);
        absent.claude_mut().session_limit = SessionLimit::Absent;
        let mut fable = crate::testing::model_limit("fable", "Fable", 0.5, Source::Profile);
        fable.share = Some(0.5);
        absent.limits.push(fable);
        let mut codex = Reading::empty(Service::Codex, "work", NOW - 600);
        codex.limits = vec![
            Limit::new(
                Span::Weekly,
                Scope::All,
                Window::new(0.42, NOW + 86_400),
                Source::Poll,
            ),
            Limit::new(
                Span::FiveHour,
                Scope::All,
                Window::new(0.1, NOW + 3_600),
                Source::Poll,
            ),
        ];
        codex.detail = Detail::Codex(CodexDetail {
            allowed: Some(true),
            reset_credits: Some(ResetCreditCount {
                available: 2,
                applicable: Some(0),
            }),
            ..Default::default()
        });
        let kimi = Reading {
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
        let view = build(Facts {
            readings: vec![
                // A Claude account of a Codex profile's name is another account.
                claude("work", 0.5),
                absent.clone(),
                claude("plain", 0.97),
                codex.clone(),
                // A read of another service under the same name is not this account's.
                crate::providers::failed(Service::Zai, "kimi", NOW, "wrong service"),
                kimi.clone(),
            ],
            default_account: Some("ultra".into()),
            agy_account: Some("g@x.y".into()),
            target_model: Some("claude-fable-5-1".into()),
            monitor: Monitor::Running,
            ..Facts::default()
        });
        let names: Vec<_> = view
            .accounts
            .iter()
            .map(|account| (account.service, account.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                (Service::Claude, "ultra"),
                (Service::Claude, "plain"),
                (Service::Codex, "work"),
                (Service::Kimi, "kimi"),
                (Service::Antigravity, "g@x.y"),
            ]
        );
        assert_eq!(
            view.services(),
            [
                Service::Claude,
                Service::Codex,
                Service::Kimi,
                Service::Antigravity
            ]
        );
        let ultra = &view.accounts[0];
        assert!(ultra.held);
        assert_eq!(ultra.reading.as_ref(), Some(&absent));
        assert_eq!(ultra.viable, Some(true));
        assert_eq!(ultra.state.as_ref().unwrap().word, "allowed");
        assert!(
            ultra.notes.iter().any(|note| note.contains("no 5h limit")),
            "{:?}",
            ultra.notes
        );
        // 3% of the weekly left is under the normal floor.
        assert_eq!(view.accounts[1].viable, Some(false));
        assert!(!view.accounts[1].held);
        let work = &view.accounts[2];
        assert_eq!(work.reading.as_ref(), Some(&codex));
        assert_eq!(work.viable, Some(true));
        assert_eq!(work.state.as_ref().unwrap().word, "ok");
        assert!(
            work.notes
                .iter()
                .any(|note| note.contains("2 reset credits")),
            "{:?}",
            work.notes
        );
        assert_eq!(view.accounts[3].reading.as_ref(), Some(&kimi));
        assert_eq!(view.accounts[3].service_label, "Kimi Code");
        let agy = &view.accounts[4];
        assert!(agy.held, "agy's login");
        assert_eq!((agy.reading.as_ref(), agy.state.as_ref()), (None, None));
        assert!((view.best_weekly_remaining.unwrap() - 0.6).abs() < 1e-12);
        assert_eq!(view.read_at, Some(NOW - 60));
        assert_eq!(view.answered(), 4);

        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["monitor"], "running");
        assert_eq!(json["origin"], "stored");
        assert_eq!(json["accounts"][0]["service"], "claude");
        assert_eq!(json["accounts"][0]["state"]["level"], "ok");
        assert_eq!(
            json["accounts"][0]["reading"]["limits"][0]["window"]["utilization"],
            0.4
        );
        assert_eq!(
            json["accounts"][0]["reading"]["detail"]["claude"]["session_limit"],
            "absent"
        );
        assert_eq!(
            json["accounts"][2]["reading"]["limits"][1]["span"],
            "five_hour"
        );
        assert_eq!(json["rotation"]["mode"], "normal");

        // One service's view holds that service's accounts alone.
        let only = Overview::build(
            &fleet(),
            Facts {
                readings: vec![kimi],
                ..Facts::default()
            },
            Some(Service::Kimi),
            NOW,
        );
        assert_eq!(only.accounts.len(), 1);
        assert_eq!(only.accounts[0].name, "kimi");
    }

    #[test]
    fn a_login_near_its_end_warns_and_an_unread_account_has_no_reading() {
        let view = build(Facts::default());
        let ultra = &view.accounts[0];
        let login = ultra.login.as_ref().unwrap();
        assert!(login.warning);
        assert_eq!(login.ends_at, Some(NOW + 2 * 86_400));
        assert!(login.text.contains("login ends"), "{}", login.text);
        let note = login.note.as_deref().unwrap();
        assert!(
            note.starts_with("login ends") && note.ends_with("soon"),
            "{note}"
        );
        assert_eq!(ultra.reading, None);
        assert_eq!(ultra.viable, None);
        let plain = view.accounts[1].login.as_ref().unwrap();
        assert!(!plain.warning);
        assert_eq!((plain.ends_at, plain.note.as_deref()), (None, None));
        assert_eq!(view.best_weekly_remaining, None);
        assert_eq!(view.read_at, None);
        assert!(
            view.accounts
                .iter()
                .all(|account| account.reading.is_none())
        );
        assert_eq!(view.accounts[2].login, None);
        assert_eq!(
            serde_json::to_value(&view).unwrap()["monitor"],
            "not-installed"
        );
    }

    #[test]
    fn fresh_readings_replace_the_stored_ones_and_the_rest_stay() {
        let stored = Facts {
            readings: vec![claude("ultra", 0.4), claude("plain", 0.5)],
            ..Facts::default()
        };
        let mut read_now = claude("ultra", 0.45);
        read_now.at = NOW;
        let failed = crate::providers::failed(Service::Kimi, "kimi", NOW, "HTTP 401: {\"x\":1}");
        let view = build(stored.with_fresh(vec![read_now.clone(), failed]));
        assert_eq!(view.origin, Origin::Fresh);
        assert_eq!(view.accounts[0].reading.as_ref(), Some(&read_now));
        assert_eq!(view.accounts[1].reading.as_ref().unwrap().at, NOW - 60);
        let kimi = &view.accounts[3];
        assert_eq!(kimi.state.as_ref().unwrap().word, "error");
        assert_eq!(kimi.state.as_ref().unwrap().level, Some(Level::Critical));
        assert_eq!(kimi.error.as_deref(), Some("HTTP 401: {\"x\":1}"));
        assert_eq!(view.read_at, Some(NOW));
        assert_eq!(view.answered(), 2);
    }

    #[test]
    fn reset_offers_and_rejections_on_record_ride_along() {
        let offer = ResetOffer {
            account: "ultra".into(),
            offer: "weekly reset, usable now".into(),
            read_at: NOW - 900,
        };
        let rejection = ObservedLimit {
            account: "plain".into(),
            family: crate::observe::Family::Opus,
            model: Some(ModelKey::new("opus48")),
            window: crate::observe::LimitWindow::Weekly,
            observed_at: NOW - 10,
            resets_at: NOW + 3600,
            session_id: None,
            source: crate::observe::LimitSource::Manual,
        };
        let facts = Facts {
            readings: vec![claude("plain", 0.2)],
            offers: vec![offer.clone()],
            observed: vec![rejection],
            mode: Mode::Sip,
            ..Facts::default()
        };
        let view = build(facts.clone());
        assert_eq!(view.resets_on_offer, [offer]);
        let plain = &view.accounts[1];
        assert!(
            plain
                .notes
                .iter()
                .any(|note| note.starts_with("! Opus 4.8 weekly rejected")),
            "{:?}",
            plain.notes
        );
        assert!(view.accounts[0].notes.is_empty());
        assert_eq!(view.rotation.mode, Mode::Sip);
        let floors = fleet().policy.floors(Mode::Sip);
        assert_eq!(view.rotation.min_weekly_remaining, floors.weekly);
        // Offers are Claude's: another service's view has none.
        let codex = Overview::build(&fleet(), facts, Some(Service::Codex), NOW);
        assert!(codex.resets_on_offer.is_empty());
    }
}

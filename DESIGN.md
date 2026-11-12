# tokeman design

tokeman keeps a fleet of Anthropic subscription accounts and makes sure that
Claude Code, which can hold exactly one credential at a time, always holds one
with room left. It also keeps Codex accounts alive and moves a running Codex
between them, which is a different problem with a similar shape.

This document is the shape the code is written to. The history of how each
fact was learned is in git; the code states the fact and nothing more.

## The problem, as it actually is

- **One slot.** Claude Code reads `CLAUDE_CODE_OAUTH_TOKEN` from
  `~/.claude/settings.json` for every API client it builds, so every live
  session spends whatever that file offers *now*. There is no per-session
  binding to manage; there is one default, and changing it moves everyone.
- **A switch is expensive.** Every switch cold-starts the prompt cache of
  every live session (tens of millions of cache-creation tokens for a swarm).
  The policy therefore stays on the default until the default itself is at
  its floor, and lands only on an account with real room.
- **Readings are partial.** The response headers of a Haiku request describe
  the 7d window, the 5h window when the account has one and has used it, and
  that model's own weekly window (`7d_oi`, Fable only); the overall status
  describes only the requested model. The profile endpoint's `limits[]` is the
  full list of meters (session, weekly_all, weekly_scoped per model) and says
  whether a 5h limit exists at all, but needs a full-scope login and
  rate-limits per address; rejections seen in a transcript are the only
  signal for some limits. A reading is a merge of these sources, each tagged.
- **Credentials have two kinds.** A setup token (`claude setup-token`) is
  long-lived and inference-only. A browser login is full-scope and refreshable,
  and its refresh token is single-use: a save from a stale copy, or a
  refresh that redeems a token twice, kills the grant. Every write to the
  account file is a locked read-modify-write; the old access token is revoked
  the moment a refresh succeeds.
- **The box is hostile when it matters.** Rotation matters exactly when the
  machine is pegged by the swarms spending the quota. The daemon runs at
  normal priority, every child process wait is bounded, and every lock wait
  is announced.

## Shape

One package: a library (`src/lib.rs`) that has no UI dependency, and a thin
binary. Modules are by domain, not by surface.

```
src/
  lib.rs
  fs.rs          owner-only files: atomic replace (0600), dirs (0700), FileLock, try_lock
  paths.rs       every on-disk location, in one place
  account.rs     Account, Credential, LoginGrant, Fleet: tokens.toml read/write (locked, comment-preserving)
  settings.rs    LaunchSettings, PolicySettings: the [settings]/[rotation] tables and `settings set`
  quota.rs       Service, Span, Scope, Limit, Reading, ReadError, Detail, Window, ModelKey, Source, Level: what is
                 known about an account's room, for every service. The one model of a limit and of a read.
  policy.rs      PURE. Mode, Floors, viability, landing choice, Decision. No IO, no clock, no strings.
  history.rs     credential history: Event, fingerprint, account_at (JSONL, same format as today)
  store.rs       sqlite read history: every read of every service is one JSON Reading in `readings`; latest/recent/since
                 over it and the old tables; Series, Stats; a read-only open that creates no file, table or row
  store/legacy.rs  the tables earlier builds wrote (snapshots, codex_readings, service_readings), converted to Readings
                 as they are read; never altered, migrated or written
  observe.rs     ObservedLimit, SessionBinding, the SessionStart hook input, transcript scan
  anthropic/
    mod.rs       the HTTP client that identifies as Claude Code; error classification
    probe.rs     the Haiku probe and the optional target-model sample -> Reading (the header shape is private to it)
    profile.rs   /api/oauth/usage: limits[] parser -> model Limits, on-disk cache, fleet-wide 429 backoff
    oauth.rs     PKCE login, code exchange with retry, refresh (incl. refresh-token expiry)
    resets.rs    cedar_ember / juniper_tide offers and claims; pending-claim journal
  claude/
    settings.rs  ~/.claude/settings.json env block: install, clear, inspect (the only writer, 0600)
    keychain.rs  macOS: capture, suppress and restore the /login Keychain record
    sessions.rs  live Claude processes (bounded census) and which account each spends
  codex/
    mod.rs       client (HTTP/1.1 + cookie jar), edge-challenge detection
    auth.rs      auth.json read/write, seating
    appserver.rs JSON-RPC over the app-server Unix socket
    probe.rs     usage probe -> Reading: account-wide windows named by length, additional limits scoped to their
                 model or feature, the rest in CodexDetail; State, is_viable
    refresh.rs   token refresh
    resets.rs    reset credits: list, claim, and the pending-claim journal (its only writer)
    commands.rs  add/discover/import/list/refresh/best/seat/reseat/watch (best ranks through policy::choose_above)
  providers/
    mod.rs       ServiceAccount: read-only quota of services tokeman does not rotate; read (dispatch by service) ->
                 Reading, read_all (concurrent, each read bounded), ranked (policy::rank_above), the shared client
    zai.rs       GLM Coding Plan: /api/monitor/usage/quota/limit, raw key; errors arrive as HTTP 200 with the code in the body
    kimi.rs      Kimi Code: /coding/v1/usages (Bearer) -> Limits + booster Balance; plan name from /coding/v1/me
    antigravity.rs  Google OAuth refresh (own access-token cache), retrieveUserQuotaSummary -> Limits scoped to their group,
                 discover() from agy's Keychain item / token file and the app's state.vscdb (read-only)
    agy.rs       the agy CLI's login slot (Keychain item via /usr/bin/security, bounded): which account it holds,
                 seat an Antigravity account in it (locked, previous value backed up 0600, read back), put the previous back.
                 Manual only (`services use`); the daemon never seats. Running agy processes keep their account.
  daemon/
    mod.rs       the loop
    cycle.rs     one cycle: refresh due grants, probe, observe, decide, install, record
    cadence.rs   typed on-disk cycle state: last probe, mode, interval, backoff (keyed to the refused credential), samples
    lock.rs      the rotation lock
    services.rs  the service-quota poll: its own task every services_interval_secs, recorded; rotation never waits on it
    service.rs   launchd plist, install/uninstall (macOS)
  ui/
    text.rs      gauges, tables, status (every println in one module)
    chart.rs     PNG + iTerm2 inline, shared segment logic
    tui.rs       --watch (cards, chart, a services strip)
    services_cli.rs  `tokeman services`: list, add, remove, import antigravity
    overview.rs  the stored view: everything last read about every account (Claude, reset offers, Codex, services),
                 built without a request or a write; `tokeman list --json` prints it and both trays draw it
    tray.rs      egui tray (feature "tray"): Claude cards from its own probe, the rest from the stored view
  cli.rs         clap definitions and dispatch; no logic
  launch.rs      the claude launch command and terminal handoff
macos/tray/      Swift menu-bar app; a viewer of `tokeman list --json`, `history --json` and `settings show` that never
                 opens tokens.toml; probes only when no service is installed; writes only through
                 `tokeman settings set` and `tokeman rotate use`
```

Dropped from the tree (history keeps them): the web dashboard and its D3
page, the transcript session browser, `per_session_reserve` (no decision it
can move once sessions follow the default), the Opus/Sonnet-specific
columns and profile fields of 0.1/0.2, the events reconstructed from the
rotation log, and the `usage_key` spelling in tokens.toml.

## Core types

```rust
// quota.rs: one model for every service. A read is a Reading; what it
// reported is a list of Limits, each a window named by its length (Span)
// over part of the account (Scope).
pub enum Service { Claude, Codex, Antigravity, Zai, Kimi }      // serde: lowercase
pub struct Window { pub used: f64 /* fraction, may exceed 1.0 */, pub resets_at: i64 /* epoch s, 0 = none pending */ }
pub enum Span { FiveHour, Weekly, Monthly /* 28-31 d */, Other(i64 /* s */), Unknown }   // from_seconds, label
pub struct ModelKey(String);                 // "opus48", "fable51", "fable" (family-wide)
pub enum Scope {
    All,                 // the whole account
    Model(ModelKey),     // Claude's per-model weekly limits; a Codex limit that stands in for a model's
    Group(String),       // Antigravity's model groups, a Codex metered feature, z.ai's MCP tools
}
pub enum Source { Headers, Profile, Observed, Carried, Poll /* a usage endpoint asked directly */ }
pub struct Limit {
    pub span: Span, pub scope: Scope,
    pub label: String,                 // the server's name, when it has one
    pub window: Window, pub source: Source,
    pub share: Option<f64>,            // size as a fraction of the weekly: Fable 0.5
    pub status: Option<WindowStatus { verdict, surpassed_threshold }>,   // the window's own status
    pub amount: Option<Amount { used, limit, unit }>,
}
impl Limit { fn room(&self, weekly: Option<&Window>) -> f64 }   // min(own room x share, weekly room)
pub enum ReadError { NoCredential, Credential{message}, Network{message}, Edge{status, attempts},
                     Http{status, body}, Decode{message}, Other{message} }   // serde: tag "kind"
pub struct Reading {
    pub service: Service, pub account: String, pub at: i64,
    pub plan: Option<String>,
    pub limits: Vec<Limit>,                 // an omitted window limits nothing
    pub balances: Vec<Balance>,
    pub error: Option<ReadError>,
    pub detail: Detail,
}
pub enum Detail { None, Claude(ClaudeDetail), Codex(CodexDetail) }
pub struct ClaudeDetail { verdict /* requested model only */, claim, session_limit, signals, overage, profile_error }
pub struct CodexDetail { email, allowed: Option<bool>, limit_reason, credits, models, spend_control, reset_credits }
pub enum Verdict { Allowed, AllowedWarning, Rejected, Unknown }
pub enum SessionLimit { Unknown, Present, Absent }   // Absent: limits[] has no session row (verified Ultra)
pub struct Signals {                       // Claude headers beyond the windows; display and history only
    claims: Vec<String>, fallback_available: bool, fallback_percentage: Option<f64>, upgrade_paths: Vec<String>,
    slow: Option<SlowLane { offer: SlowOffer, status: SlowStatus, retry_after_secs, max_wait_secs, budget: Window }>,
    grace: Option<Grace { status, session_used, weekly_used }>,
}
impl Reading {
    fn limit(&self, span, &Scope) -> Option<&Limit>; fn window(&self, span, &Scope) -> Option<Window>;
    fn session(&self) / weekly(&self) -> Option<Window>;      // the account-wide 5h / weekly window
    fn shared(&self) -> impl Iterator<Item = &Limit>;         // Scope::All
    fn scoped(&self) -> Vec<(&Scope, Vec<&Limit>)>;           // the rest, gathered by scope
    fn limits_for(&self, model) -> impl Iterator<Item = &Limit>;   // model limits that limit `model`
    fn model_room(&self, model) -> Option<f64>;
    fn has_quota(&self) -> bool;     // no error and at least one account-wide window
    fn is_refused(&self) -> bool;    // will not change until the credential does
    fn allows(&self) -> bool;        // Claude's verdict, Codex's allowed; nothing else refuses
    fn above(&self, session_floor, weekly_floor, model) -> bool;
        // has_quota, allows, no account-wide window rejected by its own status, every account-wide window above
        // its floor (the session floor for 5h, the weekly floor for any other span), every limit on `model`
        // above the weekly floor
    fn tightest(&self) -> Option<&Limit>;
    fn scoped_to(&self, needle: Option<&str>) -> Option<Reading>;
        // the reading as work under its scoped limits sees it, for ranking on a group or a per-model limit:
        // the matching scoped limits become the account-wide ones (every group's when `None`)
}
// A scoped limit never gates an account by itself: Codex's spent Spark limit and one Antigravity group at
// its wall leave the account viable. `best --limit/--group` rank on `scoped_to`.

// account.rs
pub enum Credential<'a> { Setup(&'a str), Login { token: &'a str, expires_at: i64, scopes: &'a [String] } }
pub struct Login { access_token, refresh_token, expires_at, obtained_at, logged_in_at,
                   refresh_expires_at /* reported; min kept across refreshes */, scopes, error }
pub struct Account { name, setup_key: Option<String>, login: Option<Login> }
impl Account { fn credential(&self, now: i64) -> Option<Credential> }   // login if live, else setup
pub struct Fleet { pub accounts: Vec<Account>, pub codex: Vec<CodexAccount>, pub services: Vec<ServiceAccount>, pub launch: LaunchSettings, pub policy: PolicySettings }

// providers/mod.rs  (each service module: `pub async fn read(&reqwest::Client, &ServiceAccount, now) -> Reading`, never Err)
pub struct ServiceAccount { name, service, key, key_file, refresh_token, access_token, expires_at, label }
pub fn ranked(&[Reading], Service, group) -> Vec<(&Reading, f64)>   // policy::rank_above on scoped_to(group); failed reads left out
// providers/agy.rs
pub struct Slot { email, refresh_token }  impl Slot { fn holds(&ServiceAccount) -> bool }   // same grant or same email
pub struct Seated { email, previous: Option<String>, changed: bool }
pub fn slot() -> Result<Option<Slot>> ; pub async fn seat(&Client, &ServiceAccount) -> Result<Seated> ; pub async fn seat_previous() -> Result<Seated>
impl Fleet { fn load() ; fn update(|&mut Fleet| -> T) -> T  /* locked RMW, comments kept */ }

// policy.rs  (pure; the tests are the spec). Landing spends expiring room
// first: a weekly reset within 24 h of the reading, soonest first; otherwise
// the longest dwell.
pub enum Mode { Normal, Sip }
pub struct Floors { pub session: f64, pub weekly: f64 }
pub struct Policy { normal: Floors, sip: Floors, landing: Floors, intervals: Intervals, sample_secs: u64 }
pub enum Decision<'a> { Stay, Switch { to: &'a Reading, why: Reason }, Drain, NoneViable }
pub fn choose_above(readings: &[Reading], floors: Floors, model: Option<&ModelKey>) -> Option<&Reading>
pub fn rank_above(readings: &[Reading], floors: Floors, model: Option<&ModelKey>) -> Vec<&Reading>
    // the one ranking: Claude's landing, `codex best`, `services best`. Never matches on Service.
pub fn decide(fleet: &[Reading], current: Option<&str>, mode: Mode, target: Option<&ModelKey>, policy: &Policy) -> Decision
pub fn mode_for(fleet: &[Reading], expected: usize, previous: Mode, policy: &Policy, target: Option<&ModelKey>) -> Mode

// history.rs
pub enum EventKind { Switch, Install, Activate, Refresh, Upgrade, Fallback, Repair, Pause, Revoke }   // serde: same strings as today
pub struct Event { at: i64, account: Option<String>, fingerprint: Option<Fingerprint>, expires_at: Option<i64>, kind: EventKind }
pub fn account_at(events: &[Event], at: i64) -> Option<Option<String>>

// observe.rs
pub enum Family { Opus, Sonnet, Fable, Mythos, General }
pub enum LimitWindow { Session, Weekly }
pub struct ObservedLimit { account, family, model: Option<ModelKey>, window, observed_at, resets_at, session: Option<String>, source: LimitSource }
```

Rules the types enforce:

- Nothing stringly typed for state: event kinds, backup states, verdicts,
  modes, sources are enums with serde names fixed to today's on-disk strings.
- `policy.rs` imports nothing from the rest of the crate but `quota.rs` (and
  the settings struct that carries the floors). It never reads the clock or
  a file; callers pass `now`.
- One type describes a limit (`quota::Limit`) and one a read
  (`quota::Reading`). A server's own shape is a private struct in the module
  that parses it; a shape an earlier build stored is a private struct in
  `store/legacy.rs`.
- Exactly one writer per file: `claude/settings.rs` for settings.json,
  `account.rs` for tokens.toml, `daemon/cadence.rs` for the cycle state,
  `observe.rs` for the admission state, `history.rs` for the JSONL,
  `codex/resets.rs` for the Codex pending-claim journal.
- Every subprocess wait has a timeout. Every lock acquire tries first and
  says when it waits.
- Dry run means no write anywhere, and the cycle says what it would have done.

## Compatibility kept

- `tokens.toml`: same tables and keys (`[[tokens]]` with `name`, `key`,
  `access_token`, `refresh_token`, `expires_at`, `obtained_at`,
  `logged_in_at`, `refresh_expires_at`, `scopes`, `login_error`;
  `[settings]`; `[rotation]`; `[[codex_accounts]]`), plus `[[services]]`
  (`name`, `service`, `key` or `key_file`, Antigravity's grant, `label`) and
  `services_interval_secs` in `[settings]`. Comments survive every write; keys
  added later are optional.
- `~/.claude/settings.json` env keys: `CLAUDE_CODE_OAUTH_TOKEN`,
  `CLAUDE_CODE_OAUTH_SCOPES`, `CLAUDE_CODE_OAUTH_401_WAIT_MS`, `TOKEMAN_ACCOUNT`;
  the SessionStart hook command `tokeman rotate observe-session --pid "$PPID"`.
- `claude-credential-history.jsonl` events and `claude-admission-state.json`:
  read as they are.
- `snapshots.db`: new reads are JSON rows in `readings(service, account, at,
  reading)`. The `snapshots`, `codex_readings` and `service_readings` tables
  of earlier builds are read where they are, by column name, and never
  altered, migrated or written; a row that no longer reads is left out.
- CLI commands and flags as documented in the README. `browse` is removed.
- Cadence state, the admission state and the profile-read cache load entry
  by entry: one in a shape this build does not read is dropped alone.
- `reset-claims/<account>.pending`: a grant claim sent without a known
  outcome. It is never deleted except by a read answer or `tokeman resets clear`.

## Facts the design rests on

Each was measured; the commit that measured it is findable by its date.

- Claude Code 2.1.282 to 2.1.287 re-applies the settings env block on every
  change and rebuilds its API client credentials from it, so a live session
  spends the current default from its next request (2026-10-01).
- A refresh revokes the access token it replaces within seconds (2026-09-25).
- The token endpoint answers tokeman's old headers with 429 and Claude Code's
  axios defaults with the normal 400 on a bogus exchange (2026-09-25).
- Premium models refuse subscription tokens with a bare 429 and no quota
  headers unless the request carries Claude Code's identity (2026-09-25).
- `/api/oauth/usage` answers 429 under a 20-second probe cadence; Claude Code
  caches it for 60 seconds (2026-09-25). A 429 holds back the account that
  drew it, and every account once two have: the budget is shared per address,
  yet a paused organization drew 429s while another account read fine from
  the same address (2026-11-11).
- Verified Ultra accounts (Max 20x until 2026-10-30) have no 5h limit; their
  `limits[]` has no session row. No account here sent a `5h-*` header on
  2026-11-11. Fable's weekly limit is a 50% share of the weekly
  (`weekly_scoped_shares`) and its `7d_oi` can read over 1.0.
- The token endpoint reports `refresh_token_expires_in`; refreshes do not
  extend it (Claude Code 2.1.322 keeps the earlier date). Logins have lasted
  28 days (2026-08) and 30 (2026-11).
- `cedar_ember` is read with `?cedar_ember=1&skip_spend=1`, `juniper_tide`
  with `?at_wall=1&skip_spend=1` (Claude Code 2.1.322). A Fable grant
  (`gives_back_weekly`) gives back only Fable's use of the weekly.
- Reset programs (`cedar_ember`, `juniper_tide`) are eligible only for the
  Claude Code user agent (2026-09-25).
- The ChatGPT edge challenges h2 requests with Codex's headers and replays
  `__cf_bm` only across a shared cookie jar (2026-08).
- Codex's 401 recovery refuses an `auth.json` whose account id changed; the
  app-server `reload` path does not (2026-08).
- The Codex usage body puts whichever window exists first in
  `primary_window`: a weekly-only account reports its 604800 s window there
  and `secondary_window: null` (2026-11-11).
- A Background-class LaunchAgent stretched a 20 s cycle to 10 to 24 minutes
  under a load average of 200 (2026-10-14).
- A page in the user's browser can read a loopback HTTP server through DNS
  rebinding unless the server checks `Host` (2026-10-14).

## How the rewrite is done

1. Foundation first, by one author: `fs`, `paths`, `quota`, `account`,
   `settings`, `policy`, `history`, `store`, with their tests ported from the
   old modules. These are the signatures every other module is written against.
2. Then four lanes in parallel, each given the real foundation signatures and
   the old module as a behavioral oracle: `anthropic/`, `codex/`,
   `claude/` + `observe`, `daemon/`.
3. Then `ui/`, `cli`, `launch`, the Swift tray's tiny interface; the old tree
   is deleted; every gate runs; the daemon is dogfooded for a day before
   0.3.0.

Done means: the unit tests of the old tree all have a counterpart (or a
recorded reason not to), `cargo clippy -D warnings` on both features and
both operating systems, and the installed daemon has run a day of real
rotation on this machine.

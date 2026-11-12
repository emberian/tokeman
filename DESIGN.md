# tokeman design

tokeman keeps Claude Code, Codex and agy on a subscription account with room
left, and shows the quota of every account it knows. This document is the
shape the code is written to: the problem, the modules, the types, the rules
the types enforce, and the files on disk. The code states each fact once;
this says where.

## The problem

- **Five services, one thing reported.** Claude, Codex, Antigravity, GLM and
  Kimi each report a list of limits: a window of some length over the whole
  account, one model, or a named group of models. tokeman has one model of a
  limit and one of a read, and every reader produces it.
- **Claude Code has one slot.** It reads `CLAUDE_CODE_OAUTH_TOKEN` from
  `~/.claude/settings.json` for every API client it builds, so every live
  session spends whatever that file offers now. There is one default, and
  changing it moves everyone.
- **A switch is expensive.** It cold-starts the prompt cache of every live
  session. The policy stays on the default until the default itself is at
  its floor, and lands only on an account with real room.
- **Claude's readings are partial.** The response headers of a Haiku request
  describe the weekly window, the 5-hour window when the account has one and
  has used it, and the requested model's own weekly window; the overall
  status describes only the requested model. The profile endpoint lists
  every limit and says whether a 5-hour limit exists at all, but needs a
  full-scope login and rate-limits per address. Rejections seen in a
  transcript are the only signal for some limits. A reading is a merge of
  these sources, each limit tagged with where it came from.
- **Credentials have two kinds.** A setup token is long-lived and
  inference-only. A browser login is full-scope and refreshable, and its
  refresh token is single-use: a save from a stale copy, or a refresh that
  redeems a token twice, kills the grant. Every write to the account file is
  a locked read-modify-write.
- **Codex and agy differ from Claude Code.** Codex resolves credentials per
  request from `$CODEX_HOME`, so a running Codex can be moved by rewriting
  `auth.json` and telling its app-server. agy, the Antigravity app and an
  editor's `agy acp` share one Keychain login, and a running client writes
  its own account back at its next token refresh, so that slot is changed
  only on request and only after looking at what runs.
- **One reader.** Usage endpoints rate-limit. While the background service
  runs it is the only process that reads accounts on a timer; every other
  surface shows what it stored.
- **The box is busy when it matters.** Rotation matters exactly when the
  machine is pegged by the sessions spending the quota. The service runs at
  normal priority, every child-process wait is bounded, and every lock wait
  is announced.

## Shape

One package: a library (`src/lib.rs`) with no UI dependency in its domain
modules, and a thin binary. Modules are by domain, not by surface.

```
src/
  lib.rs
  fs.rs          owner-only files: atomic replace (0600), dirs (0700), FileLock, try_lock
  paths.rs       every on-disk location, in one place
  account.rs     Account, Login, Credential, CodexAccount, Fleet: tokens.toml read/write (locked, comment-preserving)
  settings.rs    LaunchSettings, PolicySettings: the [settings]/[rotation] tables and `settings set`
  quota.rs       Service, Span, Scope, Limit, Reading, ReadError, Detail, Window, ModelKey, Source, Level: the one
                 model of a limit and of a read, for every service
  policy.rs      PURE. Mode, Floors, viability, the one ranking, Decision. No IO, no clock, no strings
  history.rs     credential history: Event, Fingerprint, account_at (JSONL)
  claims.rs      the claim journal: reset claims (Claude grants, Codex credits) sent without a definite answer, keyed
                 by (Service, account); one request id per claim, resent verbatim; the only writer of claims.json
  attention.rs   PURE. Attention: what needs the user, found in the fleet view at a given time, each with a stable key
  forecast.rs    PURE. Forecast, Outlook: when a service's accounts run dry at the rate of the last BURN_HOURS
  drift.rs       what a read carried that no parser knows: the collector every parser fills from the known sets it
                 keeps beside itself, and the ledger of items the daemon has logged; the only writer of drift.json
  notice.rs      progress lines (a lock being waited on, a retry sleeping, dry-run narration) go to a sink the binary
                 installs; the library never writes to the terminal
  util.rs        pure helpers shared across domains, once each: one_line, shell_quote, parse_reset, backoff_secs
  store.rs       sqlite read history: Store (insert, latest, recent, since), Series, Stats; a read-only open that
                 creates no file, table or row. It reads `readings` and nothing else
  store/compact.rs  `history compact`: the one-time rewrite of the tables builds before 0.4 wrote into `readings`
                 (database held exclusively, original kept, batched and resumable, compared before anything is dropped)
  store/legacy.rs  the shapes those builds stored, decoded to Readings; read only by compact
  observe.rs     ObservedLimit, session bindings, the SessionStart hook input, the transcript scan
  anthropic/
    mod.rs       the HTTP client that identifies as Claude Code; error classification
    probe.rs     the Haiku probe and the optional target-model sample -> Reading (the header shape is private to it)
    profile.rs   /api/oauth/usage: limits[] -> Limits, the on-disk cache, the 429 backoff
    oauth.rs     PKCE login, code exchange with retry, refresh (incl. refresh-token expiry)
    resets.rs    reset offers and claims (through the claim journal)
  claude/
    settings.rs  ~/.claude/settings.json env block: install, clear, inspect (the only writer, 0600)
    keychain.rs  macOS: capture, suppress and restore the /login Keychain record
    sessions.rs  live Claude processes (bounded census) and which account each spends
  codex/
    mod.rs       client (HTTP/1.1 + cookie jar), edge-challenge detection
    auth.rs      auth.json read/write, seating
    appserver.rs JSON-RPC over the app-server Unix socket
    probe.rs     the usage probe -> Reading; State (ok / limited / error, as words)
    refresh.rs   token refresh
    resets.rs    reset credits: list, plan, claim (through the claim journal)
    maintenance.rs  the service's half-hour sweep: probe, record, refresh what is due or rejected
    mover.rs     moving a running Codex: Seat (what its app-server reports), consider (leave below the Codex
                 floors for the best profile above the landing floors, via policy::relocate), announce, Look;
                 the only writer of codex-moves.jsonl. Shared by the daemon and `codex watch`
    seats.rs     Pushed: the seats tokeman pushed (home, account, token fingerprint, expiry, last Trouble); the
                 only writer of codex-seats.json, locked
    keep.rs      keeping a pushed seat alive: tend (renew through refresh.rs a day before expiry or when the token
                 is dead, push again; forget a seat whose Codex is gone), freshen, RENEW_WITHIN_SECS
  providers/
    mod.rs       ServiceAccount; read (dispatch by service) -> Reading, read_all (concurrent, each read bounded)
    zai.rs       GLM Coding Plan
    kimi.rs      Kimi Code: Limits and the booster Balance
    antigravity/
      mod.rs     the quota read -> Limits scoped to their model group; its own access-token cache
      grant.rs   the Google OAuth refresh exchange; discover() from agy's Keychain item or token file and the app's
                 state.vscdb (read-only protobuf decode)
      seat.rs    agy's login slot through `/usr/bin/security -i` (locked, previous value backed up 0600, read back,
                 written once more if it reads empty); use_account -> UseReport. Manual only (`tokeman agy use`); the
                 daemon never seats
      clients.rs the running clients of that slot: the hub app (its daemon file), `agy acp`, interactive agy, print-mode
                 agy. Bounded read-only ps/lsof census; GetUserStatus and GetAuthStatus on loopback; the resume command
                 from agy's history. Never a signal, never a logout. An agy whose HOME (ps -E) is a per-account home
                 is a Session, not a client
      run.rs     `tokeman agy run`: a per-account home under paths::agy_homes (token file from tokeman's grant, links
                 to the real home but .gemini and Library), HOME + SSH_TTY so agy keeps its login in that file; exec
  daemon/
    mod.rs       the loop; install, uninstall, pause, resume, activate
    cycle.rs     one rotation cycle: refresh due grants, probe, observe, decide, install, record
    cadence.rs   typed on-disk cycle state: last probe, mode, interval, backoff, carried samples
    lock.rs      the rotation lock, the pause marker, and the daemon's lifetime lock (held = service alive)
    log.rs       the rotation log
    refresh.rs   the login refresh sweep
    services.rs  the service-quota poll: its own task every services_interval_secs; rotation never waits on it
    notify.rs    the notification round: its own task every two minutes; the ledger of what was told (the only writer
                 of notified.json); the bounded osascript / notify-send delivery
    codex.rs     the Codex mover: codex::keep's tending of every pushed seat, then codex::mover's look at every
                 control socket, in its own bounded task every five minutes; Standing (on / off / paused / fewer
                 than two profiles) gates the moving, only a pause the tending
    service.rs   the launchd plist, install/uninstall (macOS)
  commands/      what each command does, returning data; nothing here prints or reads the terminal
    claude.rs    add, remove, the browser login in two halves, refresh, use, reset offers / plan / claim / clear
    codex.rs     add, remove, discover, import, list, refresh, use, watch, reset plan / claim / clear
    doctor.rs    one fresh read of every account, the tools, the hook, the service, file modes, agy's login and
                 pending claims -> Findings; Report::assemble (pure) decides what is a problem and names its fix
    launch.rs    `<service> launch`: codex on the best profile's CODEX_HOME, agy run on the best account, claude on
                 a GLM or Kimi plan (Endpoint: base URL, key variable, models)
    services.rs  agy / glm / kimi: add, remove, import, the account agy holds, agy use, agy run
    rotate.rs    run, status (the fleet view of Claude read as a cycle reads it), pause, resume, install, uninstall,
                 quarantine, clear-quarantine, the hook's record
    view.rs      probe (read now, record), stored / fresh / overview (the fleet view), ranked and best, history,
                 history compact, stats, settings
  ui/
    overview.rs  Overview, AccountView, Facts: the one fleet view, and what it is built from
    text.rs      what things are called and said (limit_name, limit_note, badge, summary, notes, view_caption), the
                 text card, and every println and eprintln in the crate
    prompt.rs    the only reader of the terminal: confirmations, the hook's stdin, the browser-login conversation
    chart.rs     PNG charts of any limit, the --metric resolver, the segment logic the TUI chart shares
    tui.rs       --watch: a card per account, a chart of any limit
  cli.rs         clap definitions, and REMOVED: the 0.3 spellings and what replaced each
  main.rs        dispatch: parse, call commands/, hand the result to ui/text
  launch.rs      `claude launch` (one cycle, then claude); Launch: a program, its arguments and the
                 environment that seats an account, exec'd by main
macos/tray/      the Swift menu-bar app: a viewer of `tokeman list --json`
```

## Core types

```rust
// quota.rs: one model for every service.
pub enum Service { Claude, Codex, Antigravity, Zai, Kimi }      // serde: lowercase
pub struct Window { pub used: f64 /* fraction, may exceed 1.0 */, pub resets_at: i64 /* epoch s, 0 = none pending */ }
pub enum Span { FiveHour, Weekly, Monthly /* 28-31 d */, Other(i64 /* s */), Unknown }   // from_seconds, label
pub struct ModelKey(String);                 // "opus48", "fable51", "fable" (family-wide)
pub enum Scope {
    All,                 // the whole account
    Model(ModelKey),     // Claude's per-model weekly limits; a Codex limit that stands in for a model's
    Group(String),       // Antigravity's model groups, a Codex metered feature, z.ai's MCP tools, Claude's extra usage
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
    pub unrecognized: Vec<String>,          // header names, body keys and enum words no parser knows (drift.rs)
}
pub enum Detail { None, Claude(ClaudeDetail), Codex(CodexDetail) }
pub struct ClaudeDetail { verdict /* requested model only */, claim, session_limit, signals, overage, profile_error }
pub struct CodexDetail { email, allowed: Option<bool>, limit_reason, credits, models, spend_control, reset_credits }
pub enum SessionLimit { Unknown, Present, Absent }   // Absent: limits[] has no session row
impl Reading {
    fn limit(&self, span, &Scope) -> Option<&Limit>; fn window(&self, span, &Scope) -> Option<Window>;
    fn session(&self) / weekly(&self) -> Option<Window>;      // the account-wide 5h / weekly window
    fn shared(&self) -> impl Iterator<Item = &Limit>;         // Scope::All
    fn scoped(&self) -> Vec<(&Scope, Vec<&Limit>)>;           // the rest, gathered by scope
    fn limits_for(&self, model) -> impl Iterator<Item = &Limit>;   // model limits that limit `model`
    fn has_quota(&self) -> bool;     // no error and at least one account-wide window
    fn is_refused(&self) -> bool;    // will not change until the credential does
    fn allows(&self) -> bool;        // Claude's verdict, Codex's allowed; nothing else refuses
    fn above(&self, session_floor, weekly_floor, model) -> bool;
        // has_quota, allows, no account-wide window rejected by its own status, every account-wide window above
        // its floor (the session floor for 5h, the weekly floor for any other span), every limit on `model`
        // above the weekly floor
    fn tightest(&self) -> Option<&Limit>;
    fn scoped_to(&self, needle: Option<&str>) -> Option<Reading>;
        // the reading as work under its scoped limits sees it: the matching scoped limits become the
        // account-wide ones (every group's when `None`)
}
// A scoped limit never gates an account by itself: Codex's spent Spark limit and one Antigravity group at
// its wall leave the account viable. `best --limit/--group` rank on `scoped_to`.

// account.rs
pub enum Credential<'a> { Setup(&'a str), Login { token: &'a str, expires_at: i64, scopes: &'a [String] } }
pub struct Login { access_token, refresh_token, expires_at, obtained_at, logged_in_at,
                   refresh_expires_at /* reported; the earlier date is kept across refreshes */, scopes, error }
pub struct Account { name, setup_key: Option<String>, login: Option<Login> }
impl Account { fn credential(&self, now: i64) -> Option<Credential> }   // login if live, else setup
pub struct Fleet { pub accounts: Vec<Account>, pub codex: Vec<CodexAccount>, pub services: Vec<ServiceAccount>,
                   pub launch: LaunchSettings, pub policy: PolicySettings }
impl Fleet { fn load() ; fn update(|&mut Fleet| -> T) -> T  /* locked RMW, comments kept */ ;
             fn account_keys(&self) -> Vec<(Service, &str)> }

// policy.rs  (pure; the tests are the spec). Landing spends expiring room first: a weekly reset within 24 h of
// the reading, soonest first; otherwise the longest dwell.
pub enum Mode { Normal, Sip }
pub struct Floors { pub session: f64, pub weekly: f64 }
pub struct Situation<'a> { readings, expected, current, model, force }
pub enum Decision<'a> { Hold(Hold), Stay { healthy }, Switch { to: &'a Reading, trigger: Trigger }, NoneViable }
pub fn decide<'a>(&Situation<'a>, &PolicySettings) -> Decision<'a>
pub fn rank_above(readings: &[Reading], floors: Floors, model: Option<&ModelKey>) -> Vec<&Reading>
    // the one ranking: Claude's landing and every `tokeman <service> best`. Never matches on Service.
pub enum Relocation<'a> { Stay, Drain, Move(&'a Reading) }
pub fn relocate<'a>(seat: &Reading, others: &'a [Reading], leave: Floors, land: Floors) -> Relocation<'a>
    // a tool that can be moved between turns: leaves at `leave` for the best of `others` above `land`; a seat
    // that takes no work leaves for any room. The Codex mover's decision.

// codex/mover.rs
pub struct Seat { reading /* the account-wide windows the running Codex reports */, info, account_id }
pub enum Outcome { NoSocket, Unreachable, NoWindows, Healthy, Draining, WouldMove { to }, Moved { to, announced },
                   Failed { to, message } }
pub struct Look { home, seated, tightest, probed /* the caller keeps these */, outcome }
pub struct Move { at, home, from, to, remaining, span, by: By /* daemon | watch | use */ }   // codex-moves.jsonl
pub struct Terms { floors: MoveFloors, seats: Seats, by: By }
pub async fn consider(&Client, &Fleet, &CodexHome, &Seat, &Terms, dry_run) -> Look   // probes only a low seat
pub async fn announce(&Client, &CodexAccount, &CodexHome, &Seats, By) -> Result<Announced>   // push, then Seats::put

// codex/seats.rs, codex/keep.rs
pub struct Pushed { home, account, account_id, email, token: Fingerprint, expires_at, pushed_at, by, trouble }
pub enum Cause { Refused, RefreshFailed, Unanswered, PushFailed, ProfileGone, Unknown }
pub enum Tended { Fine, Renewed { expires_at }, Ended, Troubled(Trouble) }
pub async fn tend(&Client, &Fleet, &Seats, &Pushed, now) -> Tend

// launch.rs
pub struct Launch { service, account, room, program, args, env: Vec<EnvVar { name, value, secret }>, unset }
impl Launch { fn exec(self) -> anyhow::Error }   // replaces the process; a secret is named, never printed

// store.rs
pub struct Series { pub span: Span, pub scope: Scope }     // one limit followed through the history
impl Store {
    fn insert(&self, &Reading)                             // the growth rule below
    fn latest(&self, accounts: &[(Service, &str)]) -> Vec<Reading>
    fn recent(&self, Service, account: Option<&str>, limit) / since(&self, Service, account, since) -> Vec<Reading>
    fn has_old_tables(&self) -> bool                       // reads from before 0.4 wait for `history compact`
}
// store/compact.rs
pub fn run(dry_run: bool) -> Result<Report { outcome, path, tables: Vec<Table { table, rows, unreadable, kept,
                                             accounts }>, original, bytes_before, bytes_after }>
pub struct Stats { account, readings, limits: Vec<Burn { series, label, latest, mean, stddev, peak, hours_left }> }

// claims.rs
pub struct Pending { service, account, item: Option<String> /* grant or credit */, request_id, sent_at }
impl Journal { fn pending(service, account) ; fn begin(service, account, item, now) -> (Pending, new) ;
               fn finish(..) ; fn clear(..) }

// ui/overview.rs: the one fleet view. `tokeman list --json` is its serialization.
pub enum Monitor { Running, Paused, NotInstalled }       // keeps_fresh(): the service reads every account
pub enum Origin { Stored, Fresh }
pub struct Overview {
    at, origin, read_at: Option<i64> /* the newest reading */, monitor,
    default_account, target_model, best_weekly_remaining,
    rotation: Rotation { mode, probe_interval_secs, min_session_remaining, min_weekly_remaining,
                         default_is_unmanaged, coverage, sampling, live_sessions },
    accounts: Vec<AccountView>,          // Claude, Codex, then the others, in configured order
    resets_on_offer: Vec<ResetOffer { account, offer, read_at, expires_at, weekly_refills }>,
    forecasts: Vec<ForecastView { ..Forecast, text, brief }>,      // per service; none where the history is too thin
    attention: Vec<AttentionView { key, level, text, fix, ..Attention }>,   // critical first
}
pub struct AccountView {
    service, service_label, name, label,
    held,                                // Claude Code's default, agy's login
    parked, login: Option<LoginView>,
    state: Option<State { word, level }>, viable: Option<bool>, error: Option<String>, notes: Vec<String>,
    limits: Vec<LimitView { name, note, ..Limit }>,   // in display order, as every surface names them
    reading: Option<Reading>,            // stored or fresh
}
pub struct Facts { origin, monitor, readings, recent /* the last BURN_HOURS */, offers, default_account, target_model,
                   agy_account, mode, .. }
impl Facts { fn on_disk(&Fleet, now) -> Self   /* no request, no write */ ; fn with_fresh(self, Vec<Reading>) -> Self }
impl Overview { fn build(&Fleet, Facts, only: Option<Service>, now) -> Self }

// attention.rs  (pure). The daemon notifies a key once, so what is said again when it gets closer has its
// threshold in the key.
pub enum Within { OneDay, ThreeDays }
pub enum Attention { DaemonStopped, BelowFloor { service }, RunsDry { service, at },
                     LoginRefused { account }, LoginEnds { account, ends_at, within }, LoginAgeing { account },
                     ResetExpires { service, account, expires_at, within },
                     CodexSeatExpires { home, account, profile_home, expires_at, why: Unkept } }   // serde: tag "kind"
pub enum Unkept { Tried(seats::Cause), Paused, NoDaemon, Overdue }
impl Attention { fn key(&self) -> String ; fn level(&self) -> Level }
pub fn find(&Overview, now: i64) -> Vec<Attention>

// forecast.rs  (pure). An account counts through its longest account-wide window; rooms and rates are added as
// fractions of one window. The walk spends the account that resets soonest first and refills at each reset.
pub struct Input<'a> { latest: &'a Reading, recent: &'a [Reading], refills: u32 /* resets on offer */ }
pub enum Outlook { Dry { at }, Beyond /* past HORIZON_SECS */, Idle }
pub struct Forecast { service, outlook, burn_hours, rate, room, span, accounts, measured, refills,
                      with_resets: Option<Outlook> }
pub fn forecast(Service, &[Input], model, floor, now) -> Option<Forecast>   // None: the data cannot support one

// commands/view.rs
pub async fn probe(&Fleet, only: Option<Service>) -> Vec<Reading>          // read now, record
pub async fn stored(&Fleet) -> Overview                                    // no request
pub async fn fresh(&Fleet, only: Option<Service>) -> Overview              // probe, then the view
pub async fn overview(&Fleet, force: bool) -> Overview                     // stored while the service reads; else fresh
pub fn ranked(&[Reading], Floors, model, RankOn) -> Vec<(&Reading, f64)>   // policy::rank_above
pub async fn best(&Fleet, Service, scope) -> Result<Best>                  // every `tokeman <service> best`
```

## Rules

- **One type per idea.** One type describes a limit (`quota::Limit`), one a
  read (`quota::Reading`), one the fleet (`ui::overview::Overview`). A
  server's own shape is a private struct in the module that parses it; a
  shape an earlier build stored is a private struct in `store/legacy.rs`.
- **One renderer per surface.** Each surface draws a limit in one place,
  from a `Limit`: `text::account_lines`, `tui::gauge_line`, the Swift
  `LimitRow`. What a limit is called
  (`limit_name`), what is said after it (`limit_note`), a reading's state
  word (`badge`), its one-line form (`summary`) and how a view describes
  itself (`view_caption`) are each one function in `ui/text.rs`. The fleet
  view carries the name and the note of every limit, the sentence and the
  remedy of every attention item and the line of every forecast, so a
  surface outside the crate (the Swift app) shows them and derives none.
- **Who sends a quota request.** The service's loop (the rotation cycle,
  the services poll, the Codex sweep, and the Codex mover for the other
  profiles once a running Codex is below its floor); `commands::view::probe`,
  behind every `<service> list`, `best` and `launch`, `--fresh`, `tokeman doctor`
  (which also reads the reset programs and the Codex credit list), and the
  surfaces when no service reads; `tokeman rotate status` and `rotate run`, which read Claude the way
  a cycle does; and the commands that act on one account (`codex use`,
  `codex watch`, the two `reset`s, `glm|kimi add`, `agy import`). Nothing in
  `ui/` sends a request: a surface calls `commands::view::overview`, which
  is the stored view while `Monitor::keeps_fresh()`.
- **A view says what it is.** Every `Overview` carries its origin and the
  time of its newest reading, and every surface shows both; a stored view
  the service has not refreshed for ten minutes is called stale.
- **A parser says what it did not recognise.** Beside each parser are the
  names it knows as constants: what it reads, and what has been seen and is
  not read, with where it was seen. Every other rate-limit header, body key
  and word of an enumerated field lands in `Reading::unrecognized` (the
  reset programs' in `ResetStatus::unrecognized`, the Codex credit list's in
  `CreditList::unrecognized`) as a path (`usage.limits[].kind=daily_all`,
  `header anthropic-ratelimit-unified-8h-reset`): names and enum words only,
  never the content of a free-form field. A read of a service that has not
  changed reports nothing. The daemon logs an item the first time it sees
  it; `tokeman doctor` is where it becomes a problem.
- **Nothing stringly typed for state.** Event kinds, backup states,
  verdicts, modes and sources are enums with fixed serde names.
- **The policy is pure.** `policy.rs` imports only `quota.rs` and the
  settings struct that carries the floors. It never reads the clock or a
  file, and never matches on `Service`. `attention.rs` and `forecast.rs` are
  pure the same way: the time is an argument.
- **tokeman never spends a reset by itself.** A reset on offer is shown,
  counted apart in a forecast and named in a notification; only
  `tokeman <service> reset` claims one.
- **No forecast is better than an invented one.** `forecast` returns
  `None` unless some account's history covers an hour of one reset window
  and every counted account's longest window has the same length.
- **One writer per file.** `claude/settings.rs` for settings.json,
  `account.rs` for tokens.toml, `daemon/cadence.rs` for the cycle state,
  `observe.rs` for the admission state, `history.rs` for the JSONL,
  `claims.rs` for claims.json, `codex/seats.rs` for codex-seats.json,
  `codex/mover.rs` for codex-moves.jsonl, `drift.rs` for drift.json, `daemon/notify.rs`
  for notified.json, `store.rs` for snapshots.db.
- **Printing and reading the terminal.** Nothing outside `ui/text.rs`
  prints, and nothing outside `ui/prompt.rs` reads the terminal.
  `commands/` and the domain modules return data; a long operation reports
  progress through `notice.rs`.
- **Bounded and announced.** Every subprocess wait has a timeout. Every lock
  acquire tries first and says when it waits.
- **Dry run means no write anywhere**, and the cycle says what it would have
  done.
- **No credential is printed or logged**, in any form.
- **Nothing spends a reset by itself.** A grant or a credit is claimed only
  by `tokeman claude reset` or `tokeman codex reset`. The rotation cycle,
  the Codex mover and every `launch` only choose among accounts.
- **What the daemon moves.** Claude Code's default (the cycle) and a running
  Codex (the mover). It never switches agy: a running agy cannot be told.
- **A pushed token has a keeper.** Whatever pushes a token to a running
  Codex (`mover::announce`, the only place that does) records the seat, and
  the daemon renews every recorded seat a day before its token ends. A seat
  nothing renewed is an `Attention::CodexSeatExpires` with its expiry, so a
  session never fails unannounced.
- **An uncertain claim is never retried as another.** With a claim pending,
  `Journal::begin` returns it with its request id or refuses a different
  grant or credit.
- **Removed commands are not aliased.** `cli::REMOVED` maps each 0.3
  spelling to its replacement and `main` refuses it with that text; a test
  pins the CHANGELOG's table to the constant.

## Files on disk

Config is `$XDG_CONFIG_HOME/tokeman` (default `~/.config/tokeman`), data
`$XDG_DATA_HOME/tokeman`, cache `$XDG_CACHE_HOME/tokeman`. Every file is
owner-only.

- `tokens.toml`: `[[tokens]]` (`name`, `key`, `access_token`,
  `refresh_token`, `expires_at`, `obtained_at`, `logged_in_at`,
  `refresh_expires_at`, `scopes`, `login_error`), `[settings]` (the launch
  settings, `services_interval_secs` and `notifications`), `[rotation]` (the floors and
  intervals; `codex_auto_move` and the Codex leaving and landing floors
  `codex_min_*` / `codex_land_min_*`), `[[codex_accounts]]`, `[[services]]` (`name`, `service`, `key`
  or `key_file`, Antigravity's grant, `label`). Comments survive every
  write; keys added later are optional.
- `~/.claude/settings.json` (or `$CLAUDE_SETTINGS`): the env keys
  `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_OAUTH_SCOPES`,
  `CLAUDE_CODE_OAUTH_401_WAIT_MS`, `TOKEMAN_ACCOUNT`, and the SessionStart
  hook command `tokeman rotate observe-session --pid "$PPID"`.
- `snapshots.db` (sqlite, WAL):
  - `readings(id, service, account, at, reading)`: one JSON `Reading` per
    row, indexed `(service, account, at)`. **Growth rule:** a row is written
    when the reading differs from the account's newest row in anything but
    `at`, or when that row is ten minutes old (`store::HOLD_SECS`).
  - `confirmed(service, account, at)`: when each account was last read. It
    makes the stored view as fresh as the last read, and when a reading
    changes, the old value is written once more at its last confirmation
    before the new row. A history therefore holds the first and last row of
    every hold: charts draw a held value as a step, and burn rates (each
    interval weighted by its length) are what one row per read would give.
    `since` and `recent` end each account's rows with the value it holds
    now.
  - `snapshots`, `codex_readings`, `service_readings`: written by builds
    before 0.4, and read by nothing but `tokeman history compact`. While one
    exists, every view of the history says to run it. The compaction:
    - holds the database exclusively (sqlite's exclusive locking mode): a
      process that has it open makes it refuse, and one that opens it
      meanwhile is refused. The lock is the process's, and closing any
      descriptor of the file gives it up, so the descriptor the copy reads
      through stays open until the connection is closed;
    - copies the file to `snapshots.db.pre-0.4` before it changes anything;
    - walks each old table one account after another, oldest row first,
      through the growth rule into a staging table, a transaction per batch
      with the place reached in `compact_progress`, so the next run
      continues a run that was killed; each account ends with its last read;
    - reads the old rows again and requires, per account, the same step
      function (the same value from the same instant), kept rows only where
      old rows stand, the same last time, and no value held past the hold;
    - then, in one transaction, appends the rows `readings` already has,
      makes the staging table `readings` and drops the old tables; and
      vacuums.

    A row that does not read (an unparseable time, a document in an unknown
    shape) is counted and left out, as it was by every read before.
- `snapshots.db.pre-0.4`: the database as it was before
  `history compact`, byte for byte. Nothing reads it.
- `claude-rotate-state.json` (cadence), `claude-admission-state.json`
  (session bindings, rejections on record), `claude-profile-reads.json`
  (cached profile answers and their backoff): each loads entry by entry, and
  an entry in a shape this build does not read is dropped alone.
- `claude-credential-history.jsonl`: which credential settings offered when,
  as fingerprints.
- `daemon.lock`: held exclusively by the daemon while it runs. A surface
  tests it with a shared, read-only, non-creating try-lock to decide
  whether to show the stored view or read for itself.
- `notified.json`: the attention keys the daemon has told, each with when
  it was told and when it last stood. A key is told once and forgotten six
  hours after it stops standing; the file is written before a notification
  is sent.
- `claims.json`: reset claims sent without a known outcome. An entry is
  dropped only by a read answer or `reset <account> --clear`.
  `reset-claims/<account>.pending` and `codex-reset-pending.json` from 0.3
  are read where they are; the first change to the journal folds them in and
  renames each to `<name>.imported`.
- `drift.json`: the unrecognised items the daemon has logged, by service,
  each with when and on which account's read it was first seen, and the
  version that judged them. Another version's file reads as empty: its
  known sets differ.
- `codex-seats.json`: one entry per CODEX_HOME whose running Codex holds a
  token tokeman pushed (`home`, `account`, `account_id`, `email`, `token` as
  a fingerprint, `expires_at`, `pushed_at`, `by`, `trouble`). Dropped when
  that Codex is gone or on another account.
- `codex-moves.jsonl`: one line per move of a running Codex (`at`, `home`,
  `from`, `to`, `remaining`, `span`, `by`), by the daemon, `codex watch` or
  `codex use`. Past 256 KiB the newest 200 are kept.
- `antigravity-access-tokens.json`, `agy-login-backup.json`,
  `claude-login-keychain-backup.json`, `claude-rotate.log`, the lock files,
  `claude-rotate.paused`, and the launchd launcher script and log.
- The launchd agent `com.ember.tokeman-claude-rotate` runs
  `tokeman rotate daemon`. Those two invocations (the agent's and the
  hook's) are what an installed service depends on across an upgrade;
  `tokeman rotate install` rewrites the plist, the launcher and the hook for
  the binary that runs it.

## What the design assumes about the services

- Claude Code re-applies the settings env block on every change and rebuilds
  its API client credentials from it, so a live session spends the current
  default from its next request.
- A refresh revokes the access token it replaces within seconds. The token
  endpoint reports `refresh_token_expires_in`, and refreshes do not extend
  it.
- Anthropic's OAuth and usage endpoints answer only a client that identifies
  as Claude Code: premium models refuse subscription tokens otherwise, and
  the reset programs are eligible only for that user agent.
- `/api/oauth/usage` rate-limits per address. A 429 holds back the account
  that drew it, and every account once two have.
- An account whose `limits[]` has no session row has no 5-hour limit.
  Fable's weekly limit is a share of the weekly (`weekly_scoped_shares`),
  and its header window can read over 1.0.
- A Fable grant (`gives_back_weekly`) gives back only Fable's use of the
  weekly.
- The ChatGPT edge challenges h2 requests with Codex's headers and replays
  its cookie only across a shared cookie jar, so the Codex client is
  HTTP/1.1 with one jar.
- Codex's 401 recovery refuses an `auth.json` whose account id changed; the
  app-server `reload` path does not.
- A Codex app-server answers `account/rateLimits/read` with the account-wide
  windows under `rateLimits` (`primary`, `secondary`, each with
  `usedPercent` and `windowDurationMins`), the per-feature buckets under
  `rateLimitsByLimitId`, and `accountId` (codex 0.167.0 schema). Its
  `account/rateLimits/updated` push is sparse. It fetches the usage from the
  backend for each read.
- A token given to a Codex app-server with `account/login/start`
  (`chatgptAuthTokens`) is held in memory by an `ExternalAuthBridge`
  (codex 0.167.0, `app-server/src/request_processors/account_processor.rs`,
  `app-server/src/external_auth.rs`). `AuthManager::auth`
  (`login/src/auth/manager.rs`) renews ahead of expiry only auth it manages
  itself, so the pushed token is used until a 401. Then the bridge sends
  `account/chatgptAuthTokens/refresh` to every initialized connection and
  waits ten seconds; with no client connected the turn fails. A later
  `account/login/start` replaces the token. Codex access tokens live ten
  days.
- z.ai and Kimi serve Claude Code from an Anthropic-compatible endpoint
  (bearer token and API key respectively), and Claude Code 2.1.322 sends a
  key from its environment in preference to a `CLAUDE_CODE_OAUTH_TOKEN` in
  settings.json.
- The Codex usage body puts whichever window exists first in
  `primary_window`, so windows are named by `limit_window_seconds`.
- A running agy does not pick up a swapped Keychain login and writes its own
  account back at its next token refresh; the Antigravity app re-reads the
  login when asked over its loopback port.
- A Background-class LaunchAgent is scheduled below every normal process, so
  the agent is Standard.
- `osascript -e 'display notification'` run from a LaunchAgent posts under
  Script Editor's name and is subject to that app's notification permission.
- A Claude grant that clears `seven_day` and a Codex reset credit each
  refill an account's weekly window once per reset left; a grant with
  `gives_back_weekly` does so for Fable work alone. Accounts of one service
  are near enough the same size to add their windows.

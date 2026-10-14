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
  the 5h and 7d windows and that model's own weekly window (`7d_oi` for
  Fable); per-model weekly limits come from the profile endpoint, which needs a
  full-scope login and rate-limits aggressively; rejections seen in a
  transcript are the only signal for some limits. A reading is a merge of
  these sources, each tagged.
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
  quota.rs       Window, Bucket, ModelKey, Source, Reading, Level: what is known about an account's room
  policy.rs      PURE. Mode, Floors, viability, landing choice, Decision. No IO, no clock, no strings.
  history.rs     credential history: Event, fingerprint, account_at (JSONL, same format as today)
  store.rs       sqlite snapshot history (same schema as today), Snapshot, Series
  observe.rs     ObservedLimit, SessionBinding, the SessionStart hook input, transcript scan
  anthropic/
    mod.rs       the HTTP client that identifies as Claude Code; error classification
    probe.rs     the Haiku probe and the optional target-model sample -> Reading
    profile.rs   /api/oauth/usage: parser, on-disk cache, 429 backoff
    oauth.rs     PKCE login, code exchange with retry, refresh
    resets.rs    cedar_ember / juniper_tide offers and claims
  claude/
    settings.rs  ~/.claude/settings.json env block: install, clear, inspect (the only writer, 0600)
    keychain.rs  macOS: capture, suppress and restore the /login Keychain record
    sessions.rs  live Claude processes (bounded census) and which account each spends
  codex/
    mod.rs       client (HTTP/1.1 + cookie jar), edge-challenge detection
    auth.rs      auth.json read/write, seating
    appserver.rs JSON-RPC over the app-server Unix socket
    probe.rs     usage probe
    refresh.rs   token refresh
    commands.rs  add/discover/import/list/refresh/best/seat/reseat/watch
  daemon/
    mod.rs       the loop
    cycle.rs     one cycle: refresh due grants, probe, observe, decide, install, record
    cadence.rs   typed on-disk cycle state: last probe, mode, interval, backoff (keyed to the refused credential), samples
    lock.rs      the rotation lock
    service.rs   launchd plist, install/uninstall (macOS)
  ui/
    text.rs      gauges, tables, status (every println in one module)
    chart.rs     PNG + iTerm2 inline, shared segment logic
    tui.rs       --watch
    tray.rs      egui tray (feature "tray")
  cli.rs         clap definitions and dispatch; no logic
  launch.rs      the claude launch command and terminal handoff
macos/tray/      Swift menu-bar app; reads tokens.toml, writes only through `tokeman settings set`
```

Dropped from the tree (history keeps them): the web dashboard and its D3
page, the transcript session browser, `per_session_reserve` (no decision it
can move once sessions follow the default), legacy Opus/Sonnet-specific
columns in new readings (still read from old rows).

## Core types

```rust
// quota.rs
pub struct Window { pub used: f64 /* 0..1 */, pub resets_at: i64 /* epoch s */ }
pub struct ModelKey(String);                 // "opus48", "fable51", "fable" (family-wide)
pub enum Source { Headers, Profile, Observed, Carried }
pub struct Bucket { pub key: ModelKey, pub label: String, pub window: Window, pub source: Source }
pub enum Verdict { Allowed, Rejected, Unknown }
pub struct Reading {
    pub account: String,
    pub at: i64,
    pub verdict: Verdict,
    pub session: Option<Window>,            // 5h
    pub weekly: Option<Window>,             // 7d
    pub buckets: Vec<Bucket>,               // per-model weekly, including Fable
    pub overage: Option<Overage>,
    pub error: Option<ProbeError>,          // typed: Network, Auth(Refused|Revoked|Expired), RateLimited, Edge
}
impl Reading { fn remaining(&self, w: Which) -> Option<f64>; fn bucket(&self, key: &ModelKey) -> Option<&Bucket> }

// account.rs
pub enum Credential<'a> { Setup(&'a str), Login { token: &'a str, expires_at: i64, scopes: &'a [String] } }
pub struct Account { name, setup_key: Option<String>, login: Option<LoginGrant>, login_error: Option<String> }
impl Account { fn credential(&self, now: i64) -> Option<Credential> }   // login if live, else setup
pub struct Fleet { pub accounts: Vec<Account>, pub codex: Vec<CodexAccount>, pub launch: LaunchSettings, pub policy: PolicySettings }
impl Fleet { fn load() ; fn update(|&mut Fleet| -> T) -> T  /* locked RMW, comments kept */ }

// policy.rs  (pure; the tests are the spec)
pub enum Mode { Normal, Sip }
pub struct Floors { pub session: f64, pub weekly: f64 }
pub struct Policy { normal: Floors, sip: Floors, landing: Floors, intervals: Intervals, sample_secs: u64 }
pub enum Decision<'a> { Stay, Switch { to: &'a Reading, why: Reason }, Drain, NoneViable }
pub fn decide(fleet: &[Reading], current: Option<&str>, mode: Mode, target: Option<&ModelKey>, policy: &Policy) -> Decision
pub fn mode_for(fleet: &[Reading], expected: usize, previous: Mode, policy: &Policy, target: Option<&ModelKey>) -> Mode

// history.rs
pub enum EventKind { Install, Activate, Refresh, Upgrade, Fallback, Repair, Pause, Revoke }   // serde: same strings as today
pub struct Event { at: i64, account: Option<String>, fingerprint: Option<Fingerprint>, expires_at: Option<i64>, kind: EventKind }
pub fn account_at(events: &[Event], at: i64) -> Option<Option<String>>

// observe.rs
pub enum Family { Opus, Sonnet, Fable, General }
pub enum LimitWindow { Session, Weekly }
pub struct ObservedLimit { account, family, model: Option<ModelKey>, window, observed_at, resets_at, session: Option<String>, source: LimitSource }
```

Rules the types enforce:

- Nothing stringly typed for state: event kinds, backup states, verdicts,
  modes, sources are enums with serde names fixed to today's on-disk strings.
- `policy.rs` imports nothing from the rest of the crate but `quota.rs`. It
  never reads the clock or a file; callers pass `now`.
- Exactly one writer per file: `claude/settings.rs` for settings.json,
  `account.rs` for tokens.toml, `daemon/cadence.rs` for the cycle state,
  `observe.rs` for the admission state, `history.rs` for the JSONL.
- Every subprocess wait has a timeout. Every lock acquire tries first and
  says when it waits.
- Dry run means no write anywhere, and the cycle says what it would have done.

## Compatibility kept

- `tokens.toml`: same tables and keys (`[[tokens]]` with `name`, `key`,
  `access_token`, `refresh_token`, `expires_at`, `obtained_at`, `scopes`,
  `login_error`; `[settings]`; `[rotation]`; `[[codex_accounts]]`). Comments
  survive every write.
- `~/.claude/settings.json` env keys: `CLAUDE_CODE_OAUTH_TOKEN`,
  `CLAUDE_CODE_OAUTH_SCOPES`, `CLAUDE_CODE_OAUTH_401_WAIT_MS`, `TOKEMAN_ACCOUNT`;
  the SessionStart hook command `tokeman rotate observe-session --pid "$PPID"`.
- `claude-credential-history.jsonl` events, `claude-admission-state.json`,
  `history.db` snapshots: read as they are; new rows use the same schema.
- CLI commands and flags as documented in the README. `browse` is removed.
- Cadence state and the profile-read cache are caches and start fresh.

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
  caches it for 60 seconds (2026-09-25).
- Reset programs (`cedar_ember`, `juniper_tide`) are eligible only for the
  Claude Code user agent (2026-09-25).
- The ChatGPT edge challenges h2 requests with Codex's headers and replays
  `__cf_bm` only across a shared cookie jar (2026-08).
- Codex's 401 recovery refuses an `auth.json` whose account id changed; the
  app-server `reload` path does not (2026-08).
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

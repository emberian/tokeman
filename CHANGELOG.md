# Changelog

## 0.4.0 - 2026-11-11

tokeman now reads five services (Claude, Codex, Antigravity, GLM, Kimi)
through one model: a read is a `Reading`, and what it reported is a list of
`Limit`s, each a window named by its length over the whole account, one
model or one named group. One history table, one ranking, one fleet view
that every surface draws, and a command line that names the service first.
The spellings 0.3 had are removed, not aliased; each fails naming its
replacement (table below). tokens.toml, the settings.json keys, the hook
and what the installed service invokes are unchanged, so an installed
0.3 service keeps working across the upgrade; `tokeman rotate install`
rewrites the plist, launcher and hook for the new binary.

### Claude

- The probe keeps everything the rate-limit headers say: each window's own
  `-status` and `-surpassed-threshold`, every `unified-claim`, the fallback,
  the upgrade paths, the low-priority lane and the grace allowance. Only a
  window's own status takes part in viability (a window rejected by its own
  status is above no floor); the rest is shown and never counted as room.
  Use over 100% is kept as reported.
- `/api/oauth/usage` is read from `limits[]`: session and weekly rows fill
  the windows the headers left out, model-scoped rows become per-model
  limits, and `weekly_scoped_shares` gives a limit its size as a share of
  the weekly (Fable: half). A limit that is a share is judged on its room in
  weekly terms.
- An account whose `limits[]` has no session row has no 5-hour limit
  (verified Ultra). Every surface says "no 5h limit" instead of treating the
  missing window as unread.
- A 429 from the usage endpoint holds back the account that drew it, and
  every account once two have (doubling from ten minutes to an hour); each
  account serves its last good answer meanwhile.
- Landing spends expiring room first: among accounts above the landing
  floors, one whose weekly window resets within 24 hours ranks first
  (soonest first); otherwise the longest dwell lands, as before. This moves
  where a switch lands, never whether one happens.
- A login records the refresh token's reported expiry (`refresh_expires_at`
  in tokens.toml; a refresh keeps the earlier date). `tokeman list` names
  the date and says to log in again from three days before it.
- Granted resets are read as Claude Code 2.1.322 reads them. A program the
  server did not evaluate is listed as unknown, never as "no resets"; a
  grant that gives back weekly use (the Fable launch resets) says so. A
  claim is journaled before it is sent and resent with the same request id
  until an answer is read; `tokeman claude reset <account> --clear` forgets
  one.
- `tokeman claude best` ranks the accounts as the rotation would land.

### Codex

- Windows are named by their length (5h, weekly, daily, any other), never
  by their position in the usage body, which now puts a weekly-only
  account's window first. Additional limits are scoped to their model or
  feature. The body is decoded field by field, so one field changing shape
  no longer loses the rest.
- New from the usage body: per-model availability, the workspace spending
  cap, reset credits. `tokeman codex reset <account>` spends one credit on
  an account at its limit (asks first; a claim without a definite answer is
  resent with the same request id, and `--clear` forgets it).
- `tokeman codex best` ranks through the rotation's policy: ties fall to
  the name, and a window at 100% is never a landing. `--limit <text>` ranks
  on a per-model or per-feature limit.
- `tokeman codex use [<account>]` moves a running Codex (the best viable
  account when none is named) or, with `--into`, copies credentials into
  another profile.

### Antigravity, GLM, Kimi

- New: `[[services]]` accounts in tokens.toml, read-only quota for the GLM
  Coding Plan (`tokeman glm add`), Kimi Code (`tokeman kimi add`) and Google
  Antigravity (`tokeman agy import`, from agy's Keychain item and the app's
  state database). The daemon reads them on their own cadence
  (`services_interval_secs`, default 300) in a task the rotation never
  waits on; failures are typed and logged only when they change.
- `tokeman agy use <account>` signs the agy CLI in as an account (macOS;
  manual, the daemon never switches it), and `--previous` puts back the
  login it replaced. An open Antigravity app is told to follow; an editor's
  `agy acp` holding the old account is named; an interactive agy cannot be
  told and writes its account back at its next token refresh, so the switch
  is refused while one runs unless `--force`. Nothing is signalled or
  logged out.
- `tokeman agy run <account> [-- <agy args>]` starts agy as an account
  beside the normal login (macOS): a home of its own with a token file from
  tokeman's grant, and `SSH_TTY` so agy keeps its login in that file and
  never touches the shared Keychain item. `agy list` shows these sessions;
  `agy use` ignores them.
- `tokeman agy|glm|kimi best [--group <text>]` ranks the accounts by room:
  weekly room expiring within 24 hours first, and an account with a spent
  limit is left out.

### What needs you, and a forecast

- tokeman says what needs you: a Claude login that ends within three days
  (and again within one), a login that was refused, a reset on offer (a
  Claude grant, a Codex credit) that expires within three days (and one), a
  service none of whose accounts is above its floor, a fleet forecast to
  run dry within 12 hours, and a background service that is installed but
  not running. The list opens `tokeman`, `tokeman list` and the macOS tray,
  each item with the command that deals with it, and is `attention` in the
  fleet view's JSON.
- The background service sends each item once as a desktop notification
  (macOS `osascript`; `notify-send` elsewhere when installed; otherwise the
  rotation log alone), and again only at its nearer threshold or after it
  has been gone for six hours. `tokeman notify --test` sends one so it can
  be seen and allowed; `tokeman notify` lists what stands.
  `tokeman settings set notifications false` keeps them to the log.
- A forecast per service: when its accounts run dry for the startup model
  at the rate of the last 6 hours, counting each account's weekly room
  above the floor and the resets of windows inside the next week, with what
  the unused resets on offer would add stated apart. tokeman never spends a
  reset for it. It is shown by `tokeman`, `tokeman list`, `tokeman stats`
  and the tray's header, and is `forecasts` in the JSON. A service whose
  history does not cover an hour of one reset window has no forecast.
- The fleet view carries each limit's display name and note (`limits` on an
  account); the macOS tray draws those and no longer derives its own.
- `tokeman codex list --json` and every Codex reading name when the first
  available reset credit expires (`reset_credits.expires_at`).

### Noticing when a service changes

- Every parser reports what it did not recognise: a rate-limit header, a
  key of a usage body, a `limits[]` kind, a z.ai limit type, a Kimi window,
  an Antigravity bucket window. The names each parser knows sit beside it as
  constants, seeded from what it reads and what has been seen and is not
  read, so a read today reports nothing. The items travel with the read
  (`unrecognized` in a `Reading`, left out when empty) as names only.
- The background service logs each new item once (`drift.json`).
- New: `tokeman doctor [--json]` reads every account once and reports what
  was reached, each credential's state, the unrecognised items, the
  installed tools against the versions this build was verified with (Claude
  Code 2.1.322, codex 0.167.0, agy 1.5.6, Antigravity 2.31.2), the
  SessionStart hook, the background service, file modes, agy's login and
  unanswered reset claims. Exit 0 when healthy, 1 with problems, each with
  the command that fixes it.

### Commands

- Service first: `tokeman claude|codex|agy|glm|kimi <verb>` with the same
  verbs wherever they apply (`add`, `remove`, `list`, `use`, `best`,
  `reset`). `<service> list` reads that service now.
- `tokeman list` shows every account of every service from stored reads,
  with no request and no write; `tokeman list --json` is the fleet view the
  trays read.
- Bare `tokeman`, `tokeman --watch` and both trays are viewers while the
  background service runs: they show the stored view and send no request.
  The service counts as running while its daemon holds `daemon.lock`, on
  macOS and under any other supervisor; a second daemon refuses to start.
  Without a running service they read every account themselves. `tokeman
  --fresh`, `r` in `--watch` and the trays' reload button read now. Every
  view says whether it is stored and how old it is, and says STALE when the
  service has not read for ten minutes.
- `tokeman history`, `stats` and `chart` take `--service`. `chart --metric`
  names a window by length (`5h`, `weekly`, `monthly`, `daily`), `overage`,
  or a limit by its model, group or label (`fable`, `"Opus 4.8"`, `gemini`,
  `"gemini 5h"`, `spark`); without it the service's first account-wide
  window is drawn.
- Removed spellings and what to run instead (the `services` ones were only
  in unreleased builds):

| 0.3 | 0.4 |
| --- | --- |
| `tokeman rotate use` | `tokeman claude use <account>` |
| `tokeman resets list` | `tokeman claude reset` |
| `tokeman resets use` | `tokeman claude reset <account> [--grant <id> \| --session] [--yes]` |
| `tokeman resets clear` | `tokeman claude reset <account> --clear` |
| `tokeman resets` | `tokeman claude reset [<account>]` |
| `tokeman codex seat` | `tokeman codex use <account> --into <codex-home>` |
| `tokeman codex reseat` | `tokeman codex use [<account>] [--codex-home <dir>] [--dry-run] [--force]` |
| `tokeman services list` | `tokeman agy list, tokeman glm list or tokeman kimi list (tokeman list shows every service from stored reads)` |
| `tokeman services add` | `tokeman glm add <name> --key-file <path> or tokeman kimi add <name> --key-file <path>` |
| `tokeman services remove` | `tokeman agy remove <name>, tokeman glm remove <name> or tokeman kimi remove <name>` |
| `tokeman services import` | `tokeman agy import` |
| `tokeman services use` | `tokeman agy use <account> [--previous]` |
| `tokeman services best` | `tokeman agy best [--group <text>], tokeman glm best or tokeman kimi best` |
| `tokeman services` | `tokeman agy list, tokeman glm list or tokeman kimi list (tokeman list shows every service from stored reads)` |
| `tokeman login` | `tokeman claude login [<account>] [--more]` |
| `tokeman add` | `tokeman claude add <name> <key>` |
| `tokeman remove` | `tokeman claude remove <name>` |
| `tokeman refresh` | `tokeman claude refresh [<account>]` |

### Behaviour changes to know about

- `--json` output is the new model: `tokeman --json`, `tokeman list --json`
  and `tokeman rotate status --json` print the fleet view; `<service> list
  --json` and `history --json` print `Reading`s.
- Bare `tokeman` covers every service, and while the service runs it shows
  stored reads instead of probing (`--fresh` probes).
- `tokeman list` no longer prints masked setup keys; it says which
  credential each Claude account offers.
- One-day windows are labelled `daily`. `history` on a missing database
  reads as empty.
- `stats` reports every limit of an account, and its mean and deviation
  weight each interval by its length (the latest and peak rates are as
  before).
- `chart --metric` no longer takes `opus-weekly` / `sonnet-weekly`; name the
  model.
- Chart lines from before 0.3 (the Opus/Sonnet columns) and credential
  events reconstructed from the old rotation log are no longer read, and
  the `usage_key` spelling in tokens.toml is no longer accepted.

### Internals

- `quota::{Service, Span, Scope, Limit, Reading, ReadError, Detail}` is the
  one model; every reader returns a `Reading`, and `policy::rank_above` is
  the one ranking behind the rotation's landing and every `best`.
- History: every read is one JSON row in `readings`. The tables of earlier
  builds are read where they are and never altered. A reading that says
  what the account's last row says adds no row for ten minutes; the last
  confirmation is kept apart, and a change is written after a row that
  closes the hold, so charts draw a held value as a step and burn rates are
  the same as from one row per read. `tokeman list` asks the old tables only
  for an account that has no new row.
- `ui::overview::Overview` is the one fleet view; each surface has one limit
  renderer. `commands/` returns data, every print is in `ui/text.rs`, every
  terminal read in `ui/prompt.rs`; the library reports progress through
  `notice`.
- `claims.rs` is the one journal of uncertain reset claims (Claude grants,
  Codex credits); the two 0.3 layouts are folded in on the first change.
- Cadence state, the admission state and the profile cache load entry by
  entry: one in a shape this build does not read is dropped alone.

## 0.3.1 - 2026-10-31

- A reading no longer needs both shared windows. Since late October the
  API omits a window with no usage from a healthy answer (a 200 carries the
  weekly headers and no `5h-*` headers at all), which made every healthy
  account read as "no quota headers", kept the daemon on the fast cadence
  with a probe that never completed, and left the default where it was.
  An unreported window limits nothing; status and log lines show it as
  `--` / `unreported`.
- Parsed and pinned by tests: today's 200 and 429 header shapes, including
  the new per-window `-status` headers.

## 0.3.0 - 2026-10-14

A rewrite from the knowledge 0.2.0 accumulated, built to DESIGN.md. Same
account file, settings.json keys, hook, history files and commands; the
code is a library with no UI dependency plus a thin binary, organised by
domain, with the rotation policy as a pure module whose tests are its
specification.

- Removed: the web dashboard and transcript session browser (`tokeman
  browse`), `tokeman usage capture` (`tokeman login` replaced it; a
  captured token already on disk still serves profile reads),
  `per_session_reserve`, and the legacy Opus/Sonnet-specific
  fields (old history rows still read).
- Readings carry per-model buckets with a canonical key and a source
  (profile, headers, observed rejection, carried sample); the family-wide
  Fable limit from the `7d_oi` header is one of them.
- Every state on disk is a typed enum with the same spelling as before.
- A dry run writes nothing anywhere; every subprocess wait is bounded; every
  lock wait is announced; the daemon runs at Standard priority.
- Probe failures are typed (no credential, network, HTTP) and grouped by
  cause in log lines.
- Cargo: axum is test-only; the Linux build has no OpenSSL.

## 0.2.0 - 2026-10-14

The first release since March. The daemon now manages refreshable browser
logins rather than only setup tokens, reads per-model limits the way Claude
Code does, and keeps the default on one account until that account's own
floor instead of hopping.

### Accounts and credentials

- `tokeman login` runs the browser OAuth flow Claude Code uses, with the same
  scopes, and stores a refreshable grant. Rotation installs that account's
  access token together with `CLAUDE_CODE_OAUTH_SCOPES` and a 401 wait, and
  falls back to the setup token when a login is missing or refused.
- The daemon refreshes every grant before expiry under the rotation lock and
  re-reads the account under the config lock before redeeming, since refresh
  tokens are single-use. `tokeman refresh [<name>]` does it on demand.
- `tokeman resets` lists and claims usage-limit resets (never automatically).
- Every write to `tokens.toml` is a locked read-modify-write that preserves
  comments. The Swift tray now writes through the new `tokeman settings set`
  command instead of rewriting the file from its own parser, which had
  discarded login grants, Codex accounts, floors and comments.
- `tokeman codex` manages Codex (OpenAI) profile directories: probe, refresh,
  seat, and migrate a running Codex between accounts without a restart.

### Rotation policy

- The default stays until its own floor (7% five-hour, 5% seven-day) and
  moves only to an account above the landing floors (25% / 10%); with none it
  drains in place to the sip floors. Every switch cold-starts the prompt cache
  of every live session, which the old policy triggered a dozen times a day.
- Live Claude processes follow the default: Claude Code re-reads the settings
  credential for every API client it builds, so there is no tail of sessions
  left on the old account and no "abandoned drain" warning.
- Per-model weekly limits are read from the profile endpoint and the
  `7d_oi` header the way Claude Code reads them; every model bucket is shown,
  Fable is its own family, and the Fable limit is sampled with an occasional
  target-model probe. A Fable rejection observed in a transcript records a
  Fable bucket, not the account's general weekly window.
- Profile-usage reads are cached for five minutes and back off on 429.
- The SessionStart hook binds only sessions that spend the Anthropic
  subscription: sessions pointed at a third-party `ANTHROPIC_BASE_URL`,
  Bedrock or Vertex are left alone (`TOKEMAN_SESSION_SPENDS` overrides).

### Daemon and operations

- The LaunchAgent runs at Standard priority. At Background priority its
  20-second cycle stretched to 10-24 minutes under a load average of 200,
  which left the default on an exhausted account exactly when it mattered.
- The process census is bounded (10 s) and reports "unknown" rather than
  "no sessions" when it fails; a command waiting for the rotation lock says
  so on stderr instead of appearing hung.
- Rotation state forgets accounts removed from the config; a repaired
  credential is probed on the next cycle instead of waiting out its backoff;
  `rotate run --dry-run` is read-only; the captured /login Keychain backup
  survives a later `tokeman add`.
- The local dashboard (`tokeman browse`) answers only to localhost, so a
  page in the user's browser cannot read session previews through DNS
  rebinding; d3 is version-pinned with Subresource Integrity; settings.json
  is written owner-only once it carries a token.

### Build and release

- TLS is rustls with the OS trust store on every platform; Linux builds no
  longer need OpenSSL.
- The lockfile is current and `cargo audit` is clean for the default build.
- CI (`ci.yml`) runs rustfmt, clippy, the unit tests and rustdoc on Ubuntu and
  macOS; every GitHub Action is pinned by commit, and `dist-workspace.toml`
  carries the same pins so `dist generate` keeps them.
- `scripts/check-linux.sh` runs the Linux checks from a macOS checkout.

## 0.1.1 - 2026-03-04

- Chart legend moved to bottom-left; macOS CI runner fixed.

## 0.1.0 - 2026-03-03

- First release: gauges, `launch`, startup-default rotation, charts, tray.

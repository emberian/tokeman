# tokeman

tokeman keeps Claude Code, Codex and agy on a subscription account with room
left, and shows the rest. It rotates the one credential slot Claude Code
reads across a fleet of Anthropic accounts, keeps Codex (OpenAI) profiles
alive and moves a running Codex between them, signs the `agy` CLI in as the
Antigravity account you choose, and reads the quota of GLM and Kimi coding
plans beside them.

Every service reports the same thing, a list of limits, and tokeman shows
them the same way: one gauge per limit, named by what it covers and how long
its window is.

```
 tokeman — stored view, last read just now by the background service  (`tokeman --fresh` reads now)

 ember@elide — Claude  * the default  allowed  ready
   weekly       ██████████████████████████████░░░░░░░░░░  76% left  resets Fri 5:00PM
  *Fable weekly ████████████████████████████████████████ 100% left  resets Fri 4:59PM  profile; 50% of the weekly, 50% of the weekly left for it
   no 5h limit on this account
   read just now

 work — Codex  [plus]  ok
   5h     ████████████████████████████████████░░░░  90% left  resets 2h10m (4:00PM)
   weekly ███████████████████████░░░░░░░░░░░░░░░░░  58% left  resets Tue 12:00PM
   read 12m ago
```

Commands are service first: `tokeman claude|codex|agy|glm|kimi <verb>`, with
the same verbs (`add`, `remove`, `list`, `use`, `best`, `launch`, `reset`)
wherever they apply. `tokeman <service> launch` starts the tool that spends
that service's quota on its best account. The shape of the code is written down in [DESIGN.md](DESIGN.md).

## Install

From source, on macOS:

```sh
./scripts/install.sh
```

The installer repairs the installed binary's ad-hoc signature after Cargo
copies it, then installs the background service. Set
`TOKEMAN_CODESIGN_IDENTITY` to sign with a stable identity. On Linux the
script builds and installs the binary; run `tokeman rotate daemon` under your
own init system.

Prebuilt binaries for macOS and Linux are on the
[releases page](https://github.com/emberian/tokeman/releases).

Upgrading from 0.3: the account file, the settings.json keys and the hook are
unchanged, and an installed service keeps working. The command spellings
changed; an old one fails naming its replacement, and
[CHANGELOG.md](CHANGELOG.md) has the table. The reads 0.3 stored are not shown
until `tokeman history compact` has rewritten them, once: it keeps the
database as it was beside it (`snapshots.db.pre-0.4`), compares every
account's history before and after, and only then drops the old tables.
`--dry-run` counts what it would keep. It can be interrupted and run again,
and no other tokeman reads or writes the history while it runs (the
background service keeps rotating; the reads it makes meanwhile are not
recorded).

## Everything at once

```sh
tokeman                  # the fleet view: every account of every service
tokeman --fresh          # read every account now, then show it
tokeman --watch          # the same as a live dashboard
tokeman list             # one line per account, from stored reads
tokeman list --json      # the fleet view as JSON (what the tray reads)
tokeman history [--service S] [--last N | --since HOURS] [--account NAME] [--json]
tokeman history compact [--dry-run]       # once after upgrading from 0.3: bring the old reads in
tokeman stats   [--service S]             # burn rate and time to depletion, per limit; the forecast
tokeman notify  [--test]                  # what needs you now; --test sends one notification
tokeman chart   [--service S] [--metric M] [--hours N]   # PNG, inline in iTerm2
tokeman settings [set <key> <value>]      # launch settings and the Codex mover's; the tray writes through this
```

**Who reads the accounts.** While the background service runs
(`tokeman rotate install`), it is the only reader: bare `tokeman`,
`tokeman --watch` and the tray show the stored view and send no request.
Every view says that it is stored and how old it is, and says STALE when the
service has not read for ten minutes. The service counts as running while its
daemon holds `daemon.lock`, however it was started; an installed service whose
daemon has died shows as not running. Without a running service (or while
rotation is paused) those surfaces read every account themselves. `--fresh`, `r` in
`--watch` and the tray's reload button read now either way. `tokeman list`
never sends a request; `tokeman <service> list` always reads that service.

**What needs you.** `tokeman`, `tokeman list` and the macOS tray open with a
"needs attention" section, each item with the command that deals with it: a
Claude login that ends within three days (and again within one) or was
refused, a reset on offer that expires within three days (and one), a service
none of whose accounts is above its floor, a fleet forecast to run dry within
12 hours, a background service that is installed but not running. The running
service sends each item once as a desktop notification (below).

**The forecast.** For each service in use, one line says when its accounts
run dry at the rate of the last 6 hours: every account's weekly room for the
startup model above the floor, spent soonest-reset first, each window refilled
at its reset if that falls inside the next week. What the unused resets on
offer would add is stated apart; tokeman never spends a reset by itself. The
accounts are added as if they were the same size. With less than an hour of
readings of one reset window there is no forecast, and `tokeman stats` says so.

`--service` takes `claude` (the default), `codex`, `agy`, `glm` or `kimi`.
`chart --metric` names the limit to draw: a window by length (`5h`, `weekly`,
`monthly`, `daily`), `overage`, or a limit by its model, group or label
(`fable`, `"Opus 4.8"`, `gemini`, `"gemini 5h"`, `spark`); without it the
service's shortest account-wide window is drawn. The PNG is saved under
`~/.cache/tokeman/chart.png` unless `--output` says otherwise. A value that
did not change between reads is drawn as a step.

In `--watch`: `j`/`k` move between accounts, `r` reads now, `c` and `C` show
the chart of the selected account's service, `Tab` steps through its limits,
`[`/`]` change the range, `q` quits.

## Claude

```sh
tokeman claude login [<name>] [--more]   # browser login (added if the account is new)
tokeman claude add <name> <key>          # a `claude setup-token` key instead, or as a fallback
tokeman claude remove <name>
tokeman claude refresh [<name>]          # renew grants now (the service does this before expiry)
tokeman claude list                      # read every account now
tokeman claude best                      # the accounts ranked as the rotation would land
tokeman claude use <name>                # make an account the default now
tokeman claude reset [<name>]            # choose a usage-limit reset, review, then confirm
tokeman claude launch [--auto] [-- claude args]   # pick the default, then start claude
```

Each login round prints a claude.com URL; sign in as that account and paste
the code the page shows. This grants the scopes Claude Code's own `/login`
grants, including `user:profile`, with a refresh token the service renews in
the background. `tokeman list` says when a login ends and asks for a new one
from three days before.

A setup token is inference-only: a session running on one cannot send
`/feedback`, use Remote Control, or read profile usage. When an account has
both, the login is used and the setup token is kept for the day the login is
refused.

### How the default is chosen

The service writes the default account's credential into the `env` block of
`~/.claude/settings.json`:

| Key | Value |
|-----|-------|
| `CLAUDE_CODE_OAUTH_TOKEN` | the account's login access token, or its setup token |
| `CLAUDE_CODE_OAUTH_SCOPES` | the login's granted scopes (omitted for setup tokens) |
| `CLAUDE_CODE_OAUTH_401_WAIT_MS` | `120000` |
| `TOKEMAN_ACCOUNT` | the account name |

Claude Code re-reads that credential for every API client it builds, so every
live session spends the current default from its next request. A switch moves
everyone at once and cold-starts every session's prompt cache, so the policy
switches rarely:

1. The default stays until one of its own windows reaches a floor: 7% of the
   five-hour window or 5% of the weekly window. A limit on the startup model
   (the Fable limit, a per-model weekly limit) counts as a weekly window.
2. It leaves only for an account with real room: at least 25% five-hour and
   10% weekly remaining. Among those, an account whose weekly window resets
   within 24 hours lands first (its room is about to be lost); otherwise the
   one that will last longest.
3. With nowhere worth landing, it drains in place to 2% five-hour / 3%
   weekly, or until it is refused, and only then moves to the roomiest
   account above those floors.

Probing runs every 120 seconds, and every 20 seconds while the default is
near a floor or nothing is above the normal floors. A partial probe never
selects a replacement; an account whose credential is refused is rechecked on
a backoff until its credential changes. The thresholds are in tokens.toml:

```toml
[rotation]
normal_min_5h_remaining = 0.07
normal_min_7d_remaining = 0.05
land_min_5h_remaining = 0.25
land_min_7d_remaining = 0.10
sip_min_5h_remaining = 0.02
sip_min_7d_remaining = 0.03
normal_probe_interval_secs = 120
sip_probe_interval_secs = 20
target_model_sample_secs = 1800
```

### What a read sees

Each probe is a one-token Haiku request; the `anthropic-ratelimit-unified-*`
headers of the answer report the weekly window, the five-hour window when the
account has one and has used it, the Fable limit, and each window's own
status. The profile endpoint (which needs a login) lists every limit: the
windows, a weekly limit per model, and how large each is as a share of the
weekly. An account whose list has no five-hour row has no five-hour limit,
and every surface says so. Profile answers are cached and backed off on 429.

Only limits on the startup model (read from `~/.claude/settings.json`) take
part in rotation; they are marked `*`. An exhausted Opus 4.8 limit does not
block Opus 5 sessions. Keep that setting aligned with the model you use:
session `--model` overrides do not change the daemon's selection model.
`tokeman rotate status` shows the selection model. Polling speeds up near
its quota floor, including model-specific limits such as Fable's.
When the startup model's limit is only in its own
responses' headers, accounts without profile reads get one probe with that
model every `target_model_sample_secs` (0 disables).

The SessionStart hook installed with the service records each new Claude
session, and the service tails its transcript for real rejections. A
rejection marks that model and account unavailable until the reported reset
and re-evaluates the default at once. `tokeman rotate quarantine` records a
rejection tokeman did not see itself.

### Usage-limit resets

Anthropic offers some accounts resets that refill their limits on request.
`tokeman claude reset` opens a picker of available resets; add `<name>` to
limit it to one account. Use arrow keys to browse the offers and their effects,
Enter to review and confirm, or Esc to cancel. `--list` prints offers without
opening the picker; bare `reset` also lists when piped. `--grant <id>` or
`--session` selects directly, and `--yes` skips confirmation. A grant spends
one of the account's limited resets; a session reset refills the 5-hour
allowance while usage still counts toward the weekly limit. Weekly reset dates
stay unchanged. Nothing uses one on its own. A claim that got no
definite answer is kept in `claims.json` and is only ever resent as the same
claim; `--clear` forgets it once claude.ai shows what happened.

## Codex

Codex resolves its credentials per request from `$CODEX_HOME`, so several
Codex processes can hold different accounts at once and a running one can be
moved between accounts without a restart:

```sh
tokeman codex add <name> <codex-home>   # register a profile directory
tokeman codex discover [--adopt]        # find profiles on disk
tokeman codex import <file>             # seed profiles from `<name> <access-token>` lines
tokeman codex remove <name>
tokeman codex list                      # read every profile now, with its reset credits
tokeman codex refresh                   # renew OAuth tokens before they rot into a 401
tokeman codex best [--limit spark]      # the CODEX_HOME with the most headroom
tokeman codex use [<account>]           # move a running Codex onto an account (default: the best)
tokeman codex use <account> --into <dir>   # copy its credentials into another profile instead
tokeman codex watch [--dry-run]         # follow a running Codex and move it in time
tokeman codex launch [--limit spark] [-- codex args]   # start codex on the profile with the most headroom
tokeman codex reset <account> [--credit <id>] [--yes]   # spend one reset credit at a limit
```

Windows are named by their length, not by which field the backend put them
in. `reset` refuses unless the account is at a limit and the backend counts a
credit that applies now; its request id is kept in `claims.json` until the
backend answers, so re-running after a lost answer cannot spend a second
credit (`--clear` forgets the claim). A migration is announced to the running
Codex over its app-server socket. The service probes and refreshes every
registered profile on a half-hour sweep.

The service also moves a running Codex by itself. Every five minutes it asks
each control socket it finds (`~/.codex` and every registered profile) for
the account-wide windows (tokeman sends no request for this; Codex fetches
its own usage to answer). A Codex with less than 20%
left in one (`codex_min_5h_remaining`, `codex_min_7d_remaining` in
`[rotation]`) is moved between turns onto the best other profile with at
least 30% left (`codex_land_min_5h_remaining`, `codex_land_min_7d_remaining`).
With nowhere to land it stays where it is; a Codex the backend refuses
outright takes any profile with room. Parked profiles are never landed on,
and no reset credit is spent. This needs two unparked profiles and a control
socket, stops while rotation is paused, and is turned off with
`tokeman settings set codex_auto_move false`. Each move is a line in the
rotation log and in `codex-moves.jsonl`. `tokeman codex watch` does the same
in the foreground on every update Codex pushes; `--dry-run` says what the
service would do now and moves nothing.

A Codex that was moved (by the service, `codex use` or `codex watch`) runs on
a token it was handed and cannot renew: Codex keeps it in memory, never
refreshes it ahead of time, and when the backend rejects it asks its
connected clients for another for ten seconds before failing the turn. The
service is the one that renews it. Every push is recorded in
`codex-seats.json`; on each look the service renews a seat whose token has
less than a day of its ten left, through the profile's own refresh token, and
pushes the new one. It does this whether or not `codex_auto_move` is on, and
forgets a seat whose Codex exited or logged in as something else. When it
cannot (the profile's login was refused, the socket does not answer, rotation
is paused, no service runs), that is an attention item with the time the
token stops working and the command that fixes it, shown in `tokeman`,
`tokeman list` and the tray and sent as a notification. Without a running
service, re-run `tokeman codex use <account> --codex-home <dir>` before that
time.

## Antigravity

```sh
tokeman agy import                 # register the accounts agy and the Antigravity app are signed in to
tokeman agy remove <name>
tokeman agy list                   # read every account now
tokeman agy best [--group claude]  # rank them by room left (on one model group, or on all)
tokeman agy use <account>          # sign the agy CLI in as that account (macOS)
tokeman agy use --previous         # put back the login the last `use` replaced
tokeman agy run <account> [-- …]   # start agy as that account beside your normal login (macOS)
tokeman agy launch [--group claude] [-- …]   # `agy run` on the account with the most room
```

Antigravity meters each model group apart, each with a weekly and a five-hour
window. The service reads the accounts; it never switches agy. `agy`, the
Antigravity app and an editor's `agy acp` server share one login in the
Keychain, and `agy use` looks at what is running before it writes:

- The Antigravity app is told to re-read the login and switches live.
- An `agy acp` server cannot be told from outside; if it holds the old
  account, restart the agent from the editor, or it writes the old account
  back when it next refreshes its token.
- An interactive `agy` cannot be told either, so the switch is refused while
  one runs, with each process's PID, directory and the command that resumes
  its conversation. Quit it, run the command again, resume. `--force`
  switches anyway; it lasts until that process refreshes, within the hour.

`tokeman agy run <account>` avoids switching: it starts agy with a login of
its own, so two accounts can work side by side. The session gets a home under
`~/.local/share/tokeman/agy-homes/<account>` that links to everything in your
real home except `.gemini` and `Library`, a token file written from tokeman's
grant, and `SSH_TTY`, which makes agy keep its login in that file and leave
the shared Keychain item alone. It has its own conversations (`agy -c` there
resumes that account's last one) and shares your `GEMINI.md`, skills, CLI
settings and MCP config. Commands agy runs in it cannot reach the macOS
keychain (git's https credential helper, for example). The account agy
already holds runs plain `agy`. `agy list` shows these sessions, and `agy use`
does not count them as a reason to refuse.

`--dry-run` says what is running and what would happen, and writes nothing.

## GLM and Kimi

```sh
tokeman glm add <name> --key-file ~/.zai-key     # z.ai GLM Coding Plan
tokeman kimi add <name> --key-file ~/.kimi-key   # Kimi Code
tokeman glm|kimi remove <name>
tokeman glm|kimi list                            # read now
tokeman glm|kimi best [--group <text>]           # rank by room left
tokeman glm|kimi launch [--model <id>] [-- claude args]   # Claude Code on the plan
```

These are read, not rotated. The key file is read on every poll, so the key
never enters tokens.toml.

`launch` starts Claude Code on the plan's account with the most room, through
the vendor's Anthropic-compatible endpoint (`https://api.z.ai/api/anthropic`,
`https://api.kimi.com/coding/`). The endpoint, the key and the model for each
of Claude Code's tiers (`glm-5.3` with `glm-5.3-flash` for background work;
`kimi-for-coding`; `--model` names another for every tier) are set in that
one claude's environment. `~/.claude/settings.json` is not written, so every
other session keeps its Anthropic account, and the launched one does not
send it: Claude Code prefers the key in its environment over the settings
token. A model set in the `env` block of settings.json would override the
vendor's.

## The background service

```sh
tokeman rotate install           # install and start it (macOS LaunchAgent)
tokeman rotate uninstall
tokeman rotate status            # read Claude now: the default, live sessions, every account's room
tokeman rotate run [--dry-run]   # one cycle now; --dry-run writes nothing and says what it would do
tokeman rotate pause             # stop managing; /login gets its precedence back
tokeman rotate resume            # manage again and run one cycle
tokeman rotate quarantine <name> --model fable --window weekly --until 2026-12-01T09:00:00-04:00
tokeman rotate clear-quarantine <name>
```

One process (`tokeman rotate daemon`) does all the reading: Claude on the
rotation's cadence, the Antigravity, GLM and Kimi accounts every
`services_interval_secs` (default 300, in `[settings]`), Codex on a half-hour
sweep that also refreshes tokens. It renews Claude logins before they expire
and keeps every read in the history. It exits when its binary is replaced, so
launchd starts the new one.

Every two minutes the service looks at what needs you and sends each new item
as a desktop notification titled "tokeman": through `osascript` on macOS
(where it appears under Script Editor's name and needs that allowed once),
through `notify-send` elsewhere when it is installed. An item is sent once,
again when it reaches its nearer threshold, and again if it comes back after
six hours away; `notified.json` remembers what was sent. `tokeman notify
--test` sends one now. `tokeman settings set notifications false` keeps the
items to the rotation log.

`tokeman claude launch` runs one rotation cycle, then starts claude with the
configured arguments and without a token in its environment or command line.
`--auto` keeps rotating on the fast cadence while claude runs.

## When a service changes

```sh
tokeman doctor [--json]
```

Every parser keeps the names it knows: the rate-limit headers, the keys of
each usage body, and the words of fields such as a limit's `kind`. Anything
else a service sends is kept with the read as an unrecognised item
(`usage.limits[].kind=daily_all`), by name only. The background service logs
each item the first time it sees it (`drift.json` holds the list).

`doctor` reads every account once and reports, per account, whether the
service answered, the state of the credential and the unrecognised items
(also those of the reset programs and the Codex credit list, and what the
service has logged). It then states the installed Claude Code, codex, agy
and Antigravity app against the versions this build was verified with
(2.1.322, 0.167.0, 1.5.6, 2.31.2), the SessionStart hook, whether the
background service runs, the modes of the config directory and tokens.toml,
whether agy's Keychain login reads, and reset claims without an answer. It
exits 0 when nothing is wrong and 1 otherwise, and every problem comes with
the command that fixes it. A tool at another version is stated, not counted
as a problem.

## Tray

The macOS menu-bar app is a separate Swift package:

```sh
cd macos/tray && bash build.sh && open Tokeman.app
```

It draws the fleet view: a card per account of every service with a gauge
per limit, each login's end date, the resets on offer, and whether the view
is stored and how old it is. It launches claude in a new terminal on a chosen
or the best account and exposes the launch settings and the dangerous-mode
toggle.

It runs `tokeman list --json`, `tokeman history --json` and
`tokeman settings show`, none of which sends a request; it never opens
tokens.toml. Its reload button, and its timer when no service is running, run
`tokeman --fresh --json`. Its settings go through `tokeman settings set` and
its Launch buttons through `tokeman claude use` (one account) and
`tokeman claude launch` (the best one).
`Tokeman.app/Contents/MacOS/TokemanTray --render out.png` draws the popover
once into a PNG. The app opens with what needs attention, carries the
forecasts in its header, and shows each limit under the name and note the
fleet view gives it.

## Files

| Path | Contents |
|------|----------|
| `~/.config/tokeman/tokens.toml` | Accounts, setup keys, login grants, settings, Codex profiles, service accounts (mode 0600) |
| `~/.config/tokeman/claude-credential-history.jsonl` | Which credential settings offered when (fingerprints only) |
| `~/.config/tokeman/claude-admission-state.json` | Session bindings and reset-bounded rejection quarantines |
| `~/.config/tokeman/claude-rotate-state.json` | Cadence, mode, backoffs, carried samples |
| `~/.config/tokeman/claude-rotate.log` | Rotation events (names and headroom only) |
| `~/.config/tokeman/claims.json` | Reset claims (Claude grants, Codex credits) sent without a definite answer |
| `~/.config/tokeman/drift.json` | Header names, body keys and enum words the services sent that this version does not know |
| `~/.config/tokeman/notified.json` | Which attention items the service has sent as notifications |
| `~/.config/tokeman/claude-login-keychain-backup.json` | The `/login` Keychain record set aside while managed |
| `~/.config/tokeman/claude-profile-reads.json` | Cached profile-endpoint answers and their backoff |
| `~/.config/tokeman/antigravity-access-tokens.json` | Access tokens minted from the Antigravity grants |
| `~/.config/tokeman/agy-login-backup.json` | The agy login the last `agy use` replaced |
| `~/.config/tokeman/codex-seats.json` | The running Codexes holding a token tokeman pushed: which account, the token's fingerprint and expiry, the last failed renewal |
| `~/.config/tokeman/codex-moves.jsonl` | Each move of a running Codex: when, which home, from and to which account, who asked |
| `~/.local/share/tokeman/agy-homes/<account>/` | The home of a `tokeman agy run` session: its login file and conversations |
| `~/.local/share/tokeman/snapshots.db` | Read history (sqlite): one row per reading that differs from the last, or per ten minutes |
| `~/.local/share/tokeman/snapshots.db.pre-0.4` | The history as 0.3 left it, kept by `tokeman history compact`; delete it when you no longer want it |

`XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME` are honoured, and
`CLAUDE_SETTINGS` points at another settings.json. Every file is owner-only;
no token value is ever logged or printed. Every write to tokens.toml is a
locked read-modify-write that keeps your comments, because refresh tokens are
single-use and a save from a stale copy would lose the account.

## Development

The gates CI runs, plus the tray:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                      # unit tests, all in-process (loopback servers, temp dirs)
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
macos/tray/build.sh                      # the Swift tray
scripts/check-linux.sh                   # the Linux job's lint and test build, from a Mac (needs zig)
```

While working, run the tests of the module you touched
(`cargo test --lib store::`). To try commands without touching your own
state, point `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` and
`CLAUDE_SETTINGS` at copies.

The library (`src/lib.rs`) has no UI dependency. `policy.rs` is pure and its
tests are the specification of when the default moves. `commands/` returns
data, and every line printed is in `ui/text.rs`.

## Releases

Releases are built by [cargo-dist](https://opensource.axo.dev/cargo-dist/) and
published to GitHub Releases for macOS (ARM64, x86_64) and Linux (ARM64,
x86_64). Bump `version` in `Cargo.toml`, add the section to `CHANGELOG.md`
(the release notes come from it), then:

```sh
git tag v0.4.2
git push origin v0.4.2
```

## License

MIT

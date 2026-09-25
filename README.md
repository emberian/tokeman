# tokeman

Your buddy for managing Anthropic OAuth tokens. Visualizes remaining usage,
launches Claude Code with the best available token, and maintains a safe OAuth
default for newly started sessions before a quota window hits the wall.

```
 tokeman — 4/4 tokens probed

 ember@elide
   Session (5h) ████████████████████████████████████████ 100% left  resets 4h29m (4:00PM)
   Weekly  (7d) ████████████████████████████████████████ 100% left  resets Tue 12:00PM
   Status: allowed  (limit: session)

 pug
   Session (5h) ████████████████████████████████████████ 100% left  resets 4h29m (4:00PM)
   Weekly  (7d) ██████████████████████████░░░░░░░░░░░░░░  65% left  resets Fri 9:00AM
   Status: allowed  (limit: session)

 cmrx64
   Session (5h) ██████████████████████████████████░░░░░░  85% left  resets 2h29m (2:00PM)
   Weekly  (7d) ██████████████████████████████░░░░░░░░░░  74% left  resets Fri 12:00PM
   Extra usage  ████████████████████████████████████████ 100% left  resets Tue 8:00PM
   Status: allowed  (limit: session)
```

## Install

From source:

```sh
./scripts/install.sh
```

On macOS, use the installer rather than a bare `cargo install`: it repairs the
installed Mach-O's ad-hoc signature after Cargo copies it, then installs the
rotation LaunchAgent. The LaunchAgent also starts through a signature-repairing
shim, so replacing the binary cannot strand the background monitor. Set
`TOKEMAN_CODESIGN_IDENTITY` explicitly to use a stable signing identity; Tokeman
does not auto-select one because `codesign` may open a blocking Keychain dialog.

From GitHub releases (prebuilt binaries for macOS and Linux):

```sh
# See https://github.com/emberian/tokeman/releases
```

## Setup

Log each account in through the browser:

```sh
tokeman login "my-account"       # one account (added if it is new)
tokeman login --more             # every configured account that still needs it
tokeman refresh [<name>]         # renew grants now (the daemon does this before expiry)
```

Each round prints a claude.com URL; sign in as that account and paste the code
the page shows. This grants the same full scope set Claude Code's own `/login`
does, including `user:profile`, with a refresh token the rotation service
renews in the background.

You can also add long-lived `claude setup-token` keys:

```sh
tokeman add "work" "sk-ant-oat01-..."
```

A setup token is **inference-only**. A Claude session running on one cannot
send `/feedback`, use Remote Control, or read profile usage, and every quota
probe has to spend a Haiku request. When an account has both, the login is
used and the setup token is kept as a fallback in case the login is refused.

Credentials are stored in `~/.config/tokeman/tokens.toml` with mode `0600`.
Every write to that file is a locked read-modify-write, because refresh tokens
are single-use and a save from a stale copy would lose the account.

## Usage

```sh
tokeman                          # probe all tokens, print gauges, save snapshot
tokeman --json                   # output probe results as JSON
tokeman --watch                  # live TUI dashboard (auto-refreshes every 30s)
tokeman launch [-- claude args]  # launch claude with the best token
tokeman launch --auto [-- args]  # monitor the startup default
tokeman rotate install           # install the macOS background monitor
tokeman rotate status            # show startup default, live sessions, and headroom
tokeman rotate run --dry-run     # preview a policy decision
tokeman rotate pause             # restore /login credential precedence
tokeman rotate resume            # resume and select a viable token
tokeman rotate quarantine <name> --model opus --window weekly \
  --until 2026-08-01T09:00:00-04:00
tokeman rotate clear-quarantine <name>
tokeman chart                     # inline history chart in iTerm2
tokeman chart --metric seven-day # chart another quota window
tokeman chart --metric fable      # chart any per-model weekly bucket by name
tokeman chart --metric "Opus 4.8"
tokeman usage capture <name>       # capture current /login profile usage access
tokeman browse                    # local interactive history/session dashboard
tokeman login [<name>|--more]    # browser login: full scope, refreshable
tokeman list                     # show configured tokens and their login state
tokeman add <name> <key>         # add a token
tokeman remove <name>            # remove a token
tokeman history [--last N]       # show recent snapshots
tokeman stats                    # burn rates and usage statistics
```

### Startup-default rotation

`tokeman rotate install` installs a per-user macOS LaunchAgent. It writes the
default account's credential to `~/.claude/settings.json` under `.env`:

| Key | Value |
|-----|-------|
| `CLAUDE_CODE_OAUTH_TOKEN` | the account's login access token, or its setup token |
| `CLAUDE_CODE_OAUTH_SCOPES` | the login's granted scopes (omitted for setup tokens) |
| `CLAUDE_CODE_OAUTH_401_WAIT_MS` | `120000` |
| `TOKEMAN_ACCOUNT` | the account name |

Claude assumes an env token is inference-only unless `CLAUDE_CODE_OAUTH_SCOPES`
says otherwise, so the scope list is what turns `/feedback` and Remote Control
back on.

How running Claude processes see these values matters for everything below.
Claude copies settings `env` into its process environment whenever the file
changes, but its API client keeps the token it cached at startup until the API
refuses it. Only then does it re-read the environment. So:

- A process on a **setup token** keeps its launch account for its whole life.
  Rotating the default only moves new or restarted processes.
- A process on a **login token** keeps its account until that token expires
  (8 hours) or is refreshed: a refresh revokes the access token it replaces.
  Its next request is refused, it waits (up to the 401 wait above) for settings
  to offer a live token, and continues on whatever the default is by then. No
  restart is needed, and an account rotated away from stops being drained by
  its next refresh at the latest.

The daemon refreshes each login 30 minutes before expiry and rewrites settings
when the default's token changes, so that retry is normally immediate. If a
refresh is refused for good (revoked grant), the account falls back to its setup
token and the log says to run `tokeman login` again.

Tokeman deliberately does **not** use `apiKeyHelper`: Claude treats that hook as
an external API-key credential, while tokeman stores Claude.ai OAuth bearer
tokens.

Default selection is sticky: a healthy account remains the startup default so
the fleet preserves prompt-cache locality. When a floor is genuinely crossed,
tokeman chooses the viable account with the largest balanced headroom so new or
restarted sessions can absorb cold prompt-cache creation. If two candidates
offer the same landing room, the limiting window that replenishes soonest wins.

A `/login` record left in the Keychain would undermine that recovery: when
Claude's token is refused and the stored login has a refresh token, Claude
refreshes the Keychain login instead of waiting for settings to offer a new
env token. So during explicit foreground enrollment
(`tokeman add`, `tokeman usage capture`) and installation, Tokeman privately
backs up that Claude login field and suppresses it while preserving MCP and
unknown Keychain fields. `rotate pause` and `rotate uninstall` restore the saved
login unless a newer manual `/login` has replaced it. Ordinary daemon cycles
never access Keychain, so an unattended swarm cannot deadlock on a permission
dialog. After a later manual `/login`, finish enrollment with `tokeman add` or
`tokeman usage capture` before leaving the managed fleet unattended.

Every credential tokeman installs is recorded, by fingerprint only, in a
credential history. Replaying it from a process's start time through each token
expiry gives the account that process is actually spending, which is what
session listings, drain warnings and admission feedback use. (Neither the
`TOKEMAN_ACCOUNT` a hook sees nor the token in `ps eww` can answer this: both
show what settings offer, not what the process has cached.) Installation also
adds a small `SessionStart` hook that records session id, process id and
transcript path, so rejections in a transcript can be attributed. The
dashboard's `D` marker means “default for new processes.”

The default policy has two stages:

1. **Normal:** probe every 120 seconds and rotate at 10% five-hour remaining or
   10% seven-day remaining.
2. **Sip-and-drain:** when no token remains above both normal floors, probe
   every 20 seconds and drain the reserve pool to 2% five-hour / 3% seven-day.

That second stage prevents seven accounts with 9% five-hour capacity apiece
from stranding roughly 63% of a full window between them. Exact thresholds and
cadences are configurable:

```toml
[rotation]
normal_min_5h_remaining = 0.10
normal_min_7d_remaining = 0.10
sip_min_5h_remaining = 0.02
sip_min_7d_remaining = 0.03
normal_probe_interval_secs = 120
sip_probe_interval_secs = 20
```

The service wakes every 20 seconds, but skips the network probe until the
current mode's cadence is due. Rotation is serialized with a file lock, settings
writes are atomic, and token values are never written to logs. A partial probe
batch is never used to select a replacement: tokeman keeps the current default
and retries at the fast cadence. An account whose credential is refused
(revoked, expired) is rechecked on a backoff of 5 to 30 minutes instead of every
cycle, and the log groups unavailable accounts by cause rather than repeating
each response body. The log is moved aside to `claude-rotate.log.old` past 4 MB.

Quota snapshots use a one-token Haiku 4.5 request. They are a cheap shared
capacity signal, not proof that an unusually large Opus/1M turn will be
admitted. The profile endpoint can also report arbitrary model-scoped weekly
allowances, such as separate Opus 4.8 and Opus 5 buckets. Tokeman preserves the
provider's model identity instead of collapsing those into one family-wide
number, and renders every reported bucket in the CLI, TUI cards, history, and
browser dashboard.

Anthropic does not always expose those model buckets to setup tokens. Tokeman
therefore also tails only bytes appended after each exact `SessionStart`
binding. If Claude reports a real session/weekly rejection, tokeman records that
model/account as unavailable until Claude's reported reset and immediately
reevaluates the startup default. Concrete model failures such as
`claude-opus-4-8` remain separate and do not poison Opus 5. Rotation reads the
startup model from `~/.claude/settings.json`; only matching model buckets
participate in viability and replacement scoring, while the general 5h/7d
windows always apply. A genuinely family-wide bucket still applies to that
family. A manual `rotate quarantine` command covers failures from sessions that
predate the observer. `[1m]` can still require a much larger cold-cache
admission than a cheap probe.

### Model-specific weekly buckets

Besides the general 5-hour and weekly windows, an account can have weekly
limits that apply to one model: for example a "Fable limit" (the share of the
weekly allowance Fable models may use before they need usage credits), or
separate Opus 4.8 and Opus 5 buckets. Tokeman reads them from two places:

- **The profile-usage endpoint** (`/api/oauth/usage`, needs a `tokeman login`
  credential). Its `limits` list holds one `weekly_scoped` row per model,
  named by the model's display name. Every such row is shown, including idle
  ones and ones with no reset yet; the server's `is_active` flag only marks
  its headline row and is not a filter.
- **The `7d_oi` rate-limit headers** ("seven day, overage included"), which
  carry the Fable limit on any credential, setup tokens included. They are
  recorded as a "Fable" bucket unless the profile endpoint already reported
  one.

Headers only describe the model a request used, so the cheap Haiku probe never
sees the Fable limit. When the startup model is a Fable model, tokeman
subsamples: each account without profile reads gets one probe with that model
every `target_model_sample_secs` (default 1800), and every Haiku probe in
between carries the last reading forward until its window resets. `tokeman
rotate status` shows when each account was last sampled. Set the interval to 0
to never spend a Fable request on probing:

```toml
[rotation]
target_model_sample_secs = 1800
```

A bucket's identity is a canonical key (`opus48`, `opus5`, `fable`) shared by
profile rows and observed rejections, so `claude-opus-4-8` in a rejection and
"Opus 4.8" in a profile row are the same bucket. Only buckets matching the
startup model participate in rotation: an exhausted Opus 4.8 bucket does not
block Opus 5 sessions, and Fable is its own family.

Tokeman displays missing model data as unknown and never substitutes the
general 7-day value. An observed Claude rejection is rendered as a separate,
labeled signal (`!`), not as profile telemetry.

`tokeman usage capture <name>` is the older way to get profile reads: it
copies the access token of the account currently `/login`ed in Claude Code
(reading the Keychain, which may prompt). It cannot be refreshed and expires
within hours; prefer `tokeman login`.

### Launch mode

`tokeman launch` probes all your tokens, applies the startup OAuth default, and
launches Claude Code without exposing a token in its command line:

```
$ tokeman launch -- --model opus

 tokeman: probing 4 tokens...
   ember@elide      5h: 100% left  7d: 100% left  [allowed]
   pug              5h: 100% left  7d:  65% left  [allowed]
   cmrx64           5h:  85% left  7d:  74% left  [allowed]
   ember@lunar      5h:  85% left  7d:  84% left  [allowed]

 tokeman: switch /login -> ember@elide; no managed token was active; new 5h 100% left, 7d 100% left
 tokeman: launching: claude --model opus
```

With `--auto`, tokeman runs the adaptive default monitor alongside Claude. The
launched process is guaranteed the selected credential at startup; restart or
resume after a later default change to guarantee adoption.
A globally installed background service performs the same default monitoring
without requiring `tokeman launch`.

Set `TOKEMAN_CLAUDE_BIN` to override the claude binary path.

### Charts

`tokeman chart` produces a native PNG and displays it inline when the terminal
is iTerm2. It shows remaining capacity, breaks lines across quota resets and
missing probe gaps, overlays the normal/sip floors, and includes the latest
headroom for each account. The PNG is also saved under
`~/.cache/tokeman/chart.png` unless `--output` selects another path.

```sh
tokeman chart --hours 24 --metric five-hour
tokeman chart --hours 168 --metric seven-day --output weekly.png
tokeman chart --hours 168 --metric fable --output fable.png
```

`--metric` accepts `five-hour`, `seven-day`, `overage`, or a model name in any
spelling (`fable`, `opus-5`, `"Opus 4.8"`). An unknown model lists the buckets
that do have history.

### Tray app

A menu bar / system tray app for at-a-glance token status and one-click launches.

**macOS** — native SwiftUI app using `MenuBarExtra`, built separately:

```sh
cd macos/tray
bash build.sh
open Tokeman.app
```

Lives in the menu bar with a bolt icon colored by token health. Click to see all
tokens with gauges, launch buttons, danger mode toggle, and a settings panel.
Calls `tokeman --json` under the hood.

**Linux** — egui-based tray app, built with the `tray` feature:

```sh
cargo install --path . --features tray
tokeman tray
```

Same functionality: system tray icon, dark-themed window with token gauges,
launch buttons, settings, and dangerous mode toggle. Uses `tray-icon` for the
system tray and `eframe` for the UI. Supports terminal launching via
`gnome-terminal`, `konsole`, `alacritty`, `kitty`, `wezterm`, and others.

**Common features:**

- Token gauges with colored progress bars (green/amber/red)
- Click-to-launch: opens a terminal with Claude Code using that token
- Launch Best: one-click launch with the best available token
- Dangerous mode toggle for `--dangerously-skip-permissions`
- Settings panel: launch args, terminal, claude binary, probe interval
- Auto-refresh on a configurable interval
- Tray icon color reflects overall token health

Configure launch defaults in `~/.config/tokeman/tokens.toml`:

```toml
[settings]
launch_args = ["--model", "opus"]
dangerous_mode = false
terminal = "iTerm2"
probe_interval_secs = 30
```

Tray launches first update Claude's shared OAuth setting, then open a terminal
without placing credentials in AppleScript, process arguments, or scrollback.
iTerm2 launches also get a `tokeman · managed` session badge.

### One-shot mode

Run `tokeman` with no arguments. Probes every configured token concurrently,
displays colored gauge bars, and saves a snapshot to the local database.

- Green: >50% remaining
- Yellow: 20-50% remaining
- Red: <20% remaining

Use `--json` for machine-readable output (useful for scripting or the tray app).

### Watch mode

`tokeman --watch` launches a live terminal dashboard. Navigate with `j`/`k`,
force refresh with `r`, use `c` for a split chart or `C` for the full-screen
chart, change chart windows with `Tab`, change history range with `[`/`]`, and
quit with `q`. The chart marks the startup default with `D` and draws both
normal and sip-and-drain floors for the general 5h/7d policy windows. `Tab`
cycles through 5h, general 7d, every discovered model bucket, and overage.
Model-specific
history combines profile usage with explicitly labeled, authoritative Claude
rejection feedback; it still does not claim to predict a large cold request
before the service has either profile data or a real observation. The TUI uses
`settings.probe_interval_secs` and always restores the terminal if drawing or
probing fails. Token cards, the `c`/`C` charts, and the browser dashboard all
render arbitrary model-scoped buckets.

### Stats

After collecting multiple snapshots (run `tokeman` periodically, or use `--watch`),
`tokeman stats` computes:

- **Burn rate**: how fast you're consuming quota (utilization/hr)
- **Mean / stddev**: average and variance over the last 24h
- **Peak**: maximum observed burn rate
- **Time to depletion**: estimated hours until you hit the limit

## How it works

For each token, tokeman sends a minimal API request (1 token to Haiku) and reads
the `anthropic-ratelimit-unified-*` response headers. These headers report:

| Window | What it tracks |
|--------|---------------|
| `5h` | Rolling 5-hour session quota |
| `7d` | General weekly quota reported by the Haiku probe |
| `7d_oi` | The Fable weekly limit, when the response carries it |
| `<model> 7d` | Per-model weekly buckets from the profile-usage endpoint |
| `overage` | Extra usage / pay-as-you-go credits |

`representative-claim` names the window currently deciding admission:
`five_hour`, `seven_day`, `seven_day_overage_included` (Fable),
`seven_day_opus`, `seven_day_sonnet`, or `overage`. Neither the headers nor the
profile endpoint is publicly documented; tokeman follows how Claude Code's own
client reads them, and requests identify as the installed Claude Code version.

OAuth tokens require the `anthropic-beta: oauth-2025-04-20` header.

Each probe costs a fraction of a cent (one Haiku token).

## Files

| Path | Contents |
|------|----------|
| `~/.config/tokeman/tokens.toml` | Accounts, setup tokens, login grants, and settings |
| `~/.config/tokeman/tokens.lock` | Serializes every write to `tokens.toml` |
| `~/.config/tokeman/claude-credential-history.jsonl` | Which credential settings offered when (fingerprints and expiries only) |
| `~/.config/tokeman/claude-rotate-state.json` | Adaptive monitor cadence/mode |
| `~/.config/tokeman/claude-rotate.log` | Rotation events (names/headroom only) |
| `~/.config/tokeman/claude-login-keychain-backup.json` | Private `/login` backup and managed-mode suppression marker |
| `~/.config/tokeman/claude-admission-state.json` | Session bindings and reset-bounded rejection quarantines |
| `~/.local/share/tokeman/snapshots.db` | SQLite database of historical snapshots |

Both paths respect `XDG_CONFIG_HOME` and `XDG_DATA_HOME`.

## Releases

Releases are built with [cargo-dist](https://opensource.axo.dev/cargo-dist/) and
published to GitHub Releases with prebuilt binaries for:

- macOS (ARM64, x86_64)
- Linux (ARM64, x86_64)

To cut a release:

```sh
git tag v0.1.0
git push origin v0.1.0
```

## Roadmap

- [x] System tray mode with native macOS SwiftUI and egui Linux support
- [x] JSON output (`--json`) for scripting
- [x] Sticky startup-default selection with adaptive sip-and-drain
- [x] Live Claude process inventory with explicit pinned/default distinction
- [x] iTerm2 inline history charts
- [x] Local D3 history/session dashboard
- [x] GitHub releases via cargo-dist
- [ ] Notifications when weekly quota drops below threshold
- [x] Browser login with background refresh (full-scope, refreshable credentials)

## License

MIT

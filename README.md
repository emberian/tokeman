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

Add your OAuth tokens (the `sk-ant-oat01-...` keys from Claude Code / Claude Max):

```sh
tokeman add "my-account" "sk-ant-oat01-..."
tokeman add "work" "sk-ant-oat01-..."
```

Tokens are stored in `~/.config/tokeman/tokens.toml` with mode `0600`.

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
tokeman chart --metric opus-weekly # chart the separate Opus weekly bucket
tokeman chart --metric sonnet-weekly
tokeman usage capture <name>       # capture current /login profile usage access
tokeman browse                    # local interactive history/session dashboard
tokeman list                     # show configured tokens
tokeman add <name> <key>         # add a token
tokeman remove <name>            # remove a token
tokeman history [--last N]       # show recent snapshots
tokeman stats                    # burn rates and usage statistics
```

### Startup-default rotation

`tokeman rotate install` installs a per-user macOS LaunchAgent. It updates
`~/.claude/settings.json` at `.env.CLAUDE_CODE_OAUTH_TOKEN`. New Claude Code
processes load that default at startup. Existing processes may notice settings
changes, but Tokeman does not rely on auth hot-reload: restart/resume is the
guaranteed credential boundary. Tokeman deliberately does **not** use
`apiKeyHelper`: Claude treats that hook as an external API-key credential, while
tokeman stores Claude.ai OAuth bearer tokens.

Default selection is sticky: a healthy account remains the startup default so
the fleet preserves prompt-cache locality. When a floor is genuinely crossed,
tokeman chooses the viable account with the largest balanced headroom so new or
restarted sessions can absorb cold prompt-cache creation. If two candidates
offer the same landing room, the limiting window that replenishes soonest wins.

Interactive Claude gives the short-lived OAuth record created by `/login`
precedence over this managed default. During explicit foreground enrollment
(`tokeman add`, `tokeman usage capture`) and installation, Tokeman privately
backs up that Claude login field and suppresses it while preserving MCP and
unknown Keychain fields. `rotate pause` and `rotate uninstall` restore the saved
login unless a newer manual `/login` has replaced it. Ordinary daemon cycles
never access Keychain, so an unattended swarm cannot deadlock on a permission
dialog. After a later manual `/login`, finish enrollment with `tokeman add` or
`tokeman usage capture` before leaving the managed fleet unattended.

Installation also adds a small `SessionStart` hook that receives only a
non-secret account name, session id, process id, and transcript path. That
creates an exact launch-account binding for new/resumed Claude processes; older
processes remain explicitly marked as startup estimates. The dashboard's `D`
marker means “default for new processes,” not a claim that every running process
has reloaded it.

The default policy has two stages:

1. **Normal:** probe every 120 seconds and rotate at 10% five-hour remaining or
   5% seven-day remaining.
2. **Sip-and-drain:** when no token remains above both normal floors, probe
   every 20 seconds and drain the reserve pool to 2% five-hour / 3% seven-day.

That second stage prevents seven accounts with 9% five-hour capacity apiece
from stranding roughly 63% of a full window between them. Exact thresholds and
cadences are configurable:

```toml
[rotation]
normal_min_5h_remaining = 0.10
normal_min_7d_remaining = 0.05
sip_min_5h_remaining = 0.02
sip_min_7d_remaining = 0.03
normal_probe_interval_secs = 120
sip_probe_interval_secs = 20
```

The service wakes every 20 seconds, but skips the network probe until the
current mode's cadence is due. Rotation is serialized with a file lock, settings
writes are atomic, and token values are never written to logs. A partial probe
batch is never used to select a replacement: tokeman keeps the current default
and retries at the fast cadence.

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

The long-lived setup tokens used for message probes generally do not have the
`user:profile` scope needed by Claude's profile-usage endpoint. To attach the
currently logged-in Claude account's read-only usage view to a configured token:

```sh
# In Claude Code, /login to the account represented by this tokeman name.
tokeman usage capture ember@lunar.town
```

Capture reads the macOS Keychain and may cause one explicit prompt. It also
reconciles the `/login` record after capture so managed fleet auth remains
authoritative. The background rotator, session observer, `--watch`, tray,
browser, and normal probes never read Keychain. A captured profile access token
can expire; if the model-scoped rows change to
“unavailable,” repeat `/login` and the capture command for that account. Tokeman
displays missing model data as unknown and never substitutes the general 7-day
value. An observed Claude rejection is rendered as a separate, labeled signal,
not misrepresented as profile telemetry.

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
tokeman chart --hours 168 --metric opus-weekly --output opus-weekly.png
```

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
| `<model> 7d` | Dynamically discovered model-scoped quota from the profile-usage endpoint |
| `overage` | Extra usage / pay-as-you-go credits |

OAuth tokens require the `anthropic-beta: oauth-2025-04-20` header.

Each probe costs a fraction of a cent (one Haiku token).

## Files

| Path | Contents |
|------|----------|
| `~/.config/tokeman/tokens.toml` | Token names, keys, and settings |
| `~/.config/tokeman/claude-rotate-state.json` | Adaptive monitor cadence/mode |
| `~/.config/tokeman/claude-rotate.log` | Rotation events (names/headroom only) |
| `~/.config/tokeman/claude-login-keychain-backup.json` | Private `/login` backup and managed-mode suppression marker |
| `~/.config/tokeman/claude-admission-state.json` | Exact session bindings and reset-bounded rejection quarantines |
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
- [ ] Token refresh (auto-refresh expired OAuth tokens via refresh_token)

## License

MIT

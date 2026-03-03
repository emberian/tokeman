# tokeman

Your buddy for managing Anthropic OAuth tokens. Visualizes remaining usage,
launches Claude Code with the best available token, and automatically rotates
when you hit limits.

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

```sh
cargo install --path .
```

## Setup

Add your OAuth tokens (the `sk-ant-oat01-...` keys from Claude Code / Claude Max):

```sh
tokeman add "my-account" "sk-ant-oat01-..."
tokeman add "work" "sk-ant-oat01-..."
```

Tokens are stored in `~/.config/tokeman/tokens.toml`.

## Usage

```sh
tokeman                          # probe all tokens, print gauges, save snapshot
tokeman --watch                  # live TUI dashboard (auto-refreshes every 30s)
tokeman tray                     # system tray app with glass UI
tokeman launch [-- claude args]  # launch claude with the best token
tokeman launch --auto [-- args]  # auto-rotate tokens on exhaustion
tokeman list                     # show configured tokens
tokeman add <name> <key>         # add a token
tokeman remove <name>            # remove a token
tokeman history [--last N]       # show recent snapshots
tokeman stats                    # burn rates and usage statistics
```

### Launch mode

The headline feature. `tokeman launch` probes all your tokens, picks the one with
the most weekly headroom, and launches Claude Code with it:

```
$ tokeman launch -- --model opus

 tokeman: probing 4 tokens...
   ember@elide      5h: 100% left  7d: 100% left  [allowed]
   pug              5h: 100% left  7d:  65% left  [allowed]
   cmrx64           5h:  85% left  7d:  74% left  [allowed]
   ember@lunar      5h:  85% left  7d:  84% left  [allowed]

 tokeman: best token: ember@elide (100% weekly left)
 tokeman: launching: claude --model opus
```

When claude exits (rate limited, session expired, or you quit), tokeman re-probes
and offers the next best token:

```
 tokeman: claude exited with code 1
 tokeman: probing 4 tokens...
 tokeman: switching to ember@lunar (84% weekly left)
 tokeman: relaunch with ember@lunar? [Y/n]
```

With `--auto`, it relaunches immediately without prompting.

While claude is running, tokeman samples all your tokens every 5 minutes in the
background, building a usage time series in the snapshot database.

When all tokens are exhausted, tokeman shows you when the earliest one resets
and waits:

```
 tokeman: all tokens exhausted
 tokeman: earliest reset: ember@elide in 2h14m (4:00PM)
 tokeman: waiting... (ctrl-c to quit)
```

Set `TOKEMAN_CLAUDE_BIN` to override the claude binary path.

### Tray mode

`tokeman tray` runs as a system tray application with a rich GUI window. Features:

- **Glass UI** on macOS with native vibrancy/frosted glass effect
- **Token gauges** with colored progress bars (green/amber/red)
- **Click to launch**: each token has a Launch button that opens a terminal
  with Claude Code using that token
- **Launch Best**: one-click launch with the best available token
- **Dangerous mode toggle**: prominent switch for `--dangerously-skip-permissions`
- **Settings panel**: configure launch args, terminal preference, claude binary,
  and probe interval — all saved to the config file
- **Auto-refresh**: probes tokens in the background on a configurable interval
- **Tray icon**: colored dot reflects overall token health (green/amber/red/gray)

Configure launch defaults in `~/.config/tokeman/tokens.toml`:

```toml
[settings]
launch_args = ["--model", "opus"]
dangerous_mode = false
terminal = "iTerm2"
probe_interval_secs = 30
```

To build without tray support (CLI-only, smaller binary):

```sh
cargo install --path . --no-default-features
```

### One-shot mode

Run `tokeman` with no arguments. Probes every configured token concurrently,
displays colored gauge bars, and saves a snapshot to the local database.

- Green: >50% remaining
- Yellow: 20-50% remaining
- Red: <20% remaining

### Watch mode

`tokeman --watch` launches a live terminal dashboard. Navigate with `j`/`k`,
force refresh with `r`, quit with `q`.

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
| `7d` | Weekly quota (all models) |
| `overage` | Extra usage / pay-as-you-go credits |

OAuth tokens require the `anthropic-beta: oauth-2025-04-20` header.

Each probe costs a fraction of a cent (one Haiku token).

## Files

| Path | Contents |
|------|----------|
| `~/.config/tokeman/tokens.toml` | Token names and keys |
| `~/.local/share/tokeman/snapshots.db` | SQLite database of historical snapshots |

Both paths respect `XDG_CONFIG_HOME` and `XDG_DATA_HOME`.

## Roadmap

- [x] System tray mode (`tokeman tray`) with glass UI and colored indicator
- [ ] Notifications when weekly quota drops below threshold
- [ ] Token refresh (auto-refresh expired OAuth tokens via refresh_token)
- [ ] JSON output (`--json`) for scripting

## License

MIT

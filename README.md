# tokeman

tokeman keeps a fleet of Anthropic subscription accounts and makes sure the
one credential slot Claude Code reads always holds an account with room left.
It also keeps Codex (OpenAI) accounts alive and can move a running Codex
between them.

```
 tokeman — 4/4 accounts probed with Haiku 4.5

 ember@elide
   Session (5h) ████████████████████████████████████████ 100% left  resets 4h29m (4:00PM)
   Weekly  (7d) ████████████████████████████████████████ 100% left  resets Tue 12:00PM
   Fable (7d)   ██████████████████████████████░░░░░░░░░░  76% left  resets Fri 5:00PM  profile
   Status: allowed  (limit: session)
```

The design is written down in [DESIGN.md](DESIGN.md); the code follows it.

## Install

From source, on macOS:

```sh
./scripts/install.sh
```

The installer repairs the installed binary's ad-hoc signature after Cargo
copies it, then installs the rotation LaunchAgent. Set
`TOKEMAN_CODESIGN_IDENTITY` to sign with a stable identity. On Linux the
script builds and installs the binary; run `tokeman rotate daemon` under your
own init system.

Prebuilt binaries for macOS and Linux are on the
[releases page](https://github.com/emberian/tokeman/releases).

## Setup

Log each account in through the browser:

```sh
tokeman login "my-account"       # one account (added if it is new)
tokeman login --more             # every configured account that still needs it
tokeman refresh [<name>]         # renew grants now (the daemon does this before expiry)
tokeman list                     # accounts and what each one offers
```

Each round prints a claude.com URL; sign in as that account and paste the code
the page shows. This grants the scopes Claude Code's own `/login` grants,
including `user:profile`, with a refresh token the daemon renews in the
background.

A long-lived `claude setup-token` key also works, as a fallback or on its own:

```sh
tokeman add "work" "sk-ant-oat01-..."
```

A setup token is inference-only: a session running on one cannot send
`/feedback`, use Remote Control, or read profile usage, and every quota probe
has to spend a Haiku request. When an account has both, the login is used and
the setup token is kept for the day the login is refused.

Accounts live in `~/.config/tokeman/tokens.toml` (mode `0600`). Every write
to that file is a locked read-modify-write that keeps your comments, because
refresh tokens are single-use and a save from a stale copy would lose the
account.

## Usage

```sh
tokeman                          # probe every account, print gauges, save a snapshot
tokeman --json                   # the same as JSON
tokeman --watch                  # live TUI dashboard
tokeman launch [-- claude args]  # pick the default, then start claude
tokeman launch --auto [-- args]  # keep rotating while claude runs
tokeman rotate install           # install the background service (macOS)
tokeman rotate status            # the default, live sessions, every account's room
tokeman rotate run --dry-run     # what the policy would do now; writes nothing
tokeman rotate use <name>        # make an account the default now
tokeman rotate pause             # stop managing; /login gets its precedence back
tokeman rotate resume            # manage again and run one cycle
tokeman rotate quarantine <name> --model fable --window weekly --until 2026-08-01T09:00:00-04:00
tokeman rotate clear-quarantine <name>
tokeman history [--last N]       # recent snapshots
tokeman stats                    # burn rates and time to depletion
tokeman chart [--metric fable]   # PNG chart, inline in iTerm2
tokeman resets [use <name>]      # usage-limit resets on offer
tokeman settings [set <key> <value>]   # launch settings; the trays write through this
tokeman codex ...                # Codex accounts (below)
```

### How the default is chosen

The daemon writes the default account's credential into the `env` block of
`~/.claude/settings.json`:

| Key | Value |
|-----|-------|
| `CLAUDE_CODE_OAUTH_TOKEN` | the account's login access token, or its setup token |
| `CLAUDE_CODE_OAUTH_SCOPES` | the login's granted scopes (omitted for setup tokens) |
| `CLAUDE_CODE_OAUTH_401_WAIT_MS` | `120000` |
| `TOKEMAN_ACCOUNT` | the account name |

Claude Code re-reads that credential for every API client it builds, so every
live session spends the current default from its next request. There is no
per-session binding to manage, and a switch moves everyone at once, cold-
starting every session's prompt cache. The policy therefore switches rarely:

1. The default stays until one of its own windows reaches a floor: 7% of the
   five-hour window or 5% of the weekly window (a per-model weekly bucket for
   the startup model, such as the Fable limit, counts as a weekly window).
2. It leaves only for an account with real room: at least 25% five-hour and
   10% weekly remaining. Among those, the largest balanced headroom wins; on a
   tie, the one whose limiting window refills soonest.
3. With nowhere worth landing, it drains in place to 2% five-hour / 3% weekly,
   or until it is refused, and only then moves to the roomiest account above
   those floors.

Probing runs every 120 seconds, and every 20 seconds while the default is near
a floor or nothing is above the normal floors. A partial probe never selects a
replacement; an account whose credential is refused is rechecked on a backoff
of 5 to 30 minutes until its credential changes. Thresholds are configurable:

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

### What a probe sees

Each probe is a one-token Haiku request; the `anthropic-ratelimit-unified-*`
headers of the answer report the five-hour and weekly windows, the Fable limit
(`7d_oi`) and any overage credits. Per-model weekly buckets (separate Opus 4.8
and Opus 5 limits, the Fable limit) come from the profile endpoint, which
needs a login; its answers are cached for five minutes and backed off on 429.
Every bucket is shown, idle ones included.

A bucket's identity is a canonical key (`opus48`, `opus5`, `fable`) shared by
profile rows, headers and observed rejections. Only buckets that limit the
startup model (read from `~/.claude/settings.json`) take part in rotation: an
exhausted Opus 4.8 bucket does not block Opus 5 sessions, and Fable is its own
family. When the startup model is a Fable model, accounts without profile reads
get one probe with that model every `target_model_sample_secs` (0 disables),
and the reading is carried forward until the window resets.

The SessionStart hook installed with the service records each new Claude
session, and the daemon tails its transcript for real rejections. A rejection
marks that model and account unavailable until the reported reset and
re-evaluates the default at once. Sessions pointed at a third-party
`ANTHROPIC_BASE_URL`, Bedrock or Vertex are left alone; `TOKEMAN_SESSION_SPENDS`
overrides the heuristic. `rotate quarantine` records a rejection tokeman did
not see itself.

### Usage-limit resets

Anthropic offers some accounts resets that refill their limits on request:
grants (a number of resets, a use-by date, the windows they refill) and a
session reset paid from the weekly limit. `tokeman resets` lists what is on
offer; `tokeman resets use <name>` shows what a reset would clear and asks
before claiming. Resets are scarce and irreversible, so nothing uses one on
its own. Both need a login.

### Launch

`tokeman launch` runs one rotation cycle, then starts claude with the
configured arguments and without a token in its environment or command line
(it reads settings.json like every other session). `--auto` keeps rotating on
the fast cadence while claude runs. `TOKEMAN_CLAUDE_BIN` overrides the binary.

### Charts and the TUI

`tokeman chart` renders a PNG (saved under `~/.cache/tokeman/chart.png` unless
`--output` says otherwise) and shows it inline in iTerm2. `--metric` takes
`five-hour`, `seven-day`, `overage`, or a model name in any spelling (`fable`,
`opus-5`, `"Opus 4.8"`); an unknown model lists the buckets with history.

`tokeman --watch` is the live dashboard: `j`/`k` move, `r` refreshes, `c` and
`C` show the chart, `Tab` cycles the chart's window through 5h, 7d, every
model bucket and overage, `[`/`]` change the range, `q` quits.

### Trays

The macOS menu-bar app is a separate Swift package:

```sh
cd macos/tray && bash build.sh && open Tokeman.app
```

The Linux tray is the `tray` feature:

```sh
cargo install --path . --features tray
tokeman tray
```

Both show every account's gauges and buckets, launch claude in a new terminal
on a chosen or the best account, and expose the launch settings and the
dangerous-mode toggle. Every setting they change goes through `tokeman
settings set`, so the account file is never rewritten by a tray.

### Codex accounts

Codex resolves its credentials per request from `$CODEX_HOME`, so several
Codex processes can hold different accounts at once and a running one can be
moved between accounts without a restart:

```sh
tokeman codex add <name> <codex-home>   # register a profile directory
tokeman codex discover [--adopt]        # find profiles on disk
tokeman codex import <file>             # seed profiles from `<name> <access-token>` lines
tokeman codex list                      # probe every account's quota
tokeman codex refresh                   # renew OAuth tokens before they rot into a 401
tokeman codex best                      # the CODEX_HOME with the most headroom
tokeman codex seat <account> --into <dir>
tokeman codex reseat                    # move a running Codex onto another account
tokeman codex watch                     # follow a running Codex and move it in time
```

A migration is announced to the running Codex over its app-server socket,
because Codex's own 401 recovery refuses a swapped account. The daemon
refreshes every registered profile on a half-hour sweep.

## Files

| Path | Contents |
|------|----------|
| `~/.config/tokeman/tokens.toml` | Accounts, setup keys, login grants, settings, Codex profiles |
| `~/.config/tokeman/claude-credential-history.jsonl` | Which credential settings offered when (fingerprints only) |
| `~/.config/tokeman/claude-admission-state.json` | Session bindings and reset-bounded rejection quarantines |
| `~/.config/tokeman/claude-rotate-state.json` | Cadence, mode, backoffs, carried samples |
| `~/.config/tokeman/claude-rotate.log` | Rotation events (names and headroom only) |
| `~/.config/tokeman/claude-login-keychain-backup.json` | The `/login` Keychain record set aside while managed |
| `~/.config/tokeman/claude-profile-reads.json` | Cached profile-endpoint answers and their backoff |
| `~/.local/share/tokeman/snapshots.db` | Probe history (sqlite) |

`XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME` are honoured. Every
file is owner-only; no token value is ever logged.

## Development

```sh
cargo nextest run                 # 175 unit tests, all in-process
cargo clippy --all-targets -- -D warnings
scripts/check-linux.sh            # the Linux job's checks, from a Mac (needs zig)
```

The library (`src/lib.rs`) has no UI dependency; `policy.rs` is pure and its
tests are the specification of when the default moves.

## Releases

Releases are built by [cargo-dist](https://opensource.axo.dev/cargo-dist/) and
published to GitHub Releases for macOS (ARM64, x86_64) and Linux (ARM64,
x86_64). Bump `version` in `Cargo.toml`, add the section to `CHANGELOG.md`
(the release notes come from it), then:

```sh
git tag v0.3.0
git push origin v0.3.0
```

## License

MIT

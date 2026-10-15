# Changelog

## 0.3.0 - unreleased

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

# Provider usage research, 2026-11-11

| file | what |
|---|---|
| [anthropic.md](anthropic.md) | live header probes on 8 accounts × 4 models; `/api/oauth/usage` shape; reset programs; read from the CC 2.1.322 binary |
| [codex.md](codex.md) | live `/wham/usage`; reset credits (openai/codex source) |
| [antigravity.md](antigravity.md) | `retrieveUserQuotaSummary`, credentials for both accounts, refresh, switching |
| [zai-kimi.md](zai-kimi.md) | z.ai `quota/limit` and Kimi `/coding/v1/usages`, both read live |
| [prior-art.md](prior-art.md) | ~30 external sources (CodexBar, jcode, oh-my-pi, OmniRoute, claude-go, support articles) |

## Headline facts

- **Anthropic: no account here reports a 5h window any more.** Max 20x was renamed Ultra on 2026-10-30,
  and verified Ultra has no 5h limit (support 17728952). Fable 5.2 is the only model with its own weekly
  window (`7d_oi`): a sub-cap of 50% of the weekly limit (`weekly_scoped_shares`) that can read over 100%.
  The overall `status` header only describes the requested model.
- **Refresh-token lifetime is reported.** The token endpoint returns `refresh_token_expires_in` (CC 2.1.322
  stores it as `refreshTokenExpiresAt`, keeping the earlier of the old and new values on refresh). tokeman's
  21-day heuristic (8cef9d5) can be replaced with the real date. External reports: login + 28 days (Aug),
  login + 30 days (Nov 9).
- **Codex: the 5h window is gone too**; `primary_window` is the weekly one. There are reset credits (2 here)
  that can be consumed with a POST.
- **Antigravity:** one POST per account returns both groups (Gemini; Claude+GPT), each with 5h and weekly
  buckets. Google refresh tokens don't rotate, so tokeman can hold its own copy safely.
- **GLM, Kimi:** plain GET endpoints with the API key; both have 5h and weekly windows. GLM reports errors
  as HTTP 200 with the error in the body. Kimi also returns an extra-usage wallet balance.

## Proposed order of work

1. **Anthropic correctness:** read `refresh_token_expires_in`; per-window `-status` and `-surpassed-threshold`;
   no clamp at 1.0 at parse; `limits[]` as the primary profile source (generic by kind and scope, with
   `weekly_scoped_shares`); "no 5h limit" once a `limits[]` without a `session` row confirms it; new grant
   fields (`type`, `starts_at`, `gives_back_weekly`, `unavailable_reason`) and the `?cedar_ember=1` read;
   record `slow-*`, `grace-*`, `upgrade-paths` (display only; not room).
2. **Codex:** classify windows by `limit_window_seconds`; show reset credits, `model_usage`,
   `spend_control`; `tokeman codex reset` (explicit claim); register the second account.
3. **A `providers/` layer for read-only quota** (Antigravity, GLM, Kimi): each yields `Vec<Bucket>` with a
   new `Source::Poll`. Shown in `tokeman list`, `--watch`, and the tray, and stored in `snapshots.db`.
   No rotation at first. Credentials: GLM and Kimi keys by file path in tokens.toml; Antigravity grants
   imported from the `agy` Keychain item and the IDE's state.vscdb.
4. **Later:** Antigravity CLI account switching (one Keychain item swap), once a running `agy` is shown
   to pick up the change.

## Should we use something else outright? (2026-11-11)

- **claude-swap** (realiti4, Python, ~4.6k★, pushed today) is the closest twin: auto-rotation, Ultra-aware,
  `--model Fable`, quarantines dead refresh tokens, per-terminal `cswap run`. It rotates by swapping the
  `/login` Keychain credential, so on macOS a switch only lands when Claude Code's Keychain cache expires,
  and it depends on refresh tokens that now die about 30 days after login. tokeman installs one
  `CLAUDE_CODE_OAUTH_TOKEN` (immediate, every session) and falls back to one-year setup keys; today 6 accounts
  are serving on setup keys alone (all refresh tokens are dead). It also does Codex migration. Keep tokeman.
  Worth taking: **weekly-first landing** (land on the viable account whose weekly resets soonest, because
  weekly quota left at its reset is lost), per-account disable/threshold, and exit codes plus JSON events for `--once`.
- **CodexBar** (steipete, Swift, ~23k★) reads ~100 providers, including Antigravity, z.ai, Kimi, Codex
  and Claude, and has a CLI (docs/cli.md). If the goal is only to *see* GLM/Kimi/Antigravity, installing it is the
  zero-code option. Building it into tokeman gets one dashboard, the snapshot history, and later rotation.
- **Small tricks to take:** the claude-swap #535 split (a missing header means unknown; a usage-API null means no
  such limit); the jcode `.pending` journal around reset claims; CodexBar's "count a grant only if
  started and not expired"; oh-my-pi's per-IP 429 (back off the whole fleet, not one account); read
  `weekly_scoped_shares.limit_index` against the raw `limits[]`; Fable headroom =
  min(Fable remaining × allowance, weekly remaining); add a Mythos family.

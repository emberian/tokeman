# Prior art: other people reading Claude's subscription usage APIs

Researched 2026-11-11. The goal is to compare what other projects have found about `/api/oauth/usage`, the
`anthropic-ratelimit-unified-*` headers, `reset_rate_limits` and OAuth login lifetime with what tokeman
assumes in `src/anthropic/{profile,probe,resets}.rs`, DESIGN.md and commit 8cef9d5.

Provenance tags:

- **[doc]**: Anthropic's own help center, Claude Code docs or changelog, or the @claudeai account.
- **[code]**: another project's source, at the path and commit given.
- **[capture]**: a live response that another project recorded in its repo or issues.
- **[binary]**: someone's static read of a Claude Code release bundle.
- **[inference]**: my reading, not stated by any source.

Third-party text was read as data. None of it was executed.

---

## Sources

Commit dates are the last commit touching that file, from the GitHub API on 2026-11-11.

| Who | Where | Date | What they claim |
|---|---|---|---|
| Anthropic | [support 17728952 "What is the Ultra plan?"](https://support.claude.com/en/articles/17728952-what-is-the-ultra-plan) | live 2026-11-11 | Max 20x was renamed Ultra. **Verified** Ultra accounts have no five-hour limit. Unverified ones get "a temporary limit" until verified. "If you don't see a Current session bar, your account is verified." Ultra has a weekly all-models limit and a separate weekly Fable limit. [doc] |
| Anthropic | [support 11647753 "How do usage and length limits work?"](https://support.claude.com/en/articles/11647753-how-do-usage-and-length-limits-work) | live 2026-11-11 | Paid plans have a 5h session limit and weekly limits. "Weekly limits apply across all models, and some plans also have a separate weekly limit for Fable." The weekly reset time is fixed per account. Limits can change "with weekly and monthly caps or model and feature usage". [doc] |
| Anthropic | [support 17506494 "Claude Fable 5.2 launch offers"](https://support.claude.com/en/articles/17506494-claude-fable-5-2-launch-offers) | live 2026-11-11 | Fable 5.2 launched 2026-11-09. Max, Ultra and premium seats get 1 to 3 Fable resets, by tenure (<6 months: 1, 6-12: 2, >12: 3), expiring 2027-03-08 23:59 PT. Pro gets Fable inside the plan until 2026-11-16, capped at 50% of the weekly limit, plus 1 reset expiring 2026-11-16. A Fable reset gives back only the weekly usage spent on Fable and refills the whole 5h window if the account has one. Its example: 80% weekly (50 Fable + 30 other) goes back to 30%. Resets can be used early and cannot be undone. [doc] |
| Anthropic / @claudeai | [x.com/claudeai/status/2116220869479428500](https://x.com/claudeai/status/2116220869479428500) | 2026-10-30 | "Max 5x → Max, Max 20x → Ultra … removing the 5-hour limit for verified Ultra accounts, starting today with most existing subscribers." [doc, seen as a search snippet] |
| Anthropic | [claude.com/pricing](https://claude.com/pricing) | live | Ultra: "No 5-hour limit ‡ (verified accounts)". [doc, search snippet] |
| Anthropic | [anthropics/claude-code CHANGELOG.md](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md) | last commit 2026-11-10 | 2.1.318 added `loginExpiresAt` to `claude auth status` and `credential_expires_at` to `claude -p` JSON. It also fixed the error shown "when a stored claude.ai login reaches its time limit". 2.1.312 renamed "session limit" to "5-hour limit" in messages. 2.1.305 added `CLAUDE_CODE_RETRY_WATCHDOG_USAGE_LIMIT_RECHECK_MS`. 2.1.283 fixed the weekly Fable limit not showing with telemetry off. [doc] |
| Anthropic | [code.claude.com/docs/en/authentication](https://code.claude.com/docs/en/authentication) | live | "When the login you created with `/login` is within three days of expiring, Claude Code shows a warning at startup." [doc, search snippet] |
| steipete | [CodexBar `docs/claude.md`](https://github.com/steipete/CodexBar/blob/main/docs/claude.md) @c727c08a | 2026-11-08 | Reads `GET /api/oauth/usage?cedar_ember=1`. On a 400/403 it retries once without the query. Sends `anthropic-beta: oauth-2025-04-20`, and the reset inventory needs UA `claude-cli/<ver> (external, cli)`. Needs the `user:profile` scope. `five_hour` null means no session row. Fallbacks: `seven_day_oauth_apps` for `seven_day`, `limits[].weekly_scoped` for per-model rows, `seven_day_routines`/`seven_day_cowork` for a "Daily Routines" row. Free accounts return null `five_hour` and `seven_day`. The web twin is `claude.ai/api/organizations/{org}/usage?cedar_ember=1`. [code] |
| steipete | CodexBar `Sources/CodexBarCore/Providers/Claude/ClaudeRateLimitResetCredits.swift` @4bd90c43 | 2026-10-04 | Counts `resets_left` over grants that are not paused, have started and have not expired, and ignores `usable_now`. Requires `eligible: true`. Never redeems. [code] |
| 1jehuang | [jcode `docs/anthropic-fable-resets.md`](https://github.com/1jehuang/jcode/blob/master/docs/anthropic-fable-resets.md) @e6c94f94, `crates/jcode-base/src/usage/anthropic_fable_reset.rs` @fb64b269 | 2026-11-10 | From a static read of CC **2.1.322**: status is `GET /api/oauth/usage?cedar_ember=1&skip_spend=1`. The `cedar_ember` block has `eligible, at_limit, exhausted, grants, next_grant_id, weekly_resets_at, cooldown_until`. Grant fields: `id, resets_left, starts_at, ends_at, clears, paused, usable_now, use_requires_limit, percent_used, blocking, type, gives_back_weekly, unavailable_reason`. The CLI identifies Fable grants **by `gives_back_weekly`**. Claim results: `reset, already_used, not_limited, cooldown, ineligible, unavailable`. Grant id `^[a-z0-9_-]{1,40}$`, request id `^[A-Za-z0-9_-]{1,64}$`. Treats an uncertain claim as possibly spent, using a durable `.pending` journal. [binary, code] |
| can1357 | [oh-my-pi `packages/ai/src/usage/claude.ts`](https://github.com/can1357/oh-my-pi/blob/main/packages/ai/src/usage/claude.ts) and `claude-reset.ts` @5bdac7ef | 2026-11-11 | `seven_day_opus` and `seven_day_sonnet` have been "permanently null" since 2026-07-02, and per-model caps come only from `limits[]`. `is_active` marks only the binding row. "Anthropic answers `five_hour: null` with no `session` row while an account has no running 5-hour window." The `/usage` 429 is "rate-limited per source IP" and "stays rate-limited for minutes", so it never retries a 429 inside one fetch and backs off 60 s. Program blocks are null "until the matching probe query (`?cedar_ember=1`, `?at_wall=1`) asks", so **null means unknown, not zero**. Maps the `7d_oi` header to a Fable tier and also knows a `mythos` tier. [code] |
| can1357 | oh-my-pi `packages/ai/src/providers/anthropic-slow-mode.ts` @52512d95 | 2026-09-25 | Before the slow lane, the server may grant "a small wrap-up allowance (drawn from the weekly limit) past the session limit". 2xx responses then carry `grace-{5h,7d}-utilization` > 0. [code] |
| cheaper-inference | [OmniRoute `open-sse/services/claudeLowPriority.ts`](https://github.com/cheaper-inference/OmniRoute/blob/main/open-sse/services/claudeLowPriority.ts) @62c170f8 | 2026-11-09 | Slow-lane wire contract, captured from CC 2.1.263. A 5h-wall 429 carries `slow-offer: treatment|control`. Accepting means resending with request header `anthropic-usage-limit: slow` until the 5h reset. `slow-status` is `active, not_needed, slot_busy, weekly_limit, budget_exhausted, ineligible, off`. Also documents `slow-retry-after`, `slow-max-wait`, `slow-budget-utilization` (0..1) and `slow-budget-reset`. CC defaults: retry 20 s (5 s to 600 s), max-wait 20 min (1 min to 6 h), ±30% jitter, 10 min cool-off. [code] |
| cheaper-inference | OmniRoute `open-sse/services/claudeLimitReset.ts` @62c170f8, `claudeResetCreditCount.ts` @f3a3ad37 | 2026-11-09 | `juniper_tide` status is read from `?at_wall=1&skip_spend=1`: `{eligible, ineligible_reason, in_experiment, arm: control|reset, available, next_available_at, weekly_resets_at, resets_per_week}`. The claim body is `{program:"juniper_tide"}`. The org UUID comes from provisioning, falling back to `/api/claude_cli/bootstrap`. [code] |
| gbasin | [claude-go `internal/api/ratelimit.go`](https://github.com/gbasin/claude-go/blob/main/internal/api/ratelimit.go) @bbd9f529 | 2026-10-06 | The full header list it parses, read from the CC bundle. It includes repeatable `unified-claim`, `overage-reset`, `overage-scope`, `overage-utilization`, `overage-period-channel-utilization`, `overage-period-monthly-utilization`, `overage-surpassed-threshold`, `upgrade-paths`, and **`unified-fallback == "available"`**. [code] |
| wtfwhs | [tengu-decoded `versions/2.1.300/plan-tier-gating.md`](https://github.com/wtfwhs/tengu-decoded/blob/main/versions/2.1.300/plan-tier-gating.md) @98f56b42 | 2026-10-14 | `seven_day_overage_included` is labeled "Fable limit" and abbreviated `7d_oi` in headers. `overage-scope` is `service|channel|group_pool`. The near-limit 5h checkpoint threshold depends on tier: 0.9975 for `default_claude_max_20x`, 0.99 for `max_5x`, 0.95 otherwise. `cedar_ember` (gate `tengu_cedar_ember`) can clear any of `five_hour, seven_day, seven_day_overage_included, seven_day_opus, seven_day_sonnet`. `juniper_tide` (gate `tengu_nifty_lemur`) clears `five_hour` only, once a week, "uses weekly limit". Status reads are `?cedar_ember=1&skip_spend=1` and `?at_wall=1&skip_spend=1`. `anthropic-usage-limit: extended` (gate `tengu_lantern_spool`) predates `slow`. [binary] |
| shcv | [harness-investigations `changelog-v2.1.304.md`](https://github.com/shcv/harness-investigations/blob/main/archive/claude-code/changelog/changelog-v2.1.304.md) @6d793ffe, `changelog-v2.1.217.md` @449e72f3 | 2026-10-19, 2026-07-28 | 2.1.304: `/limit-reset` gains `5-hour`, `weekly`, `full` and `fable` choices, and "A Fable reset returns only Fable's weekly usage". **`starry_waterfall` is the usage-credit offers gate** (`tengu_starry_waterfall`: discounts and bonus credits in `/usage-credits`), not a window. 2.1.217: a grace state machine reads `unified-grace-status` and `grace-{5h,7d}-utilization` ("Usage limit reached — grace window active. Wrap up"). [binary] |
| xsyetopz | clawdipedia `openspec/changes/archive/2026-10-20-track-2.1.304/binary-diff.md` @d0fa08c1 | 2026-10-19 | New gates in 2.1.304: `cedar_ember_give_back` and `starry_waterfall(+_asked/_shown/_wall)`. [binary] |
| heikopanjas | [token-torch `docs/unused-usage-response-fields.md`](https://github.com/heikopanjas/token-torch/blob/main/docs/unused-usage-response-fields.md) @59b6a2fe, `docs/claude-code-usage-response.json` @e05a42d8 | 2026-10-31 | The best field-by-field captures, from 2026-10-04 and 2026-10-31. Every window carries `limit_dollars, used_dollars, remaining_dollars, locked_reason`. Top-level keys include `spend{used{amount_minor,currency,exponent},…}`, `seven_day_breakdown`, `member_dashboard_available`, `promo_credits`, `pace`, `granite_teacup`, `starry_waterfall` and `amber_cistern_offer` (all null), plus `weekly_scoped_shares`. The `cedar_ember` block has `paused_held` and `event_props{surface, tier, tenure_bucket, billing_path, billing_period, extra_usage_state}`, and `tier` still says `claude_max_20x` after the Ultra rename. `iguana_necktie` is a $250 cloud-session credit window. Example grant id `opus55-launch-promax-20260921`, with `clears` = `five_hour, seven_day, seven_day_overage_included`. Always null: `nimbus_quill, cinder_cove, copper_kite, brass_thimble, harbor_lantern, wattle_ember, amber_ladder, amber_cistern, amber_gauge`. [capture] |
| FrancMunoz | [claude-status `docs/data-source.md`](https://github.com/FrancMunoz/claude-status/blob/main/docs/data-source.md) @1352028c | 2026-10-27 | A capture of `weekly_scoped_shares: [{limit_index: 2, used_percent_of_weekly: 27, allowance_percent_of_weekly: 50}]`. `limit_index` indexes the **raw** `limits[]`, unreadable entries included. Checked: Fable 54% × allowance 50% = 27. `percent` arrives as an int and `utilization` as a float. `severity` is an open set. It "do[es] not assume 5 h" for `session`. [capture] |
| FernandoTN | [ClaudeUsageWidget `docs/research/2026-09-22-claude-reset-credits.md`](https://github.com/FernandoTN/ClaudeUsageWidget/blob/main/docs/research/2026-09-22-claude-reset-credits.md) @4ff4ac86 | 2026-09-24 | Live on 4 accounts. On the `?at_wall=1&skip_spend=1` read, `spend` and `extra_usage` become null and **both** program blocks are filled in. With a non-CC user agent both were `ineligible_reason: "surface"`, so `grants: []` there means unknown. [capture] |
| suppressio | [aipermiler `docs/RESEARCH.md`](https://github.com/suppressio/aipermiler/blob/main/docs/RESEARCH.md) @5a23cfc8 | 2026-10-27 | Free tier (expired subscription): every window null, `limits: []`, `member_dashboard_available: false`, `weekly_scoped_shares: null`. A Team seat had all windows null, with data in `spend` and `limits` instead. `extra_usage` and `spend` are the same budget. [capture] |
| DanielC000 | loom `docs/decisions/26b3977f-usage-endpoint-shape-still-unconfirmed.md` @45550be4 | 2026-11-03 | The full top-level key list from a live poll. The server returns keys that the installed CLI binary does not contain (`seven_day_breakdown`, `weekly_scoped_shares`, `member_dashboard_available`), so binary archaeology has a ceiling here. [capture] |
| cortexkit | [anthropic-auth#572](https://github.com/cortexkit/anthropic-auth/issues/572) | 2026-10-30 | A verified Ultra account's `/api/oauth/usage` returns HTTP 200 with `five_hour: null` and a normal `seven_day`. A fail-closed rotator marked the account ineligible. [capture] |
| realiti4 / jasonczc | [claude-swap PR #535](https://github.com/realiti4/claude-swap/pull/535) | merged 2026-11-04 | An omitted, null or empty `five_hour` is "a limit the account does not have". A window that is present but unreadable is a bad response. Separates "the header probe: missing header is missing information" from "the usage API says null". [code] |
| noctalia-dev | [community-plugins#1344](https://github.com/noctalia-dev/community-plugins/issues/1344) | 2026-11-07 | For verified Ultra, `five_hour: null` **and `limits[]` has no `session` row**. "Pro, Max and unverified Ultra accounts still have the 5-hour window." [capture] |
| (various) | serious-alchemy/arbiter#1205/#1206, TauCetiWorker#429, DevDock-AI/claude-unlimited#20, titeya/dms-claudecode#56, hank-warren/pi-extensions#89, NousResearch/hermes-agent#152095 | 2026-10-31 to 11-07 | Other tools broke on `five_hour: null` or missed `weekly_scoped`. They confirm the shape and add nothing new. [capture] |
| ArkNill | claude-code-hidden-problem-analysis `02_RATELIMIT-HEADERS.md` @5500c2ee | 2026-04-22 | `unified-fallback-percentage` was 0.5 on 37,363 of 37,363 requests and is "a fixed per-account value". Its meaning is undocumented. [capture, old] |
| LeenHawk | gproxy `crates/gproxy-channel/src/channels/claudecode/quota.rs` @fa6d3744 | 2026-11-02 | Header windows `5h`, `7d`, `7d_oi` (mapped to `seven_day_fable`), plus `slow_budget`, `grace_five_hour` and `grace_seven_day` as extra allowances read only from 2xx responses. Low-priority mode hands the 5h limit to the upstream, and "extended" mode also hands over the weekly limit. Scoped model limits stay enforced in both. [code] |
| anthropics/claude-code issues | [#84090](https://github.com/anthropics/claude-code/issues/84090) (comment by 33modeling) | 2026-08-05 | All logins died together. The cause was "the 28-day login lifetime": the renewed credential shows `refreshTokenExpiresAt` ≈ login + 28 days. The 3-day warning shows only at startup. [capture] |
| anthropics/claude-code issues | [#108586](https://github.com/anthropics/claude-code/issues/108586) | 2026-11-09 | `claudeAiOauth.refreshTokenExpiresAt` = **login + 30 days** (measured: login 2026-11-09, expiry 2026-12-09), and daily access-token refreshes do not move it. [capture] |
| anthropics/claude-code issues | [#108871](https://github.com/anthropics/claude-code/issues/108871) | 2026-11-10 | Two interactive sessions on one login race the refresh, and the loser reports "could not be refreshed" although the stored credentials are valid. [capture] |
| bernardopg | [AiOverviewControl#123](https://github.com/bernardopg/AiOverviewControl/issues/123) | 2026-11-11 | Retrying a 403 from `prepaid/credits` on every refresh triggers 429 on the usage endpoint. That suggests the 429 budget is shared across endpoints. [capture] |

HN, Reddit and X searches through Scry (2026-10-01 onward) found nothing technical about the endpoint or the headers. They only had user complaints and the Ultra news: [HN 50308320](https://news.ycombinator.com/item?id=50308320) (2026-10-30, linking the Ultra article) and [HN 50350904](https://news.ycombinator.com/item?id=50350904) (2026-11-03). One Reddit post ([r/ClaudeCode 1xdk3cn](https://reddit.com/r/ClaudeCode/comments/1xdk3cn), 2026-10-23) says `claude -p` burns the 5h window about 1.5x faster than interactive use, measured on four Max 20x accounts.

---

## Where prior art disagrees with or extends tokeman

### 1. A missing 5h window now has two causes, and one of them is permanent

tokeman's memory note (`anthropic-api-changes-2026-10.md`) and commit c3a62f5 say that a healthy 200 omits the `5h-*` headers "when the 5h window has no usage". That fits oh-my-pi's claim that `/usage` answers `five_hour: null` with no `session` row "while an account has no running 5-hour window". But since 2026-10-30, **verified Ultra accounts (formerly Max 20x) have no 5h limit at all** [doc]. Their `/usage` gives `five_hour: null` and no `session` row in `limits[]` every time [capture: cortexkit#572, noctalia#1344]. Unverified Ultra accounts still have a temporary limit [doc]. Whether that limit shows up as a `session` row is not documented.

- tokeman's rule that "an unreported window limits nothing" is correct for both cases. Most other rotators got this wrong and failed closed.
- Not modeled yet: whether an account is weekly-only. [inference] That changes several things:
  - The `juniper_tide` session reset is pointless for such an account.
  - A `cedar_ember` or Fable reset touches only the weekly window on it.
  - A 5h-wall `slow-offer` cannot arise.
  - The account's whole budget is the 7d window, so `best`/`seat` should rank it on weekly headroom alone.
- What the headers on a weekly-only account look like (whether `representative-claim` can still be `five_hour`) is not documented anywhere I found. It is worth one live probe on an Ultra account.

### 2. Refresh-token lifetime: 28 days in August, 30 days in November, and Claude Code knows the date

tokeman (8cef9d5) says "The token endpoint reports no refresh-token lifetime" and warns from day 21 against an observed ~28 days.

- #84090 (Aug 5): `refreshTokenExpiresAt` ≈ login + 28 days.
- #108586 (Nov 9): `refreshTokenExpiresAt` = login + **30** days, fixed, with no sliding renewal.
- Claude Code 2.1.318 exposes it as `loginExpiresAt` in `claude auth status` and as `credential_expires_at` in `claude -p` JSON [doc].

So Claude Code stores an explicit expiry in `~/.claude/.credentials.json` (`claudeAiOauth.refreshTokenExpiresAt`). Either the token endpoint returns it (a `refresh_token_expires_in`-style field, which GitHub code search did not find in any public repo) or the CLI computes it. [inference] tokeman should check two things: the raw token-exchange response it already receives, and `refreshTokenExpiresAt` in a fresh Claude Code credentials file. If either carries the date, store it instead of guessing from `logged_in_at`, because the lifetime has apparently already changed once. All sources agree it does not slide with refreshes.

### 3. Fable is a sub-cap inside the weekly limit, and `weekly_scoped_shares` says how big

- Anthropic's docs say only Fable has a separate weekly limit [doc]. Pro's launch week capped Fable at 50% of the weekly limit.
- `weekly_scoped_shares[]` = `{limit_index, used_percent_of_weekly, allowance_percent_of_weekly}`, with `limit_index` pointing into the **raw** `limits[]` [capture: FrancMunoz, token-torch]. Observed `allowance_percent_of_weekly: 50` on Max and Ultra.
- Fable usage also counts toward `weekly_all`. The Fable reset example (80% = 50 Fable + 30 other) shows this [doc].

tokeman reads Fable from `7d_oi` headers (Fable requests only) and the `weekly_scoped` row, but not `weekly_scoped_shares`. Two consequences [inference]:
- Fable headroom is `min(Fable row remaining × allowance, weekly_all remaining)`.
- A 100% Fable row means about 50% of the weekly limit spent on Fable.

The legacy `seven_day_opus`/`seven_day_sonnet` have been null since July [code: oh-my-pi]. tokeman's legacy fallback in `profile::buckets` is harmless but dead. oh-my-pi and tengu-decoded also know a **Mythos** tier (`HVn = [Fable, Haiku, Mythos, Opus, Sonnet]`, model id `claude-mythos-5`). tokeman's `Family` enum has no Mythos.

### 4. Reset grants: a Fable grant does not "refill the listed windows"

`resets.rs` doc: "`cedar_ember` grants refill the listed windows (`clears`) without moving the weekly reset day." For Fable grants, `clears` can list `seven_day` while the grant gives back only the Fable share of it [doc]. Claude Code tells these grants apart by **`gives_back_weekly`** [binary: jcode on 2.1.322].

`ResetGrant` does not model these grant fields: `starts_at`, `gives_back_weekly`, `type`, `blocking`, `unavailable_reason`, `arm`. CodexBar counts a grant only when it is not paused, `starts_at <= now < ends_at` and `resets_left` is readable. tokeman would count a grant that has not started yet.

The program block also has `exhausted`, `weekly_resets_at`, `paused_held` and `event_props`.

The claim body `{program, grant_id, request_id}` and the result enum match tokeman exactly. jcode adds:
- the regexes `^[a-z0-9_-]{1,40}$` (grant) and `^[A-Za-z0-9_-]{1,64}$` (request);
- the policy that an uncertain claim must be journaled and never retried with a new grant.

Grants that exist now [doc]:
- Fable 5.2 launch: 1 to 3 per Max/Ultra account, by tenure, expiring 2027-03-08 PT.
- Pro: 1, expiring 2026-11-16.
- The earlier `opus55-launch-promax-20260921` grant expired 2026-10-22.

### 5. Which query form reveals which program

- tokeman reads both programs from `?at_wall=1&skip_spend=1`. FernandoTN saw both blocks filled in on that read, so this works.
- Claude Code 2.1.300+ and 2.1.322 read `cedar_ember` from **`?cedar_ember=1&skip_spend=1`** and `juniper_tide` from `?at_wall=1&skip_spend=1` [binary: tengu-decoded, jcode]. CodexBar uses `?cedar_ember=1`.
- Every source agrees the program keys are null on the plain read, and that **null or `ineligible_reason: "surface"` means unknown, not zero** [code: oh-my-pi; capture: FernandoTN].
- `skip_spend=1` nulls `spend` and `extra_usage`.

[inference] Whether `at_wall=1` alone evaluates cedar fully is not proven. Copying Claude Code's `cedar_ember=1` read is the safer choice.

### 6. Headers tokeman does not parse

From claude-go, tengu-decoded, OmniRoute, oh-my-pi and gproxy:

- **`anthropic-ratelimit-unified-claim`**: repeatable, one per exceeded window. It is more precise than `representative-claim` when two windows are hit.
- **`unified-fallback`** (`"available"`): claude-go reads this, not `fallback-percentage`. `fallback-percentage` was a constant 0.5 per account in April [capture]. tokeman's test fixture has it with the same value.
- **`overage-scope`** (`service|channel|group_pool`), `overage-reset`, `overage-utilization`, `overage-period-channel-utilization`, `overage-period-monthly-utilization`, `overage-surpassed-threshold`: Team and Enterprise spend caps.
- **`upgrade-paths`** (contains `overage`, `upgrade_plan`).
- **`slow-offer`, `slow-status`, `slow-retry-after`, `slow-max-wait`, `slow-budget-utilization`, `slow-budget-reset`**: the low-priority lane (see 7).
- **`grace-status`, `grace-5h-utilization`, `grace-7d-utilization`**: a wrap-up allowance past the limit. **A 2xx can arrive with the window at or above 1.0**, while grace is > 0 [code: oh-my-pi, gproxy; binary: shcv 2.1.217]. tokeman should not read "200 with 7d-utilization 1.0" as contradictory or as headroom.

### 7. Low-priority lane: an alternative to rotating at the 5h wall

A 429 at the 5h wall can carry `slow-offer: treatment`. Resending with request header `anthropic-usage-limit: slow` keeps the account serving at low priority until the 5h reset, charged to the weekly limit [code: OmniRoute; binary: tengu-decoded].
- `slot_busy` 429s and 529s mean "wait `slow-retry-after`".
- `weekly_limit`, `budget_exhausted`, `ineligible` and `off` end the lane.
- It applies to `five_hour` only, and only to the `treatment` arm.

tokeman does not install request headers for Claude Code. [inference] It cannot opt sessions in, but it could read `slow-offer` and `slow-status` in the probe to tell "walled but can continue slowly" from "walled". An older `anthropic-usage-limit: extended` value also exists (gate `tengu_lantern_spool`). gproxy treats it as also lifting the weekly limit. I found no capture of its behaviour.

### 8. The `/api/oauth/usage` 429 may be per IP, not per account

oh-my-pi: "rate-limited per source IP" and it "stays rate-limited for minutes". AiOverviewControl#123 saw other claude.ai API calls push the usage endpoint into 429. tokeman backs off **per account** (`FIRST_BACKOFF_SECS` 600, max 3600). [inference] If the limit is per IP, a 429 on one account should hold back polling of all accounts. With N accounts on a 300 s cache, tokeman sends N requests per 5 minutes from one IP. This is not verified. Watch whether a 429 on one account coincides with 429s on the others.

### 9. Smaller facts

- `limits[]` rows: `{kind, group, percent, severity, resets_at, scope{model{id,display_name}, surface}, is_active}`.
  - `percent` can be an int.
  - `severity` ∈ {normal, warning, critical, …}.
  - `scope.model.id` has been null in every capture.
  - `scope.surface` exists. tokeman already skips surface rows and ignores `is_active`, which matches oh-my-pi's reasoning.
  - FrancMunoz: do not assume `session` is 5 h; use `resets_at`.
- Free or lapsed accounts: every window null, `limits: []`, `member_dashboard_available: false`. A Team seat can have every window null with data only in `spend` [capture: aipermiler]. tokeman already treats paused accounts as 403 `oauth_not_allowed_for_organization`. A lapsed-to-Free account answers 200 with nothing, which is another "no quota" case.
- Tier naming: `event_props.tier` still says `claude_max_20x` after the rename to Ultra [capture]. In code the tier strings are `default_claude_max_20x`, `default_claude_max_5x`, `enterprise_usage_based`, `default_claude_zero` [binary].
- Claude Code's own 5h near-limit thresholds are 0.9975 (max_20x), 0.99 (max_5x) and 0.95 (others) [binary: tengu-decoded]. These are a reference point for tokeman's floors.
- Codenames:
  - **`starry_waterfall`** is the usage-credit offer program, a gate plus a usage key that has been null in every capture. It is not a window.
  - **`wattle_ember`**, **`amber_cistern`** and the newer **`amber_cistern_offer`** were null in every capture I found (2026-09-22 to 2026-11-03). Nobody has decoded them.
  - `iguana_necktie` is a dollar window ($250 cloud-session credit), and `cinder_cove` has also appeared with `limit_dollars` [capture: aipermiler].
  - Treat any populated codename window with `limit_dollars` as money, not as quota.

## Open questions worth one live probe each

1. Which headers does a verified Ultra account get on a 200 and on a 429? Is there any `5h-*` header or `five_hour` claim?
2. Does tokeman's token-exchange or refresh response carry a refresh-token expiry field, and does the current lifetime measure as 28 or 30 days?
3. Does `?at_wall=1&skip_spend=1` return the same `cedar_ember` grants as `?cedar_ember=1&skip_spend=1` when the account is not at a wall?
4. Is the `/api/oauth/usage` 429 shared across accounts from one IP?

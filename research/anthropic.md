# Anthropic subscription usage: state of the API, 2026-11-11

Sources, each marked:
- **live**: 1-token `/v1/messages` probes on 2026-11-11 (~20:30Z) from 8 accounts with setup keys,
  on `claude-haiku-4-5-20251001`, `claude-fable-5-2`, `claude-opus-5-5`, `claude-sonnet-5-5`.
- **cache**: tokeman's own `~/.config/tokeman/claude-profile-reads.json`, last good bodies 2026-10-14 and
  2026-10-29. `/api/oauth/usage` answered 429 to every live attempt today; the only full-scope login
  (`toot@`) belongs to a paused org (403 `oauth_not_allowed_for_organization` on inference).
- **cc**: the Claude Code 2.1.322 binary (`~/.local/share/claude/versions/2.1.322`, built 2026-11-09),
  read with `strings` plus a context extractor. Minified identifiers are quoted as they appear.

## 1. Response headers (`anthropic-ratelimit-unified-*`)

### What every account sends now (live)

No account in the fleet sent **any `5h-*` header**: not the 0% accounts, not the 80% ones, not the 429.
In tokeman's own log, `bobb@` still had a 5h window on an earlier date, so this was rolled out per
account, not all at once. Typical 200:

```
unified-status = allowed_warning            # overall verdict FOR THE REQUESTED MODEL
unified-representative-claim = seven_day    # which window drives status
unified-reset = 1794657600
unified-7d-status = allowed_warning         # per-window status: allowed | allowed_warning | rejected
unified-7d-utilization = 0.8                # fraction
unified-7d-reset = 1794657600
unified-7d-surpassed-threshold = 0.75       # which warning threshold was crossed (absent below)
unified-fallback-percentage = 0.5
unified-overage-status = rejected
unified-overage-disabled-reason = out_of_credits
unified-upgrade-paths = overage             # NEW: comma list of what the user could buy
```

On `claude-fable-5-2` (and only on Fable; Opus 5.5 and Sonnet 5.5 have no own window) the response adds
`7d_oi-{status,utilization,reset,surpassed-threshold}`. **`7d_oi` utilization can exceed 1.0** (1.02 seen).
When it is exhausted, Fable returns 429 with `representative-claim = seven_day_overage_included`,
`status = rejected`, and `retry-after` = seconds to the weekly reset (66288). On the **same** account,
Haiku answers 200 `allowed_warning`. The overall `status` describes the requested model only.

### What CC parses (cc)

The window table, in order: `[["five_hour","5h"],["seven_day","7d"],["seven_day_overage_included","7d_oi"],["overage","overage"]]`.
For each prefix it reads `-utilization`, `-reset`, `-surpassed-threshold` and `-status` (status is not read
for overage). A window counts when it has `surpassed-threshold`, or both utilization and reset.
CC also has local "early warning" curves: five_hour warns at util ≥ 0.9 when ≤ 72% of the window has passed;
seven_day at 0.75/0.5/0.25 by 60%/35%/15% of the window.

`rateLimitType` values CC knows: `five_hour`, `seven_day`, `seven_day_opus`, `seven_day_sonnet`,
`seven_day_overage_included`, `overage`. Labels: "5-hour limit", "weekly limit", "Opus limit",
"Sonnet limit", "Fable limit", "usage credit limit".

Headers CC reads that tokeman ignores today:

| header | meaning (cc) |
|---|---|
| `-{5h,7d,7d_oi}-status` | per-window allowed/allowed_warning/rejected |
| `-{w}-surpassed-threshold` | warning threshold crossed (0.25/0.5/0.75/0.9/1.0) |
| `-upgrade-paths` | e.g. `overage`; drives upsell copy |
| `-fallback` / `-fallback-percentage` | `unifiedRateLimitFallbackAvailable`; model fallback |
| `-overage-scope` | what the overage covers |
| `-overage-daily-cap-paused` | `"true"` when the daily overage cap paused spending |
| `-overage-period`, `-overage-period-monthly-utilization`, `-overage-period-channel-utilization` | org/channel monthly spend caps ("channel's monthly spend limit") |
| `-grace-5h-utilization`, `-grace-7d-utilization` | grace allowance past a limit |
| `-slow-status` | **low-priority lane**: `active`, `not_needed`, `slot_busy`, `weekly_limit`, `budget_exhausted`, `ineligible`, `off` |
| `-slow-offer` | `treatment` / `control` (A/B arm) |
| `-slow-budget-utilization` (≤1), `-slow-budget-reset` | budget of the slow lane |
| `-slow-retry-after`, `-slow-max-wait` | seconds (`lowPriorityRetryAfterSeconds`, `lowPriorityMaxWaitSeconds`) |

None of the slow/grace headers appeared on today's probes (no account here is in that experiment, or they
only appear at a wall).

## 2. `GET /api/oauth/usage`

### Read modes (cc)

```
plain             /api/oauth/usage
cedar_ember       /api/oauth/usage?cedar_ember=1&skip_spend=1
at_wall           /api/oauth/usage?at_wall=1&skip_spend=1
                  /api/oauth/usage?at_wall=1&starry_waterfall=<surface>&skip_spend=1   (when the offer flag is on)
starry_waterfall  /api/oauth/usage?starry_waterfall=<surface>&skip_spend=1
```
`<surface>` defaults to `cli_menu`; others: `cli_credits_required`, `ide_banner`, `alert_band`,
`overage_band`, `code_usage_band`, `credits_required`, `code_credits_required`.
tokeman uses `plain` and `at_wall` only.

A body counts as a usage body if it has any of: `five_hour, seven_day, seven_day_oauth_apps,
seven_day_opus, seven_day_sonnet, cinder_cove, wattle_ember, extra_usage, limits`.

### Shape (cache, matches cc schema)

- `five_hour`, `seven_day`: `{utilization: 0-100, resets_at: ISO, limit_dollars, used_dollars, remaining_dollars, locked_reason}`.
- `seven_day_opus`, `seven_day_sonnet`, `seven_day_oauth_apps`, `seven_day_cowork`, `seven_day_omelette`,
  `cinder_cove`: same window shape, null when not applicable (all null on these Max 20x accounts).
- `wattle_ember`: a **dollar-denominated** window: `{utilization, resets_at, label, limit_dollars,
  used_dollars, remaining_dollars, periods: [{starts_at, ends_at}]}`. CC's "usage allocation" copy
  ("Now using your usage allocation") points at team/enterprise seat allowances.
- **`limits[]`: the authoritative list, and CC renders it verbatim** (statusline schema:
  "a new meter needs no client release"):
  `{kind, group, percent 0-100, resets_at|null, severity normal|warning|critical, is_active, scope}`.
  `kind ∈ {session, weekly_all, weekly_scoped, …}`, "classify a row on kind, never on a label".
  `scope = {model: {display_name, id}} | {surface: {display_name}} | null`. The Fable row has `id: null` today.
  **No `session` row = no 5h limit.** That is how the "5h gone" accounts look from this endpoint.
- `weekly_scoped_shares: [{limit_index, allowance_percent_of_weekly, used_percent_of_weekly}]`.
  Fable's scoped limit is **50% of the weekly**. A scoped row's percent is relative to its own allowance,
  so Fable 37% = 19% of the whole weekly. `limit_index` indexes `limits[]`.
- `seven_day_breakdown`: `{as_of, window_started_at, rows: [{key: claude_code|chat|cowork|other, display_name, percent}]}`.
- `extra_usage`: `{is_enabled, monthly_limit, used_credits, utilization, currency, disabled_reason,
  credits_ever_enabled, daily, weekly, user_disabled, spend_limit_reached}`.
- `spend`: `{enabled, can_toggle, can_purchase_credits, balance, cap, limit, percent, severity, used: {amount_minor, currency, exponent}, auto_reload, disclaimer}`.
- `pace` (null so far), `member_dashboard_available`.
- Also in tokeman's cached answers of 2026-11-11 and not in the lists above: top-level `indicator` and
  `promo_credits` (null), `seven_day_cowork`, `seven_day_omelette` (null); `limits[].group` is `session` or
  `weekly`; `cedar_ember.paused_held`, a grant's `arm` (null), `juniper_tide.event_props` (null) and
  `juniper_tide.ineligible_reason: "not_at_wall"`; `extra_usage.decimal_places`.
- Codenamed programs: `cedar_ember`, `juniper_tide`, `starry_waterfall`, `amber_cistern(_offer)`,
  `amber_gauge`, `amber_ladder`, `brass_thimble`, `copper_kite`, `granite_teacup`, `harbor_lantern`,
  `iguana_necktie`, `nimbus_quill`, `tangelo`, `omelette_promotional`.

### Which models share the Fable (`7d_oi`) bucket (cc)

GrowthBook flag `tengu_usage_overage_included_models`; built-in fallback `["Fable","Fable 5","Fable 5.1"]`
(display names). CC builds its header-derived Fable row from this list. Fable 5.2 carries `7d_oi` live, so
the server list includes it.

## 3. Reset programs

### `cedar_ember` (granted resets), cc schema
Program fields: `eligible, ineligible_reason ∈ {config_off, tier, seat, mobile, surface, cli_version,
no_grant, tenure, other_experiment, unavailable, unknown}, at_limit, exhausted[], grants[], next_grant_id,
weekly_resets_at, cooldown_until, event_props{surface, tier ∈ claude_pro|claude_max_5x|claude_max_20x|claude_team,
tenure_bucket ∈ under_14|14-29|30-89|90-364|365+, billing_path, billing_period, extra_usage_state}`.

Grant: `id, label, resets_total, resets_left, starts_at, ends_at, clears[], paused, usable_now,
use_requires_limit, percent_used{window: 0-100}, blocking[], type ∈ session|weekly|full, gives_back_weekly,
unavailable_reason ∈ {cooldown, blocked_five_hour, blocked_seven_day, blocked_opus, blocked_sonnet,
blocked_other, cannot_clear, needs_limit, unavailable}`.
Clearable windows include `five_hour, seven_day, seven_day_overage_included, seven_day_opus,
seven_day_sonnet, seven_day_cowork, seven_day_omelette, seven_day_oauth_apps`.
New since tokeman 0.3.0: **`type`, `starts_at`, `blocking`, `gives_back_weekly`, `unavailable_reason`**.
Copy: "Fable reset used · what Fable used is back in your weekly limit" (`gives_back_weekly`).

Claim: `POST /api/organizations/{org}/reset_rate_limits` `{program, grant_id, request_id}` →
`{result, reason, resets_left, cleared[], weekly_resets_at}`; failure reasons add `paused, expired,
unknown_grant, not_next_grant, grant_id_required, not_limited, already_used, cooldown,
stamp_indeterminate, reset_unconfirmed`. A claim can be marked `only_if_unused_reset`.

### `juniper_tide` (session reset at the 5h wall)
Unchanged shape (`available, eligible, ineligible_reason, next_available_at, resets_per_week, arm,
in_experiment, weekly_resets_at`). It only matters to accounts that still have a 5h window.

### `starry_waterfall` (usage-credit discount offer, new)
`{eligible, ineligible_reason, active, arm_id, kind ∈ none|pct_off|extra_credit, headline_pct 0-300,
menu ∈ base|plus40, default_bundle_id, segment ∈ never|lapsed|recent, first_offered_at, expires_at,
days_left, redeemed, event_props}`. A purchase upsell, not a quota. Display only.

## 4. What this means for tokeman

1. **5h is optional, and now usually absent.** 0.3.1 already treats an unreported window as limiting
   nothing. The UI should say "no 5h limit", not "5h unreported", once the profile confirms there is no
   `session` row (or headers have lacked it for N probes).
2. **Per-window status.** Read `-{w}-status` and `-surpassed-threshold`, and stop inferring rejection
   only from the overall status. The overall status is per requested model: a Haiku probe says
   `allowed_warning` on an account whose Fable is rejected.
3. **Don't clamp at 1.0** at the parse boundary. Keep over-100% (1.02) for display; policy clamps.
4. **`limits[]` should be the primary profile source**, rendered generically by kind/scope
   (session, weekly_all, weekly_scoped/model, weekly_scoped/surface), with `weekly_scoped_shares` to express
   a scoped limit as a share of the weekly. Legacy `seven_day_opus/sonnet` stay as fallbacks.
5. **Fable bucket key.** The header sample uses the family-wide key; the profile uses display name
   "Fable". `scope.model.id` exists (null today); prefer it when set.
6. **Resets:** parse the new grant fields; show `type` and `gives_back_weekly`.
7. **New low-priority lane (`slow-*`):** when an account is rejected but `slow-status=active`, it can still
   serve slowly. Record it and show it; policy should not count it as room.
8. **Store:** `snapshots.db` should carry `limits[]` rows generically (kind, scope label, percent, reset)
   rather than fixed columns.

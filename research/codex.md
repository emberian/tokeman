# Codex (ChatGPT) usage: state of the API, 2026-11-11

Sources: **live** = `GET https://chatgpt.com/backend-api/{wham,codex}/usage` on the one Codex account in
`~/.codex/auth.json` (plan `promax`), 2026-11-11. **src** = openai/codex `main`:
`codex-rs/backend-client/src/client/rate_limit_resets.rs` (last touched 2026-11-03, #59255),
`codex-rs/backend-client/src/types.rs`, and
`codex-rs/app-server/src/request_processors/account_processor/rate_limit_resets.rs`.
Installed CLI is 0.167.0.

## Usage body (live; `/wham/usage` and `/codex/usage` return the same thing)

```json
{
 "plan_type": "promax",
 "rate_limit": {
  "allowed": true, "limit_reached": false,
  "primary_window": {"used_percent": 42, "limit_window_seconds": 604800,
                     "reset_after_seconds": 177908, "reset_at": 1794607293},
  "secondary_window": null
 },
 "code_review_rate_limit": null,
 "additional_rate_limits": null,
 "model_usage": {"gpt-6-astra": {"available": true, "available_at": null, "credits_would_enable": false}},
 "credits": {"has_credits": false, "unlimited": false, "overage_limit_reached": false, "balance": "0",
             "approx_local_messages": [0, 0], "approx_cloud_messages": [0, 0]},
 "spend_control": {"reached": false, "individual_limit": null},
 "rate_limit_reached_type": null,
 "promo": null,
 "rate_limit_reset_credits": {"available_count": 2, "applicable_available_count": 0}
}
```

Changes against what tokeman assumes (its fixtures use primary = 18000 s, secondary = 604800 s):
- **The 5h window is gone here too.** `primary_window` is now the 7-day window and `secondary_window` is null.
  tokeman must label windows by `limit_window_seconds`, never by primary/secondary position.
- `model_usage`: per-model availability (`available`, `available_at`, `credits_would_enable`).
- `additional_rate_limits[]`: per-feature/model limits, each `{limit_name, metered_feature, rate_limit{…},
  normal_model_slug}` (src). Null on this account.
- `spend_control {reached, individual_limit}`, `rate_limit_upsell`, `promo`.
- **`rate_limit_reset_credits {available_count, applicable_available_count}`**: reset credits.
  This account has 2, with 0 applicable now (nothing is at a limit).

## Reset credits (src)

- List: `GET /backend-api/wham/rate-limit-reset-credits` (or `/api/codex/rate-limit-reset-credits`) →
  `{available_count, credits: [{id, reset_type, status, granted_at, expires_at, title, description}]}`.
  Live it answered **500** in the morning and **200** at 13:50 the same day, with fields beyond the
  source's: per credit `is_supported_by_plan`, `redeem_started_at`, `redeemed_at`, `profile_image_url`,
  `profile_user_id`; top level `total_earned_count`, `immediate_reset_purchase_eligible`, `history_enabled`.
  Timestamps are RFC 3339 strings. The CLI tolerates a failing list ("failed to fetch rate limit reset credit
  details; falling back to the usage response").
- Consume: `POST …/rate-limit-reset-credits/consume` `{redeem_request_id, credit_id?}` →
  `{code: reset|nothing_to_reset|no_credit|already_redeemed, windows_reset}`.
- "Luna Reserve": header `x-openai-codex-luna-reserve: 1` on the usage GET opts into a reserve-usage
  fallback (only for clients that can apply it).

## For tokeman
1. Classify windows by `limit_window_seconds` (18000 → 5h, 604800 → weekly). `tokeman codex list`
   shows neutral PRIMARY/SECONDARY columns, so nothing is mislabeled, but the user can't tell that
   PRIMARY is now the weekly window. Test fixtures still model primary as 5h.
2. Parse and show `rate_limit_reset_credits`, `model_usage`, `additional_rate_limits`, `spend_control`.
3. Possible new feature, `tokeman codex reset`: when an account is limited and `applicable_available_count > 0`,
   consume one credit (explicit user action, like `tokeman resets` for Anthropic).
4. Only one Codex account is registered in tokeman today; ember has two, so add the second
   (`tokeman codex add`/`discover`).

# Reading quota for Devin and Grok Build

Researched 2026-11-28 (~01:30 UTC). Provenance tags:

- **[live]**: seen in a real response on 2026-11-28, made with the login the service's own CLI keeps.
- **[log]**: written by the client itself to its own log on this machine.
- **[source]**: read in client source code or in the strings of the installed binary (paths below).
- **[web]**: other people's notes on the same endpoints.
- **[inference]**: my reading of the evidence, not stated anywhere.

Sources read:

- devin 3000.15.14 (`~/.local/share/devin/cli/_versions/3000.15.14/bin/devin`, a Rust binary): `strings`, the
  `PlanStatus` / `UserStatus` / `PlanInfo` field lists, the Connect paths. Its logs under
  `~/.local/share/devin/cli/logs/` record every `connect_post{path=...}`.
- grok 1.0.77 (`~/.grok/downloads/grok-1.0.77-macos-aarch64`): `strings`; `~/.grok/logs/unified.jsonl`;
  `~/.grok/settings_cache.json`; `grok models --debug-file`.
- `xai-org/grok-build@2bdd1d6a` `crates/codegen/xai-grok-shell/src/extensions/billing.rs` (the client is open source):
  the request, `BillingConfig`, `BillingConfigResponse`, its tests' bodies.
- `steipete/CodexBar` `docs/grok.md`; `robinebers/openusage#1237`; `akitaonrails/ai-usagebar#360`;
  `cckyros/devin-usage`; `jlcodes99/cockpit-tools` `src/types/windsurf.ts` (search snippets for the last two).

Neither CLI prints plan usage outside its TUI. `devin` has `/usage` only as a slash command; `devin auth status`
prints the login, tier and plan and no quota **[live]**. `grok usage [--json]` answers
"`grok usage` can't show your plan's usage here yet. Run /usage inside grok instead." unless an interactive grok
(its "leader") is running **[live]**.

---

## Devin

The Devin CLI is a Windsurf (Codeium) client: its key is called `windsurf_api_key` and its API server is
`https://server.codeium.com` **[live]**.

### Credential

- `$XDG_DATA_HOME/devin/credentials.toml`, else `~/.local/share/devin/credentials.toml`, on macOS too **[live]**
  (`%APPDATA%\devin\credentials.toml` on Windows **[web]**). Keys: `windsurf_api_key`, `api_server_url`,
  `devin_webapp_host`, `devin_api_url` **[live]**. Mode 0644 as the CLI writes it.
- The key is static: no expiry, no refresh, nothing to write back **[inference** from a file last written at login,
  2026-10-04, that still works**]**. `devin auth status` names this file as the credential **[live]**.
- The Keychain items `Devin Safe Storage` / `Windsurf Safe Storage` belong to the desktop apps, not the CLI **[inference]**.
- `~/.config/devin/config.json` holds the org id and preferences, no secret **[live]**.

### Endpoint and auth

- `POST {api_server_url}/exa.seat_management_pb.SeatManagementService/GetUserStatus`, Connect protocol with JSON:
  `Content-Type: application/json`, `Connect-Protocol-Version: 1` **[live]**. The CLI itself calls it three or four
  times at start and then every 20 minutes **[log]**, so a poll every five minutes is in its own range.
- The key travels in the body, not a header **[live]**:

  ```json
  {"metadata": {"apiKey": "<key>", "ideName": "devin-cli", "ideVersion": "3000.15.14",
                "extensionName": "devin-cli", "extensionVersion": "3000.15.14"}}
  ```

- With only `apiKey` in `metadata` the answer is HTTP 400 `{"code":"invalid_argument","message":"an internal error
  occurred"}` **[live]**. `ideName: "windsurf"` also works and returns a larger body (model configs) **[live]**.
  Others report `ideName: "devin"` and version `0.0.0` accepted **[web: ai-usagebar#360]**.
- No rate-limit headers; fronted by Google (`via: 1.1 google`) **[live]**.
- Errors are Connect errors, `{"code": "...", "message": "..."}` with a matching HTTP status. Only the 400 above was
  seen; `unauthenticated` for a bad key is **[inference]** from the protocol and was not provoked.

### Real response (ids, name and email redacted; `planInfo` cut to what matters)

The whole body, redacted, is `src/providers/fixtures/devin-user-status.json`.

```json
{"userStatus": {
  "pro": true, "name": "<redacted>", "email": "<redacted>", "userId": "<redacted>", "teamId": "<redacted>",
  "teamStatus": "USER_TEAM_STATUS_APPROVED", "teamsTier": "TEAMS_TIER_DEVIN_MAX",
  "planStatus": {
    "planInfo": {"teamsTier": "TEAMS_TIER_DEVIN_MAX", "planName": "Max", "monthlyPromptCredits": -1,
                 "isDevin": true, "billingStrategy": "BILLING_STRATEGY_QUOTA", "hideDailyQuota": true,
                 "devinInfo": {"orgId": "<redacted>", "webappHost": "app.devin.ai", "...": "..."}, "...": "..."},
    "planStart": "2026-11-04T07:56:19Z", "planEnd": "2026-12-04T07:56:19Z",
    "availablePromptCredits": -1,
    "dailyQuotaRemainingPercent": 100, "weeklyQuotaRemainingPercent": 98,
    "overageBalanceMicros": "-5258321",
    "dailyQuotaResetAtUnix": "1795852800", "weeklyQuotaResetAtUnix": "1795939200"},
  "teamConfig": {"...": "..."}, "cascadeModelConfigData": {}},
 "planInfo": {"...the same plan info without devinInfo...": "..."}}
```

### Field meanings

| field | meaning |
|---|---|
| `dailyQuotaRemainingPercent`, `weeklyQuotaRemainingPercent` | Percent of the window **left**, 0-100, a JSON number **[live]**. `100 - remaining` matched the CLI's `/usage` on a Pro account **[web: ai-usagebar#360]** and "0% remaining" in the CLI banner matched an absent weekly field **[web: openusage#1237]**. |
| absent percentage | The body is proto3 JSON, which omits zero. A window whose reset is present and whose percentage is absent is **spent**, not unknown: seen for the weekly window on a Max account while the CLI refused work with "weekly usage quota has been exhausted" **[web: openusage#1237]**, and for the daily window on a Pro account **[web: ai-usagebar#360]**. Not seen here (98% left). |
| `dailyQuotaResetAtUnix`, `weeklyQuotaResetAtUnix` | Unix seconds in a string (proto int64) **[live]**. Here 2026-11-28 08:00 UTC and 2026-11-29 08:00 UTC: both on 08:00 UTC, the weekly one on a Sunday **[live]**. A fixed calendar, not anchored to the subscription (`planStart` is 07:56:19) **[inference]**. |
| `planInfo.hideDailyQuota` | `true` on this Max plan **[live]**. Devin's own banner then shows the weekly quota alone **[web: openusage#1237]**. Whether the hidden daily window is still enforced is unknown; tokeman does not read it when hidden **[inference]**. |
| `overageBalanceMicros` | Millionths of a dollar, in a string. Dividing by 1,000,000 matched one tested account's USD balance; the currency is not in the body **[web: ai-usagebar#360]**. Here `-5258321`, i.e. -$5.26: the binary calls the row "Extra usage balance" **[source]**, and a negative value was also seen by others (`-44347`). That negative means extra usage owed rather than a display quirk is **[inference]**. |
| `planInfo.planName` | `"Max"`. `devin auth status` prints it as "Plan", and `teamsTier` (`TEAMS_TIER_DEVIN_MAX`) as "Tier: Devin Max" **[live]**. |
| `planStart`, `planEnd` | The billing cycle (RFC 3339), a month long **[live]**. Not a quota window. |
| `availablePromptCredits`, `planInfo.monthlyPromptCredits` | `-1` on a quota plan **[live]**: the older credit plans' counters, unused here. |
| `planInfo.billingStrategy` | `BILLING_STRATEGY_QUOTA` **[live]**. Credit plans are reported by other clients as a `credits` strategy **[web: cockpit-tools]**; the exact enum word is a guess. |
| not sent here | The binary's `PlanStatus` also has `availableFlexCredits`, `availableFlowCredits`, `usedPromptCredits`, `usedFlowCredits`, `usedFlexCredits`, `topUpStatus`, `wasReducedByOrphanedUsage`, `gracePeriodStatus`, `acuConsumed`, `acuLimit`, `dollarsLimit` **[source]**. The ACU fields are presumably the Team/Enterprise meter **[inference]**; never seen, so not mapped. |

### What is missing

- Nothing absolute: no token, request or ACU count behind the percentages on this plan.
- The 5-hour-style burst limit, if any, is not in this answer.
- An ACU-metered (Team/Enterprise) account would report a plan name and no window with this parser. The fields exist
  in the binary; their JSON types and meaning need a live body from such an account.
- A Devin Enterprise login uses another `api_server_url`; the parser honours the file's value (https only) but no
  such login was available to test.

---

## Grok Build

### Credential

- `~/.grok/auth.json` (`$GROK_HOME/auth.json`), mode 0600, guarded by `auth.json.lock` **[live]**. A map from
  `<issuer>::<client id>` (here `https://auth.x.ai::<uuid>`) to one login: `key` (the access token, a JWT), `auth_mode`
  (`oidc`), `create_time`, `expires_at` (RFC 3339), `refresh_token`, `user_id`, `email`, `first_name`, `principal_type`,
  `principal_id`, `team_id`, `oidc_issuer`, `oidc_client_id` and a few privacy flags **[live]**.
- The access token lives **six hours** (`create_time` 01:08:35, `expires_at` 07:08:35) **[live]**.
- Refresh is an OIDC refresh grant at `https://auth.x.ai/oauth2/token` (found through
  `/.well-known/openid-configuration`) **[log]**. The binary's messages ("refresh found the scope removed by another
  process", "auth: adopted") show it expects other processes to redeem the same token, i.e. the refresh token rotates
  **[inference from source]**.
- **A refresh token that the server rejects makes grok empty the file.** On 2026-11-28 01:29 UTC, `grok models` on a
  login whose access token had expired on 2026-11-16 tried the refresh, got HTTP 400 `invalid_grant` ("Invalid or
  unknown refresh token", credential age 12 days), logged `auth.refresh.permanent_failure` and "cleared credentials
  after permanent refresh failure ... store emptied (no scopes left)", and left `auth.json` as `{}` **[live, log]**.
  So a grok login left idle does not last (this one was dead at twelve days; when it died is unknown), the next grok
  command deletes its record, and the only way back is `grok login` in a browser. This is why no live read was possible for this note.
- tokeman therefore never redeems the refresh token: a second redeemer of a rotating token signs the CLI out.

### Endpoint and auth

- `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits` **[source]**. The proxy forwards to the backend RPC
  `GetGrokCreditsConfig` **[source: comment in billing.rs]**. The route exists: without a token it answers HTTP 401
  `{"error":"Invalid or expired credentials (auth_kind=none, x_xai_token_auth=none, upstream=Unauthenticated,
  reason=no auth context)"}`, where an unknown path is an nginx 404 **[live]**.
- Headers grok sends **[source]**: `Authorization: Bearer <key>`, `X-XAI-Token-Auth: xai-grok-cli`, `x-userid: <user_id>`,
  `x-grok-client-version`, `x-grok-client-mode`. CodexBar sends only the first two and `Accept` and is answered
  **[web]**. tokeman sends the first three and `Accept`.
- Errors: a non-2xx status with `{"error": "<text>"}` **[source, live for 401]**.
- grok calls this on every prompt, on `/usage` and on a poll **[source: comment]**.

### Response

No body was captured off the wire. What follows is the config grok itself logged after a successful fetch
(`billing: fetched credits config` in `unified.jsonl`): the client's own re-serialization of what it parsed
(`BillingConfig`, camelCase, `None`s skipped), with `history` replaced by `historyLen` by the logger. 46 such lines
from 2026-08-20 to 2026-11-15 **[log]**.

```json
{"config": {"creditUsagePercent": 6.0,
            "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY",
                              "start": "2026-10-02T04:07:01.963528+00:00", "end": "2026-10-09T04:07:01.963528+00:00"},
            "onDemandCap": {"val": 0}, "onDemandUsed": {"val": 0}, "prepaidBalance": {"val": 0},
            "isUnifiedBillingUser": true,
            "billingPeriodStart": "2026-10-02T04:07:01.963528+00:00",
            "billingPeriodEnd": "2026-10-09T04:07:01.963528+00:00", "historyLen": 0},
 "onDemandEnabled": null, "subscriptionTier": "SuperGrok Heavy"}
```

`onDemandEnabled` and `subscriptionTier` are **not** in the proxy's answer: grok fills them from its remote settings
after parsing **[source]**. On the wire the envelope is `{"config": {...}}` **[source: `BillingConfigResponse`, its tests]**.

### Field meanings

| field | meaning |
|---|---|
| `config.creditUsagePercent` | "Included credit usage as a percentage of the allowance (0.0 to 100.0)" **[source]**. Percent **used**. Whole numbers in every logged value (1, 5, 6, 20, 56, 80, 91, 92) **[log]**. |
| absent `creditUsagePercent` | Zero, omitted by proto3 JSON **[inference]**. In the log it is absent exactly at the start of each period and before the first use of it: absent 2026-09-01 08:20 then 5.0 at 18:32 the same day; absent 09-10 04:22 then 1.0 at 17:53; absent 10-03 then 5.0 on 10-06; absent for the periods of 10-09 and 11-13, in which grok was barely used **[log]**. grok-build says the same of its money fields ("proto3 JSON omits zero-valued scalars, so a `$0` Cent arrives as `{}`") **[source]**. CodexBar treats absence as unknown and reports plans whose answer never carries the field **[web]**; no such plan was seen here. |
| `config.currentPeriod` | `{type, start, end}`, RFC 3339. `type` is `USAGE_PERIOD_TYPE_WEEKLY` here **[log]**; "weekly or monthly" per the source's comment, so `USAGE_PERIOD_TYPE_MONTHLY` is **[inference]**. The period is seven days from the subscription's own instant (04:07:01.96 UTC on a Friday), not a calendar week **[log]**. |
| `config.billingPeriodStart` / `End` | Deprecated copies of the period **[source]**; equal to `currentPeriod` in every logged line **[log]**. |
| `config.monthlyLimit`, `config.used` | Deprecated: the older `GetGrokBuildBillingConfig` shape, cents under `val` **[source]**. Never in the log. |
| `config.onDemandCap`, `config.onDemandUsed`, `config.prepaidBalance` | US cents under `val` ("Cent value from the billing API (USD cents)") **[source]**. `prepaidBalance` is bought credits; the on-demand pair is pay-as-you-go spend and its cap. All 0 here **[log]**. |
| `config.isUnifiedBillingUser` | The account draws on a pool shared with grok.com (set from the remote setting `unified_consumer_billing_enabled`) **[source]**. `true` here **[log]**. So the percentage is of the whole SuperGrok allowance, not of Grok Build's share alone **[inference]**. |
| `config.productUsage[]` | `{product, usagePercent}` per product; in grok-build's test body, "not read yet" by the CLI **[source]**. Not in the log (the client drops it). |
| `config.history[]` | Past periods **[source]**; length 0 in every logged line **[log]**. |
| plan | `subscription_tier_display` ("SuperGrok Heavy"), else `subscription_tier`, from the remote settings (`GET {base}/settings`), which grok caches in `~/.grok/settings_cache.json` as `{"payload": "<json>", "signature": ...}` with the settings under `payload.settings` **[live on disk, source]**. tokeman reads the cache file, not the endpoint. |

### What is missing, and how sure this is

- **Not read live.** The login on this machine had expired, and finding that out erased it (above). The request and
  the wire shape are from grok's source; the values are from grok's own log of the same account. The parser has not
  met a real response. First thing to do after `grok login`: `tokeman grok import`, then compare
  `tokeman grok list --json` with `/usage` inside grok, and replace the test's logged body with a captured one.
- One window only. If the plan has a shorter burst limit it is not in this answer.
- No absolute numbers: a percentage of an allowance whose size is not sent.
- **tokeman can read Grok only while grok's own access token is live** (six hours after grok last renewed it). With
  no grok running the row says the login expired. Reading at any time needs tokeman to hold a grant of its own, as it
  does for Antigravity (`agy login`): an OIDC browser login with the CLI's client id whose refresh token only tokeman
  redeems. Not built.

---

## Mapping onto tokeman's `quota::Limit`

- Devin: for each of `daily` (unless `hideDailyQuota`) and `weekly`, `used = (100 - remaining) / 100`, `resets_at` the
  Unix reset; a reset without a percentage is `used = 1`; neither means no such window. `plan = planInfo.planName`.
  `overageBalanceMicros / 10,000` cents is a `Balance` ("extra usage balance", USD, may be negative). A plan with no
  window reads as a plan name with no limit, not as an error.
- Grok: one account-wide `Limit` whose span is `end - start` of the period (weekly here), `used = creditUsagePercent / 100`
  (0 when absent and a period exists; `used / monthlyLimit` for the old shape), `resets_at = end`. `prepaidBalance` (when
  not zero) and `onDemandCap - onDemandUsed` of `onDemandCap` (when a cap is set) are `Balance`s. No period at all is
  a decode error.
- Both are `Source::Poll`, scope `All`, with no `Detail`: the fleet view, `best`, the forecast and the attention
  items treat them as they treat GLM and Kimi.

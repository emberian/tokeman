# Reading quota for the z.ai GLM Coding Plan and Kimi Code

Researched 2026-11-11. Provenance tags:

- **[live]**: seen in a real response on 2026-11-11 (~20:30 UTC), made with `~/.zai-key` / `~/.kimikey`.
- **[source]**: read in client source code (paths below).
- **[web]**: vendor documentation.
- **[inference]**: my reading of the evidence, not stated anywhere.

Sources read:

- kimi-code 2.3.1 (Homebrew, `@moonshot-ai/kimi-code`), bundled:
  `/opt/homebrew/Cellar/kimi-code/2.3.1/libexec/lib/node_modules/@moonshot-ai/kimi-code/dist/main.mjs`
  (region `packages/oauth/src/managed-usage.ts`, about lines 61318-61500; the `/usage` renderer is about lines 460290-460360).
- CodexBar `steipete/CodexBar@7447bc77` (2026-11-08):
  `Sources/CodexBarCore/Resources/Plugins/zai.js`, `Sources/CodexBarCore/Providers/Zai/ZaiAPIRegion.swift`,
  `Sources/CodexBarCore/Providers/Kimi/KimiUsageFetcher.swift`, `Sources/CodexBarCore/Providers/Kimi/KimiModels.swift`.
- The official z.ai plugin `zai-org/zai-coding-plugins@0446d0bb`:
  `plugins/glm-plan-usage/skills/usage-query-skill/scripts/query-usage.mjs`.
- Docs: https://docs.z.ai/devpack/overview, https://www.kimi.com/en/help/membership/membership-update-rules,
  https://www.kimi.com/en/help/kimi-code/benefits (seen as a search snippet only).

Neither service returns quota or rate-limit headers on a messages response **[live]**. I made one
`max_tokens: 1` request to each Anthropic-compatible endpoint, and the response headers held only
date, content-type, request-id, cf-ray and similar. A tokeman reading for these services therefore
has to poll the usage endpoint. There is no header path like the Haiku probe.

---

## z.ai GLM Coding Plan

### Endpoint and auth

- `GET https://api.z.ai/api/monitor/usage/quota/limit` **[live][source]**.
  China mirror: `https://open.bigmodel.cn/api/monitor/usage/quota/limit`. The same global key works there and
  returns the same data with `msg: "操作成功"` **[live]**.
- `Authorization: <key>` (the raw key) and `Authorization: Bearer <key>` both work **[live]**. The official plugin
  sends the raw key along with `Accept-Language: en-US,en` and `Content-Type: application/json` **[source]**. CodexBar
  sends Bearer **[source]**.
- **Errors arrive as HTTP 200** with the error in the body **[live]**:
  - no header: `{"code":1001,"msg":"Authentication parameter not received in Header, unable to authenticate","success":false}`
  - bad key: `{"code":401,"msg":"token expired or incorrect","success":false}`
  - a CN account with no plan: `success:false, code:500, msg:"当前用户不存在coding plan"` **[source: zai.js]**

  The gate is therefore `success == true && code == 200`, not the HTTP status.
- Team scope: add the headers `Bigmodel-Organization` and `Bigmodel-Project` and the query `?type=2` **[source: zai.js]**.

### Real response (redacted: nothing identifying in it)

```json
{"code":200,"msg":"Operation successful","success":true,
 "data":{"level":"max",
  "limits":[
   {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,"currentValue":177,"remaining":27822,
    "percentage":1,"nextResetTime":1794429533576},
   {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,"currentValue":1849,"remaining":138150,
    "percentage":1,"nextResetTime":1794879119966}]}}
```

### Field meanings

| field | meaning |
|---|---|
| `type` | `CREDIT_LIMIT` is the current credit plans **[live]**. `TOKENS_LIMIT` is a legacy token plan's 5h window; `TIME_LIMIT` is the monthly MCP/tool-call quota (web search, web reader, zread) **[source: zai.js, query-usage.mjs]**. This account has no `TIME_LIMIT` entry. Under credit plans, MCP calls are charged as credits (1.2 credits per call) **[web]**. |
| `unit` + `number` | Window length. The unit codes are 1=day, 3=hour, 5=minute, 6=week **[source: zai.js multipliers `{1:1440,3:60,5:1,6:10080}`]**. So `3/5` is 5 hours and `6/1` is 1 week **[live]**. In a `TIME_LIMIT` entry, `unit 5, number 1` is a *marker* for monthly, not one minute **[source]**. |
| `usage` | The window's **limit** (the name is misleading), in credits **[live]**. The values match the docs: Max is 28,000 per 5h and 140,000 per week; Pro is 12,000/60,000; Lite is 2,000/10,000 **[web]**. |
| `currentValue` | Credits used in the window **[live]**. |
| `remaining` | `usage - currentValue` **[live]**. |
| `percentage` | An integer percent used, rounded *up* (177/28000 = 0.6% is reported as 1) **[live]**. Compute `currentValue/usage` yourself instead, as CodexBar does. |
| `nextResetTime` | **Epoch milliseconds**, absolute and TZ-free **[live]**. **Omitted** when the 5h window is idle **[live]**. |
| `usageDetails` | Present on `TIME_LIMIT` entries: per-tool `{modelCode, usage}` **[source]**. |
| `data.level` | Plan tier, lowercase: `"max"` **[live]**. The others are presumably `lite` and `pro` **[inference]**. |

### Reset semantics

- **5h window: one fixed block that opens at the first spend and fully resets** **[live]**. The docs say "credit quota
  resets 5 hours after consumption" **[web]**. Live behavior: at 20:39:23 UTC, just after `nextResetTime` 20:38:53 passed,
  the 5h entry read `currentValue 0, remaining 28000, percentage 0` **with no `nextResetTime` key at all**. Its window
  had run 15:38:53-20:38:53, and the first activity in the hourly history falls in that 15:00 UTC hour. So:
  - `nextResetTime` is **optional**. It is absent while no window is open (idle). Treat it as "full, nothing pending".
  - A reset returns the whole window to zero. It is not a partial refund. The next spend opens a new 5h block.
  - CodexBar drops a 5h `nextResetTime` that lies more than 5h+60s in the future as implausible **[source]**. tokeman should apply the same guard.
- **Weekly window**: "activated upon subscription; resets every 7 days" **[web]**. It is a fixed 7-day cycle
  anchored to the subscription time. The live reset was 2026-11-17 01:31:59 UTC.
- **Peak pricing**: Mon-Fri 14:00-18:00 UTC+8 (06:00-10:00 UTC) charges 1x; off-peak charges 0.5x **[web]**. The
  API exposes nothing about this; it depends only on the clock. A credit is
  `(in*mult_in + cached*mult_cached + out*mult_out)/10000` **[web]**.
- The values update with some lag: after a 1-token request, `currentValue` was unchanged 10 s later **[live]**.

### History endpoints (optional, for charts)

- `GET /api/monitor/usage/model-usage?startTime=yyyy-MM-dd HH:mm:ss&endTime=...` (URL-encoded) and
  `GET /api/monitor/usage/tool-usage?...`. Without the parameters the body is `code 500 "Parameter validation failed"`
  (still HTTP 200) **[live]**.
- `model-usage` data: `x_time[]` (`"2026-11-10 00:00"` hourly labels), `modelCallCount[]`, `tokensUsage[]`,
  `modelDataList[{modelName,tokensUsage[],totalTokens}]`, `totalUsage{totalModelCallCount,totalTokensUsage,modelSummaryList}`,
  `granularity:"hourly"` **[live]**. The arrays can contain `null` for empty hours **[inference: CodexBar coerces non-integers to 0]**.
- `tool-usage` data: `x_time[]`, `networkSearchCount[]`, `webReadMcpCount[]`, `zreadMcpCount[]`, `totalUsage{...}` **[live]**.
- **The timestamps are UTC+8** (Beijing/Singapore), not the caller's zone and not UTC **[live]**. A query at
  20:31 UTC returned its last bucket as `2026-11-12 04:00`, and the server's `x-log-id` also starts with UTC+8
  wall time. The official plugin and CodexBar both format the caller's *local* time, so their windows are
  off by the zone difference **[source + live]**.

### Rate limits on the usage endpoint

No rate-limit headers were returned **[live]**. A burst of 8 calls in about 2 s all returned 200 in about 250 ms
**[live]**. No documented limit was found. Polling once a minute is harmless.

---

## Kimi Code ("Kimi For Coding")

### Endpoint and auth

- `GET https://api.kimi.com/coding/v1/usages` with `Authorization: Bearer <key>` **[live][source]**. The
  host `https://api.kimi.ai/coding/v1/usages` (the global alias in kimi-code's `GLOBAL_KIMI_CODE_BASE_URL`) answers
  the same way **[live]**. kimi-code honors `KIMI_CODE_BASE_URL` **[source]**.
- An `sk-kimi-…` API key works **[live]**. kimi-code itself sends its OAuth access token (from
  `~/.kimi/credentials/kimi-code.json`, which `auth.kimi.com` issues) to the same URL **[source]**. tokeman should use
  the API key and leave the CLI's OAuth grant alone.
- A raw key without `Bearer` returns HTTP 401 `{"error":{"message":"Invalid Authentication","type":"invalid_authentication_error"}}`.
  A bad key also returns HTTP 401 **[live]**. Here the HTTP status is meaningful: 401 means bad auth, 404 means the endpoint is unavailable for that key, and 403 means permission or quota was denied **[source]**.
- A companion endpoint, `GET https://api.kimi.com/coding/v1/me` (same auth), returns the plan tier: `user_level`
  (int) and `user_level_name`, for example `30` / `"Vivace"` **[live]**. It also returns PII (user_id, nickname, avatar,
  phone, global_id), which tokeman should not store.

### Real response (booster wallet ids redacted)

```json
{
 "usage":  {"limit":"100","used":"21","remaining":"79","resetTime":"2026-11-12T21:40:19Z"},
 "limits": [{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
             "detail":{"limit":"100","used":"1","remaining":"99","resetTime":"2026-11-11T23:40:19Z"}}],
 "usages": {"limit_5h":{"used_ratio":0.0057,"reset_time":"2026-11-11T23:40:17Z"},
            "limit_7d":{"used_ratio":0.2119,"reset_time":"2026-11-12T21:40:18Z"}},
 "booster_wallet": {"id":"<redacted>","userId":"<redacted>",
   "balance":{"id":"<redacted>","feature":"FEATURE_OMNI","type":"BOOSTER","amount":"5000000000",
              "amountLeft":"2195500648","unit":"UNIT_CURRENCY","periodStart":"2026-08-25T21:13:27.949998Z",
              "periodEnd":"2126-08-25T21:13:28.949998Z","domain":"DOMAIN_NEXUS", "...":"..."},
   "status":"STATUS_ACTIVE","allowTopup":true,"topupLimit":{"currency":"USD","priceInCents":"100000"},
   "monthlyChargeLimitEnabled":true,"monthlyChargeLimit":{"currency":"USD","priceInCents":"5000"},
   "monthlyUsed":{"currency":"USD","priceInCents":"0"}, "...":"..."}
}
```

### Field meanings

The body has two parallel shapes. Read the new one and fall back to the old one.

| field | meaning |
|---|---|
| `usages.limit_5h` / `limit_7d` / `limit_month_total` / `limit_month_code` | **New shape** **[live + source]**. `used_ratio` is a float from 0 to 1, and `reset_time` is RFC 3339 UTC (`Z`). kimi-code's `/usage` shows "5h limit", "Weekly limit" and "Monthly limit", in that order and only for the entries present. `limit_month_code` is Kimi Code's *share* of the shared monthly pool, not an independent limit **[source: kimi-code `monthlyBreakdown`, CodexBar comment]**. |
| `usage` (top level) | **Legacy shape**, the weekly window. `limit`/`used`/`remaining` are **strings** on a 0-100 scale: percentages, not request counts. `used:"21"` matches `limit_7d.used_ratio 0.2119` **[live]**. `resetTime` is camelCase RFC 3339. |
| `limits[]` | **Legacy shape**, the per-window detail. `window.duration` + `timeUnit` (`TIME_UNIT_MINUTE`/`HOUR`/`DAY`) gives the length, here 300 min = 5h. `detail` is again a 0-100 string percent, ceil-rounded (`"1"` for 0.57%) **[live + source: KimiModels.swift]**. |
| `booster_wallet` | Paid "extra usage" credit. Money amounts are **fixed-point, 1e6 units per cent**: `amount` 5000000000 = $50.00 and `amountLeft` 2195500648 = $21.96 left **[source: kimi-code `FIXED_POINT_CENTS = 1e6`]**. `priceInCents` values are plain cents in strings. **kimi-code 2.3.1 parses `payload["boosterWallet"]` (camelCase), but the server sends `booster_wallet`**, so the CLI's extra-usage row is silently empty against this response **[live + source]**. Parse both spellings. |
| plan tier | Not in `/usages` for this account. CodexBar reads an optional `user.membership.level` (`LEVEL_FREE`=Adagio, `LEVEL_TRIAL`=Andante, `LEVEL_BASIC`=Moderato, `LEVEL_INTERMEDIATE`=Allegretto, `LEVEL_ADVANCED`=Allegro) when it is present **[source]**. `/me` returned `user_level_name: "Vivace"` **[live]**. |

### Reset semantics

- **Two plan generations** **[web: kimi.com membership-update-rules]**. *Legacy* plans (Andante and higher; this account)
  have a rolling 5-hour limit **and** a weekly limit, which "resets every 7 days from the subscription start time". *New*
  plans (Plus and higher) have the rolling 5h limit and **no weekly limit**, and both generations draw on a **monthly shared
  credit pool** that refreshes on the subscription anniversary (for example 15:00 on Dec 1 refreshes at 15:00 on Jan 1). That
  is why a response may carry `limit_7d` (legacy) or `limit_month_total`/`limit_month_code` (new) **[inference
  joining web + source]**.
- `reset_time` is absolute UTC. Its seconds field jitters by ±1 s between calls (`:17`, `:18`, `:19`), so the server computes
  it per request **[live]**. Compare at minute granularity.
- In this sample both the 5h reset (23:40 UTC) and the weekly reset (21:40 UTC) land on :40 **[live]**. That suggests the 5h
  window is also aligned to the subscription anchor rather than to first use. This is **[inference]**, and one sample cannot
  settle it.
- When the 5h quota is exhausted, the server answers HTTP 429 "You've reached…" on inference **[web: error-reference snippet]**.

### Rate limits on the usage endpoint

No rate-limit headers were returned (Cloudflare-fronted; `__cf_bm` cookie) **[live]**. A burst of 8 calls all returned
200 in about 750 ms each **[live]**. kimi-code uses an 8 s timeout **[source]**. No documented limit was found.

---

## Mapping onto tokeman's `quota::Window { used: 0..1, resets_at: epoch s }`

- z.ai: for each `limits[]` entry, `used = currentValue/usage`, `resets_at = nextResetTime/1000`, and the label comes
  from `unit`/`number`. When `nextResetTime` is missing (an idle 5h window), the window is empty: `used = 0` with
  no reset pending.
- Kimi: `used = usages.<k>.used_ratio`, `resets_at = parse_rfc3339(reset_time)`. Fall back to the legacy
  `usage`/`limits[]` with `used = used/100`.
- The `Source` enum would need a new variant (for example `Endpoint` or `Poll`), because neither service fits
  `Headers` or `Profile`.

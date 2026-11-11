# Google Antigravity: quota reading and account switching

Researched 2026-11-11 against `agy` 1.5.5 (`~/.local/bin/agy`, built 2026-11-10; the strings this
note relies on were re-checked in 1.5.6, and the switching experiment ran on 1.5.6) and
Antigravity.app 2.0.1 (`/Applications/Antigravity.app`, May 2026). Each fact is tagged:

- **[live]** verified with a real request on 2026-11-11
- **[code]** read from the installed binaries (strings / embedded proto descriptors) or local state
- **[web]** from docs or open-source projects (cited)

No tokens appear in this file. Emails are the two account identities, which are already in
local plaintext state.

## The two accounts

| account | where its grant lives | plan |
|---|---|---|
| `first@example.com` | `agy` CLI: macOS Keychain + file fallback | Google AI Ultra **[live]** |
| `second@example.com` | Antigravity.app (IDE): `state.vscdb` | Google AI Ultra **[live]** |

Both grants come from the **same OAuth client** (see Refresh), so a grant from one store works in
the other. **[live]**

## Credential locations **[code]**

### `agy` CLI (primary, in daily use)

1. **Keychain (primary)**: generic password, service `gemini`, account `antigravity`, in
   `login.keychain-db`. Value is `go-keyring-base64:<base64 of JSON>`. The JSON:
   ```json
   {"token": {"access_token": "<ya29…, 260 chars>", "token_type": "Bearer",
              "refresh_token": "<1//…, 103 chars>",
              "expiry": "2026-11-11T13:07:02.25159-08:00", "expires_in": 3599},
    "auth_method": "consumer",
    "id_token_email": "first@example.com"}
   ```
   (`token` is a Go `oauth2.Token`.) `agy` rewrites it on every refresh; mdate was today.
   Logs say `ChainedAuth: authenticated via keyring (effective: keyring)`.
   `security find-generic-password -s gemini -a antigravity -w` read it with no prompt.
2. **File fallback**: `~/.gemini/antigravity-cli/antigravity-oauth-token` (0600), the same JSON
   as plain text. Used when the keyring times out (`Keyring LoadStoredToken timed out after %v,
   falling back to file storage`). Currently **stale** (expiry 2026-11-09); the Keychain copy is
   the live one. CodexBar reports that setting `SSH_TTY` makes `agy` use file storage only
   **[web]**, which gives a way to run `agy` against a private `HOME` without touching the
   Keychain.
3. "Active account" is simply whatever grant is in that slot. There is one slot; no account
   list. `~/.gemini/google_accounts.json` (`{"active": null, "old": ["second@example.com"]}`) belongs
   to the old Gemini CLI and is not read by `agy`.

### Antigravity.app (VS Code fork)

`~/Library/Application Support/Antigravity/User/globalStorage/state.vscdb`, table `ItemTable`
(open with `?mode=ro`):

- `antigravityUnifiedStateSync.oauthToken`: base64 protobuf. Outer entries are
  `{1: sentinelKey, 2: {1: value}}`. Entry `oauthTokenInfoSentinelKey` holds another base64 protobuf:
  `1` access_token, `2` "Bearer", `3` refresh_token, `4 {1: expiry unix seconds}`. Entry
  `authStateWithContextSentinelKey` holds JSON `{"state":"signedIn", ...}`.
- `antigravityUnifiedStateSync.userStatus`: base64 protobuf cache of the user status: `3` name,
  `7` email (`second@example.com`), `33` model configs, each with `15 = quotaInfo {1: remainingFraction
  (float), 2: resetTime}`. This is a cache, last written in May.
- The stored access token expired 2026-05-25; the refresh token was still valid today.

The CLI's per-app dirs `~/.gemini/antigravity{,-ide,-backup}/` hold conversations/brain, not
credentials. Nothing under `~/.antigravity/` holds credentials (extensions, argv.json).

## Refresh **[live]**

```
POST https://oauth2.googleapis.com/token
Content-Type: application/x-www-form-urlencoded

grant_type=refresh_token
client_id=1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com
client_secret=GOCSPX-K58F…           (28 chars after the prefix)
refresh_token=<1//…>
```

- Response keys: `access_token`, `expires_in` (3599), `id_token`, `scope`, `token_type`.
  **No `refresh_token` is returned: Google does not rotate it.** A refresh is idempotent and a
  stored refresh token can be held by several copies at once. This is the opposite of
  Anthropic's single-use refresh tokens: no lock-and-redeem discipline is needed for
  correctness, only to avoid wasted requests.
- Scopes granted: `cloud-platform cclog userinfo.profile openid experimentsandconfigs userinfo.email`.
- The `id_token`'s `aud`/`azp` equal the client id above, for both accounts.
- The client id and secret are embedded in both `agy` and the app's
  `Contents/Resources/bin/language_server`. Both binaries carry several client ids
  (`1071006060591-…`, `884354919052-…`, plus `1098991175725-…` in `agy`) and several `GOCSPX-`
  secrets, and the order of the strings does not show which secret goes with which id. Tested:
  `1071006060591` with `GOCSPX-9YQW…` gives `invalid_client`; with `GOCSPX-K58F…` it works. Extract at
  runtime with `[0-9]{8,}-[a-z0-9]{32}\.apps\.googleusercontent\.com` and
  `GOCSPX-[A-Za-z0-9_-]{28}`, or pin them. CodexBar extracts them from the app the same way
  **[web]**. The pair is also published in many OSS projects.
- Adding a new account: the simplest route is to sign in with `agy` (interactive Google OAuth,
  `auth_method: consumer`) and then import the grant from the Keychain slot.

## Quota endpoints

Host: `agy` 1.5.5 talks only to `https://daily-cloudcode-pa.googleapis.com` (log counts: 69,888
streamGenerateContent, 4,656 loadCodeAssist, 4,488 fetchAvailableModels). Prod
`https://cloudcode-pa.googleapis.com` answers the same calls identically. **[live]**

Common headers: `Authorization: Bearer <access_token>`, `Content-Type: application/json`, and a
**User-Agent that matters** (see the next section).

### 1. `POST /v1internal:loadCodeAssist`: tier and project **[live]**

Body: `{"metadata":{"ideType":"ANTIGRAVITY","platform":"DARWIN_ARM64","pluginType":"GEMINI"}}`
(enum values from the embedded `ClientMetadata` proto; `{"metadata":{"ideType":"ANTIGRAVITY"}}` is
what Antigravity-Manager sends **[web]**). The relevant response fields:

```json
{"currentTier": {"id": "free-tier", "name": "Antigravity", "upgradeSubscriptionType": "GDP_HELIUM", "...": "..."},
 "allowedTiers": [{"id": "free-tier", "isDefault": true}, {"id": "standard-tier", "userDefinedCloudaicompanionProject": true}],
 "cloudaicompanionProject": "aicode-consumers",
 "gcpManaged": false,
 "paidTier": {"id": "g1-ultra-tier", "name": "Google AI Ultra",
              "upgradeSubscriptionText": "You are subscribed to the best Google AI plan."}}
```

The plan is `paidTier.id`, not `currentTier`. That `currentTier` says `free-tier` for an Ultra
account is expected; CodexBar and Antigravity-Manager both prefer `paidTier` **[web]**. The
project for consumer accounts is the shared `aicode-consumers`.

### 2. `POST /v1internal:retrieveUserQuotaSummary`: **the endpoint to use** **[live]**

This is what `agy`'s `quota_manager.go` calls (`doRefreshQuota` → `Cache(retrieveUserQuotaSummary)`)
and what `/usage` displays **[code]**.

Body: `{"project":"aicode-consumers"}` (`{}` also works).

Proto **[code]** (`google.internal.cloud.code.v1internal`):
```
RetrieveUserQuotaSummaryRequest  { string project = 1; }
RetrieveUserQuotaSummaryResponse { repeated QuotaSummaryBucket buckets = 1 (deprecated?);
                                   repeated QuotaSummaryGroup groups = 2; string description = 3; }
QuotaSummaryGroup  { string display_name; string description; repeated QuotaSummaryBucket buckets; }
QuotaSummaryBucket { string bucket_id; string display_name; string description; string window;
                     oneof remaining { double remaining_fraction; int64 remaining_amount; }
                     bool disabled; google.protobuf.Timestamp reset_time; }
```

Real response, `first@example.com`, Agy-hub User-Agent:

```json
{"groups": [
  {"displayName": "Gemini Models",
   "description": "Models within this group: Gemini Flash, Gemini Pro",
   "buckets": [
     {"bucketId": "gemini-weekly", "displayName": "Weekly Limit Remaining", "window": "weekly",
      "resetTime": "2026-11-18T08:38:15Z", "remainingFraction": 0.9466626},
     {"bucketId": "gemini-5h", "displayName": "Five Hour Limit Remaining", "window": "5h",
      "resetTime": "2026-11-12T01:3x:xxZ", "remainingFraction": 1}]},
  {"displayName": "Claude and GPT models",
   "description": "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS",
   "buckets": [
     {"bucketId": "3p-weekly", "displayName": "Weekly Limit Remaining", "window": "weekly",
      "resetTime": "2026-11-18T08:38:15Z", "remainingFraction": 1},
     {"bucketId": "3p-5h", "displayName": "Five Hour Limit Remaining", "window": "5h",
      "resetTime": "2026-11-12T01:33:53Z", "remainingFraction": 1}]}],
 "description": "Within each group, models share a weekly limit and a 5-hour limit. Quota is consumed proportionally to the cost of the tokens. Thus, limits will last longer with shorter tasks or using more cost-effective models. The 5-hour limit smooths out aggregate demand to fairly distribute global capacity across all users, while your weekly limit is tied directly to your individual tier."}
```

`second@example.com` returned the same shape, every fraction 1, with every resetTime equal to now
+ window.

Reading notes:
- **An untouched window's `resetTime` floats** (now + 5h or now + 7d, different on every call).
  Once the window is used, it is anchored (cmrx64's weekly anchor is 2026-11-18T08:38Z). Treat
  `remainingFraction == 1` with a floating reset as "window not started".
- `remaining` is a oneof: `remainingAmount` (int64) can appear instead of `remainingFraction`.
  Nook PR #917 also saw the payload wrapped in `response`/`summary` and fractions left out
  **[web]**. Parse defensively.
- Other bucket ids exist: Antigravity-Manager observed `gemini-argon-wtu-weekly` for Gemini 4
  Argon (medium/low tiers), and argon-high metered by `gemini-5h` + `gemini-weekly` but admitted
  only while the argon bucket is above 0 (`src-tauri/src/modules/quota.rs`, comment dated
  2026-11-05) **[web]**. CodexBar shows a separate "Gemini 4 Argon weekly" group when the server
  sends one **[web]**. Neither of our accounts got an argon group today.
- Free and Plus accounts get weekly-only ("Starter") groups with no 5h bucket **[web: CodexBar docs]**.

### The User-Agent gates the Gemini group **[live]**

The same token and body against the same host:

| User-Agent | groups returned |
|---|---|
| `antigravity-cli/1.5.5 darwin/arm64` (made up) | Claude and GPT only |
| `antigravity/1.5.5 (aidev_client; os_type=darwin; arch=arm64)` | Claude and GPT only |
| `antigravity/hub/2.28.0 (aidev_client; os_type=darwin; arch=arm64; cl=1008300195)` | **Gemini Models + Claude and GPT** |

Bisected on 2026-11-11 with bare `antigravity/hub/<v>` agents **[live]**: 2.7.0 and below
(and 2.0.1, the installed app's version, and `2.9` without a patch number) get only Claude/GPT;
2.8.0, 2.8.9, 2.9.0, 2.9.1, 2.28.0, 3.0.0, 9.0.0 get both groups. The gate is a version
comparison at hub ≥ 2.8.0. The suffix is not checked: `(aidev_client; os_type=…; arch=…)`, a
`cl=` field, or `darwin/arm64` all work. The format comes from Antigravity-Manager
`src-tauri/src/constants.rs::build_official_upstream_user_agent` **[web]**, and `agy` contains the
`%s/%s (%s)`, `aidev_client`, `os_type=%s` pieces **[code]**. tokeman sends
`antigravity/hub/2.28.0 (aidev_client; os_type=<os>; arch=<arch>)`, or the installed app's version
when that is newer.

### 3. `POST /v1internal:fetchAvailableModels`: per-model view (fallback only) **[live]**

Body `{"project":"aicode-consumers"}`. Response `models` is a map from model id to
`{displayName, quotaInfo:{remainingFraction, resetTime}, model:"MODEL_PLACEHOLDER_M402",
apiProvider, modelProvider, maxTokens, ...}`, plus `defaultAgentModelId`, `agentModelSorts`,
`tieredModelIds`, and so on. Ids seen today include `claude-opus-5-5-{low,medium,high}`,
`claude-sonnet-5-5-{low,medium,high}`, `gpt-oss-120b-medium`, `gemini-3.8-flash-tiered`,
`gemini-3.5-flash-lite`, `gemini-3.1-flash-image`, and internal `chat_*`/`tab_*`.

**Don't use it for quota.** Every Gemini model reported `remainingFraction: 1` while
`gemini-weekly` was 0.9467. Per-model quotaInfo is a 5h-only or minimum view, not the weekly
picture. sylearn/AIUsage issue #81 and Antigravity-Manager PR #3185 say the same **[web]**.

### 4. `POST /v1internal:retrieveUserQuota` (older) **[live]**

Body `{"project":…}` → `{"buckets":[{"modelId","tokenType":"WTUS"|"REQUESTS","remainingFraction"|"remainingAmount","resetTime"}]}`,
per model, the same information as fetchAvailableModels. Use only as a last resort.

### Rejections as a signal **[code]**

When a limit is hit, the `agy` logs (`~/.gemini/antigravity-cli/log/cli-*.log`, `cli.log` symlink)
carry `RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to
increase your limits. Resets in 159h54m37s.` (weekly) or `... Resets in 1h49m32s.` (5h). This is
the transcript-rejection analogue for an account that tokeman is not polling.

### Local alternatives (not needed)

- `agy -p /usage --output-format json` is the supported non-interactive report (since 1.1.11)
  **[web: CodexBar]**. In this machine's logs, 37 such runs were cancelled before
  `retrieveUserQuotaSummary` returned (`context canceled`). It is also heavy: it spawns a language
  server and refreshes models. The direct HTTP call is better.
- The app's and `agy`'s localhost language-server RPC
  (`/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary`) needs a CSRF token
  that `agy` ≥ 1.2.2 no longer exposes **[web: CodexBar]**.

## Limit structure

- Two groups, each with its own **5-hour** and **weekly** window: "Gemini Models" (Flash, Pro)
  and "Claude and GPT models" (Opus, Sonnet, GPT-OSS). **[live]**
- Consumption is proportional to token cost, not request count, and models within a group share
  the group's windows. **[live, server's own `description`]**
- Plans: Ultra gets the highest quota, refreshed every 5h, and the highest weekly limit. Pro gets
  a 5h refresh up to a weekly limit. Free and Plus get a weekly refresh only. Third-party
  (Claude/GPT) models need a paid plan. Pro and Ultra can spend AI credits for overage, controlled
  by the "AI Credit Overages" setting (Never/Always). The docs give no absolute numbers.
  **[web: https://antigravity.google/docs/plans/, https://antigravity.google/docs/cli/commands/usage]**
- `state.vscdb` has an `antigravityUnifiedStateSync.modelCredits` key (empty here), and `agy` has
  `/credits`. Credit balance is not covered here.

## Account switching

**CLI (`agy`): works by slot swap. [live, 2026-11-11, `agy` 1.5.6]** Built as
`tokeman services use <name>` (`src/providers/agy.rs`).

The slot is one Keychain generic password: service `gemini`, account `antigravity`, in the login
keychain. `agy` writes it through go-keyring, which drives `/usr/bin/security`. The item's
access list therefore trusts `/usr/bin/security` and not tokeman, so the switch uses the same
tool. go-keyring keeps the secret off the command line by running `security -i` and writing the
command to its stdin; tokeman does the same:

```
/usr/bin/security -i            (stdin, one line, then EOF; bounded wait)
add-generic-password -U -s gemini -a antigravity -w go-keyring-base64:<base64(JSON)>
```

where JSON is `agy`'s own compact shape, in this field order:

```json
{"token":{"access_token":"<ya29…>","token_type":"Bearer","refresh_token":"<1//…>",
          "expiry":"<RFC 3339 with offset>","expires_in":3599},
 "auth_method":"consumer","id_token_email":"<the account's email>"}
```

What was done and seen (no `agy` CLI process was running at the start; Antigravity.app was, and
it does not use this slot):

1. Backup: the item's value (763 bytes with `security`'s trailing newline) and the credential
   files in `~/.gemini/antigravity-cli/` (`antigravity-oauth-token`, `settings.json`,
   `jetski_state.pbtxt`, `installation_id`), each read back and compared. The 7.2 GB of
   conversations and logs were not copied; a swap does not touch them.
2. Baseline: `agy models` is a quota-free identity check. It lists models and its log
   (`~/.gemini/antigravity-cli/cli.log`) says `OAuth: authenticated successfully as first@example.com`.
3. Swap: wrote `second@example.com`'s grant (from the app's `state.vscdb`, freshly refreshed) with
   the command above. `security` exited 0, the item read back equal, `cdat` was kept and `mdat`
   moved. `agy models` then logged `keyringAuth: loaded token`, `ChainedAuth: authenticated via
   keyring`, `OAuth: authenticated successfully as second@example.com`, and did not rewrite the item.
   Google's userinfo for the slot's access token said `second@example.com`, and
   `retrieveUserQuotaSummary` with it returned that account's four windows.
4. Restore: wrote the saved value back. The item compared equal to the backup byte for byte,
   and `agy models` logged `first@example.com` again without rewriting the item.
5. The same round trip through the built binary against a copied tokens.toml
   (`services use second@example.com`, `agy models`, `services use --previous`) gave the same
   results, and the item again equalled the original backup.

Findings:

- **A newly started `agy` uses whatever the slot holds.** No restart of anything else, no
  re-login, no prompt.
- **A running `agy` does not notice the swap.** A long-lived `agy acp` server signed in as
  `second@example.com` kept that account for 90 s of idling and through a new session after the
  slot went back to `first@example.com`; its log never printed `Stored token metadata changed
  externally`. tokeman therefore counts running `agy` processes after a switch and says they
  need a restart.
- **`agy` refreshes early and writes the slot when it does.** Started on an access token with
  143 s left, it refreshed at once (`token refreshed, new expiry=…`) and rewrote the item. So a
  slot value with a stale access token is fine, and the item changes under tokeman whenever
  `agy` refreshes; the backup is of the value at the moment of the switch.
- **The fallback token file.** `~/.gemini/antigravity-cli/antigravity-oauth-token` held a copy
  of the `cmrx64` grant from 2026-11-09. It was still there after the first swap and the first
  `agy models`, and gone after the `agy acp` run that refreshed and saved the other account's
  token; nothing in the log mentions it. It was put back from the backup. tokeman rewrites the
  file with the seated account's JSON when it exists and does not create it when it does not;
  with the file rewritten, `agy models` signed in from the Keychain as expected.
- The client is shared and the refresh token does not rotate, so seating a grant never
  invalidates tokeman's copy, and seating the other one back restores the first account.
- Not done, on purpose: running `agy` in file-storage mode (`SSH_TTY`). The binary contains
  `Failed to clear stale keyring credential after file fallback`, so such a run may delete the
  shared Keychain item.
- Switching is manual. The daemon does not seat accounts. Expect the cost of a Claude switch:
  server-side caches are per account.

**IDE (Antigravity.app): possible but not worth it.** Swapping means rewriting the protobuf in
`state.vscdb` while the app is closed, and the cached `userStatus` would still show the old email.
That app's state has not been touched since May. Treat the IDE grant as a source to import
`second@example.com` from, not as a slot to manage.

**Quota for both accounts needs no switching.** tokeman can hold both refresh tokens, refresh each
(stateless), and call `retrieveUserQuotaSummary` per account. Today's readings were taken this
way. **[live]**

## Prior art

- CodexBar `docs/antigravity.md` (https://github.com/steipete/CodexBar/blob/main/docs/antigravity.md):
  four sources (app LS, `agy` HTTPS, IDE LS, OAuth), multi-account through stored OAuth creds,
  client id/secret discovered from the app binary, hub UA, `retrieveUserQuotaSummary` preferred.
  Also issues #2427 (OAuth path shows fewer groups than the agy path, which matches the UA gating
  above) and #536 and #577 (label mapping).
- lbjlaq/Antigravity-Manager `src-tauri/src/modules/quota.rs`: endpoint fallback order Daily →
  `daily-cloudcode-pa.sandbox` → prod, `{"project":pid}` bodies, argon bucket notes.
  `src-tauri/src/constants.rs` has the UA builders. PR #3185 adds weekly via the summary endpoint.
- sylearn/AIUsage issue #81 and PR #82: the move from fetchAvailableModels to retrieveUserQuotaSummary.
- router-for-me/CLIProxyAPI issue #3876: summary UI with two groups × weekly + 5h.
- decolua/9router issue #3891: 3-minute cache and in-flight dedupe for the summary, free-tier
  detection via `paidTier.id`.
- IAMichan/Nook PR #917: response wrappers and the oneof `remaining`.

## Recipe for tokeman

1. Import grants: CLI Keychain item (`gemini`/`antigravity`) → `first@example.com`. IDE
   `state.vscdb` `oauthToken` → `second@example.com`. Store `{refresh_token, email}` per account.
2. Refresh as needed (form POST above, extracting the client id and secret from `agy`).
3. `loadCodeAssist` once per account to get `paidTier.id` and `cloudaicompanionProject`.
4. `retrieveUserQuotaSummary` with the hub User-Agent to get groups × {5h, weekly} →
   `Window`/`Bucket`. Floating reset with fraction 1 means the window has not started.
5. Optional: slot swap of the `agy` Keychain item to move the CLI between accounts.

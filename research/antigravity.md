# Google Antigravity: quota reading and account switching

Researched 2026-11-11 against `agy` 1.5.5 (`~/.local/bin/agy`, built 2026-11-10; the strings this
note relies on were re-checked in 1.5.6, and the switching experiment ran on 1.5.6) and
Antigravity.app 2.0.1 (`/Applications/Antigravity.app`, May 2026). Each fact is tagged:

- **[live]** verified with a real request on 2026-11-11
- **[code]** read from the installed binaries (strings / embedded proto descriptors) or local state
- **[web]** from docs or open-source projects (cited)

No tokens appear in this file. The two accounts are written `first@example.com` (the one `agy`
held) and `second@example.com`.

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
  Once the window is used, it is anchored (the first account's weekly anchor is 2026-11-18T08:38Z). Treat
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
it was assumed not to use this slot; that was wrong, see "Telling a running client" below):

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
- **A running `agy` does not notice the swap, and later undoes it.** A long-lived `agy acp`
  server signed in as `second@example.com` kept that account for 90 s of idling and through new
  sessions after the slot went back to `first@example.com`; its log never printed `Stored token
  metadata changed externally`. Its access token expired at 15:40. When its refresh went through
  at 15:54:19 (the network was down from about 15:32), it logged `token refreshed, new
  expiry=…16:54:18` and the slot then held `second@example.com` with exactly that expiry, modified
  at 15:54:19: the running process wrote its own account back over the other one. So a switch
  made while an `agy` runs lasts at most until that process refreshes, within the hour. tokeman
  counts running `agy` processes after a switch and says to quit them and switch again. After
  the test server was stopped (by its PID) the original value was written back, and `agy models`
  signed in as `first@example.com`.
- **`agy` refreshes early and writes the slot when it does.** Started on an access token with
  143 s left, it refreshed at once (`token refreshed, new expiry=…`) and rewrote the item. So a
  slot value with a stale access token is fine, and the item's bytes change whenever `agy`
  refreshes; the backup is of the value at the moment of the switch. (Two `agy` runs that were
  not part of this experiment, at 14:42 and 14:47, refreshed the original account's token the
  same way.)
- **The fallback token file is transient.** `~/.gemini/antigravity-cli/antigravity-oauth-token`
  held a copy of the first account's grant from 2026-11-09. `agy` deletes it when it refreshes and saves
  to the Keychain: it vanished after each of the three refreshes seen (14:40 and 15:54 by the
  test server, 15:55 by `agy models` on the original account) and survived the runs that did not
  refresh. Nothing in the log mentions it. It was put back from the backup twice; after the last
  refresh, by `agy` itself on the original account, it was left absent, which is the state `agy`
  chose. tokeman rewrites the file with the seated account's JSON when it exists and does not
  create it when it does not; with the file rewritten, `agy models` signed in from the Keychain.
- The client is shared and the refresh token does not rotate, so seating a grant never
  invalidates tokeman's copy, and seating the other one back restores the first account.
- File-storage mode (`SSH_TTY`) was not used in this experiment. "Side by side" below tested it
  afterwards: file mode never reaches the Keychain, and the delete behind `Failed to clear stale
  keyring credential after file fallback` happens only in keyring mode.
- Switching is manual. The daemon does not seat accounts. Expect the cost of a Claude switch:
  server-side caches are per account.

### Telling a running client about a new login **[live, 2026-11-11, `agy` 1.5.6, app 2.31.2]**

Question: can a running `agy` be told to re-read the slot, so a switch reaches live sessions?
Short answer: the Antigravity app can, an `agy acp` server can only through its own ACP client,
and an interactive `agy` cannot; it has to be quit and resumed.

The app in `/Applications` was replaced on 2026-11-11 by the 2.31.2 "hub" app. Its
`language_server --subclient_type hub` **shares the `gemini`/`antigravity` slot with `agy`** (it
wrote the slot at 15:57:30 with an extra `writer_id` field), so it is a third kind of running
client. The same backup and restore discipline was used; the slot ended byte-identical to the
backup on the first account, the app on the first account, and every process started for the
tests was stopped by its PID file.

**Two auth implementations in one binary [code].** `agy` in CLI mode uses `cli/backend/auth`
(`ChainedAuth` → `keyringAuth`, log lines `keyringAuth: loaded token`, `ChainedAuth: authenticated
via keyring`). The hub uses `language_server/auth_client.AuthClient`, which has
`syncWithStoredToken`, `hasExternalStorageChange`, `hasStoredMetadataChanged`, `hydrateStoredToken`,
`getWriterID`, `getLastSyncedTokenKey` and the messages `Stored token changed externally; hydrating
updated session from storage`, `Stored token metadata changed externally; updating in-memory auth
session`, `Stored token was removed externally; clearing in-memory auth session`, `Skipping
refreshed token save because stored token changed externally`. Those strings are the hub's. A
CLI-mode language server answers the hub-only RPCs with `interactive auth is only supported in
antigravity-hub mode and is unavailable in cli, standalone, or stubby-auth modes`. This is why the
earlier `agy acp` test never printed them and overwrote the slot at its refresh. There is no
sidecar or metadata file to touch: the "metadata" is inside the slot value (`writer_id`, the
token's identity), and nothing in CLI mode watches it.

**Control surfaces found**

| surface | hub app | `agy acp` | interactive `agy` |
|---|---|---|---|
| localhost ports (HTTPS and HTTP Connect RPC, `/exa.language_server_pb.LanguageServerService/<Method>`) | yes; ports and CSRF token in `~/.gemini/antigravity/daemon/hub_<port>.json` (0600) | yes, two ports, **no CSRF token needed**; found with `lsof -nP -p <pid>` | yes, two ports, CSRF required (`401 missing CSRF token`) and the token is not written anywhere |
| `GetUserStatus` (which account) | works with the CSRF header `x-codeium-csrf-token` | works | not reachable |
| `GetAuthStatus` | works, **and makes the hub re-read the slot** | works, read-only | not reachable |
| `AuthLogout`, `GetLoginSession`, `Login…` RPCs | hub-only (not called) | refused: hub mode only | not reachable |
| ACP `logout` / `authenticate` | n/a | works, but only from the ACP client that owns its stdio | n/a |
| `/logout` slash command | n/a | n/a | exists; there is no `/login`, `/account` or `/auth` |
| Unix sockets, named pipes, files under `~/.gemini/antigravity-cli`, `$TMPDIR` | none for control | none (one anonymous unix socket, pipes to children) | none |
| signals | none found | none found | none found (`SIGHUP` is only "terminal gone, shutting down") **[code]** |
| subcommands | n/a | `agy` has no `auth`, `login`, `logout`, `account`, `status` or `daemon` subcommand; `remote-control` manages an unrelated daemon | |

**What each client did**

- **Hub app: follows the slot when asked.** After the slot was swapped, `GetUserStatus` kept
  answering the old account for 90 s (15 polls). One `GetAuthStatus` call later it answered the
  new account. The 90 s wait was done once; on each of the other four swaps and restores the hub answered
  the slot's account right after a `GetAuthStatus`.
  It also survived the slot being deleted for under a second (below) and re-read it. Whether it
  follows on its own (focus, timer, refresh) was not seen in 90 s. Its refresh-save path has the
  `Skipping refreshed token save because stored token changed externally` guard **[code]**, so
  it should not write a stale account back the way `agy` does; not observed.
- **`agy acp`: ACP `logout`, then rewrite the slot, then ACP `authenticate`.**
  - `authenticate {"methodId":"oauth-personal"}` on a signed-in server returns success at once
    and re-reads nothing.
  - `logout` answers `{}`, logs `ACP Request received: logout`, and **deletes the Keychain item**.
    The server then reports `hasToken` empty and no user.
  - With the slot rewritten, the next `authenticate` logs `keyringAuth: loaded token`,
    `ChainedAuth: authenticated via keyring`, `OAuth: authenticated successfully as <the slot's
    account>` and the server serves that account (confirmed by `GetUserStatus`), with no browser.
    Done in both directions. `session/new` and `GetAuthStatus` alone did not trigger the re-read.
  - These are requests on the server's stdin. tokeman is not that client; an editor is. The
    tokenless local port does not help: `AuthLogout` there is refused in CLI mode.
- **Interactive `agy`: no.** It did not notice the swap. `/logout` logged `Logging out current
  user`, **deleted the Keychain item**, and showed `Select login method: 1. Google OAuth 2. Use a
  Google Cloud project`. With the slot rewritten, choosing Google OAuth went straight to the
  browser flow (`consumerOAuth: starting OAuth flow`) and ignored the slot. Leaving that screen
  with Ctrl-C logged `Logging out current user` again and **deleted the rewritten item a second
  time**. So `/logout` is not a way to switch, and a logged-out `agy` that is quit removes
  whatever is in the slot.
- Other things seen: something on this machine runs `agy -p /usage` from time to time (print
  mode, `silent auth`); each run signs in from the slot, so it follows a switch by itself.

**Quit and resume [code, flags from `agy help`; resuming across accounts not tested]**

- On exit `agy` prints `Resume with -c (or command below):` and `agy --conversation=<id>`.
  `agy -c` / `--continue` continues the most recent conversation, `--conversation <id>` a given
  one, and `/resume` switches inside the TUI. ACP has `session/load`.
- Conversations are local sqlite files, `~/.gemini/antigravity-cli/conversations/<id>.db`, with
  an index in `conversation_summaries.db` that has no account column, so a conversation started
  on one account should resume on the other. A test `agy acp` kept its sessions through a
  logout and a sign-in as the other account. Loading a conversation with real history under the
  other account was not tried (it would need a prompt).

**Recommendation for `tokeman agy use`**

1. Classify what is running before writing: the hub (`~/.gemini/antigravity/daemon/hub_*.json`
   with a live `pid`), `agy acp` processes, and other `agy` processes (interactive or print).
2. Hub: write the slot, then call `GetAuthStatus` on its `httpPort` with the CSRF token from the
   daemon file, and confirm with `GetUserStatus`. This switches the open app live. Treat a
   failure as a warning; the app's own save guard should keep it from undoing the switch.
3. `agy acp`: tokeman cannot switch it. Ask it which account it holds (`GetUserStatus` on its
   port needs no token) and, if it is the old one, say that the editor must restart its agent
   (or log out and in from the editor). Until then it will write the old account back at its
   next refresh.
4. Interactive `agy`: refuse by default while one runs, because it reverses the switch within
   the hour. Print, per process, its PID and working directory, and:
   `quit it (Ctrl-C twice; it prints "agy --conversation=<id>"), run this command again, then
   resume with "agy -c" or "agy --conversation=<id>"`. Offer `--force` to write anyway, with the
   warning that the switch lasts only until that process refreshes.
5. Never use `/logout` or ACP `logout` as part of a switch: both delete the slot, and for a
   moment every other client sees no login. If tokeman ever sees the slot empty right after its
   own write (a logged-out `agy` exiting), it should write again.
6. A slot value without `writer_id` was accepted by `agy`, by the hub, and by `agy acp`. tokeman
   need not write one.

Left behind by the tests: a few empty conversations in `~/.gemini/antigravity-cli/conversations/`
(from `session/new` on the test servers).

**The old IDE build of Antigravity.app (2.0.1, replaced on 2026-11-11 by the hub app above): possible but not worth it.** Swapping means rewriting the protobuf in
`state.vscdb` while the app is closed, and the cached `userStatus` would still show the old email.
That app's state has not been touched since May. Treat the IDE grant as a source to import
`second@example.com` from, not as a slot to manage.

**Quota for both accounts needs no switching.** tokeman can hold both refresh tokens, refresh each
(stateless), and call `retrieveUserQuotaSummary` per account. Today's readings were taken this
way. **[live]**

### Side by side: a second account in its own home **[live + code, 2026-11-11, `agy` 1.5.6]**

Question: can `agy` run as `second@example.com` while every other client stays on
`first@example.com`, without touching the shared slot? Yes. Built as `tokeman agy run`
(`src/providers/antigravity/run.rs`).

**How agy picks its token store [code, disassembled from the 1.5.6 binary].** Every token store in
agy is a `codeassistclient.compositeTokenStorage`, built by `NewCompositeTokenStorage` (callers: the
CLI backend, ACP auth, remote-control `SeedToken`, the feature provider, the hub's auth client). The
constructor sets a `useFileStatic` flag, and the store is then file-only for its whole life, when:

1. the caller asks for it (`Using file-based token storage because the caller requested it`);
2. `shouldBypassKeyring` reports an environment (`... because %s detected`). It runs five detectors
   in order in a goroutine: Android (`GOOS == "android"`), **SSH (`SSH_TTY`, `SSH_CLIENT` or
   `SSH_CONNECTION` non-empty)**, WSL, container (`/.dockerenv`, or `docker`/`containerd`/`kubepods`
   in `/proc/1/cgroup`), D-Bus (always false on macOS);
3. a keyring-timeout marker newer than one hour exists at
   `os.UserHomeDir()/.gemini/cache/<name>-keyring-unavailable` (`... because a keyring timeout was
   recorded within the last %v`).

With the flag set, `SaveToken`, `LoadToken`, `LoadStoredToken`, `UpdateStoredToken` and
`RemoveToken` (which is what `/logout` and ACP `logout` call) go straight to the file store,
`<gemini dir>/antigravity-cli/antigravity-oauth-token`; nothing reaches go-keyring. Every Keychain
delete in the binary (`removeKeyringQuietly`, which logs `Failed to clear stale keyring credential
after file fallback`, and `purgeTokenStorage`) sits in the keyring-mode branches: a keyring-mode
save when a file token exists (`saveTokenWithExistingFileState`), a failed keyring write
(`fallbackSaveTokenToFile`), and a migration. That keyring-mode save is the mechanism behind
antigravity-cli #1514 and its mirror image: a keyring-mode agy next to a file token deletes the
Keychain item.

**The detection has a 500 ms timer [code].** `shouldBypassKeyring` waits on the detector goroutine
with `time.NewTimer(500ms)`. If the timer wins, it returns false (`detection timeout`) and the store
is a **keyring** store. A process that is descheduled for half a second at startup (heavy load,
sleep) can therefore come up in keyring mode even with `SSH_TTY` set, and every store construction
runs the race again (an `agy acp` built two at `session/new`). `SSH_TTY` alone is a strong default,
not a guarantee.

**A HOME without `Library` hides the login keychain [live].** go-keyring drives `/usr/bin/security`,
and `security` finds the user's keychains through `$HOME/Library`: with `HOME` set to a directory
without `Library`, `security list-keychains` shows only `/Library/Keychains/System.keychain`,
`security default-keychain` finds none, and `find-generic-password -s gemini -a antigravity` exits
44 (not found). A keyring-mode agy in such a home cannot read, write or delete the slot.

**What was run.** The slot was backed up (839 bytes, read back equal), along with the small files in
`~/.gemini/antigravity-cli/` and `~/.gemini/`. The hub app was running and had written the slot
last (its value carries `writer_id`); no `agy` CLI process was running. The second account's token
file was minted from tokeman's stored grant in agy's own JSON shape. After every step the slot was
compared byte for byte with the backup and the real files were hashed.

| run (throwaway home) | storage | account | slot | real `~/.gemini` |
|---|---|---|---|---|
| `HOME` + `SSH_TTY`, fresh token, `agy models` | file (`SSH session detected`) | second | identical | unchanged |
| same, expired token (forced refresh) | file; `token refreshed`; the file was rewritten with the new expiry and the same refresh token | second | identical | unchanged |
| `HOME` + `SSH_TTY` only (no other variable) | file; agy also spawned its background updater once (the update check is per home) | second | identical | unchanged |
| `HOME` alone, fresh token | **keyring mode**, token loaded from the file because the keychain was unreachable | second | identical | unchanged |
| `HOME` alone, expired token | keyring mode; refreshed, then `Keyring SaveToken timed out after 5s` | second | identical | unchanged |
| real `HOME` + `SSH_TTY` + `--gemini_dir=<dir>` | file, everything under `<dir>` | second | identical | unchanged (no new log there) |
| `HOME` = links to every real-home entry but `.gemini` and `Library`, + `SSH_TTY`, expired token | file; refreshed | second | identical | unchanged; git saw the user's identity |

Interaction [live]: an `agy acp` server for the second account (file mode, own home, PID file) was
started with a token that expired 12 minutes later. While it ran, `agy models` in the real home
signed in as the first account from the Keychain (`ChainedAuth: authenticated via keyring`), and the
server's `GetUserStatus` answered the second account. At its expiry the idle server refreshed by
itself (`token refreshed`, 17:28:35) and rewrote only its own file (still 0600); the slot stayed
identical. Then, with that server and a `tokeman agy run` session (both second account) still
running, an interactive `agy` in the real home (not started by the test) refreshed the first
account at 17:52:22: the slot then held the same refresh token with the new expiry and a
`writer_id` (so CLI-mode agy writes `writer_id` too, not only the hub), and `agy models` in the
real home signed in as the first account. Both test sessions still answered as the second account.
Stopping them (by PID file) left the slot as it was and their token files in place.

**Findings**

- `SSH_TTY` set and a `HOME` of its own are both needed. `SSH_TTY` keeps agy away from the Keychain
  by design; the `HOME` without `Library` keeps it away when the 500 ms detection loses. `HOME`
  alone works, but in keyring mode: each save stalls 5 s on a keychain `security` cannot find, and
  whether a dialog appears was not checked.
- `--gemini_dir=<dir>` moves agy's whole data directory and the token file with it, and would let
  the user's `HOME` stay. It gives up the second guard, and the timeout marker cannot replace it: the
  marker is under the real `HOME`, so writing it would put the normal agy in file mode for an hour.
- A file-mode refresh rewrites the file with the same refresh token: Google does not rotate it, so
  tokeman's stored grant stays valid while a session runs, and a later `tokeman agy run` keeps the
  file agy refreshed.
- A per-account home lacks the user's dotfiles, which the commands agy runs need (git identity,
  ssh, toolchains). Linking every entry of the real home except `.gemini` and `Library` restores
  them and keeps the keychain hidden. Shared from the real `~/.gemini` by link: `GEMINI.md`, `skills`,
  `config/mcp_config.json`. `antigravity-cli/settings.json` is a copy instead **[live]**: agy saves
  it (a `/model` choice, a "trust this folder" answer) by replacing the file, which turned a link
  into a regular file and left the link's target untouched. A link would follow the real settings
  until the session's first save and then silently stop, so tokeman seeds a copy once and the
  session's model and trust choices stay its own. A first interactive run in a new home shows agy's
  onboarding again (theme, then the terms and the Interactions-data consent), because its status is
  `antigravity-cli/cache/onboarding.json` in that home. Not shared: the token file, conversations and
  `conversation_summaries.db`, history, logs, `installation_id`. So `agy -c` in a per-account home
  resumes that home's last conversation, not the normal one's.
- agy's background updater runs once per home (it writes `last_check.timestamp` there) and updates
  the one binary on `PATH`, as it would from the normal home.
- `ps -E` shows a process's environment to its owner, so a census can tell a per-account session
  (`HOME=` under tokeman's homes) from a client of the slot.

Left behind: the throwaway homes in the experiment's scratch directory. At the end the slot held
the first account with the backup's refresh token; its access token was the one the real-home
interactive agy refreshed at 17:52:22.

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

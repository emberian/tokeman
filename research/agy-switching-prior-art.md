# Antigravity / agy account switching: prior art

Researched 2026-11-11. Companion to `antigravity.md` (read its "Account switching" and "Telling a
running client" sections first). Tags:

- **[code]** read from the project's source at the commit date given, or from the local `agy` 1.5.6
  binary / `Antigravity.app` 2.31.2 (`strings`, `ps -o args`; nothing was run or written)
- **[doc]** a project's own documentation
- **[issue/discussion]** GitHub issues, HN
- **[inference]** reasoning from the above, not observed

Addresses are written `first@example.com` / `second@example.com`. No tokens are reproduced here.

## Summary

1. Every switcher found does what tokeman does: rewrite the `gemini`/`antigravity` Keychain item
   (or Windows `gemini:antigravity`, or Secret Service `service=gemini username=antigravity`).
   Several also write fallback files. None makes a running `agy` (interactive or `agy acp`)
   adopt a new account in place. They either refuse while `agy` runs, warn that it will write its
   old account back (Antigravity-Manager says exactly that), or kill it and relaunch it with
   `--conversation <id>` / `--continue`.
2. The "hot switch" tricks all target the **desktop app** or an **IDE**, not the CLI:
   - kill the app's `language_server` child; the Electron parent respawns it within about 2 s and
     the new one reads the slot (Antigravity-Manager);
   - call `LanguageServerService/Restart` from the app's renderer over CDP (omid-io);
   - in the older VS Code-fork IDE, an extension calls the host API
     `antigravityUnifiedStateSync.OAuthPreferences.setOAuthTokenInfo(...)` (Antigravity Cockpit).
     This is the only real in-process live switch found, and it exists only inside that IDE.
   tokeman's `GetAuthStatus` call to the hub is gentler than all of these. Nobody else was seen
   doing it.
3. The only way anyone changes the account of an already-running CLI is at the **network layer**:
   a TLS-intercepting proxy rewrites `Authorization: Bearer` on each request to cloudcode-pa
   (agy-account-rotator via `HTTPS_PROXY` set at launch; 9router via `/etc/hosts` plus a root CA in
   the System keychain). This changes which account pays for each request. It does not change the
   client's idea of who it is, and its own refresh still writes its old account back to the slot.
4. Running accounts **side by side** is possible today: a private `HOME` plus `SSH_TTY` set makes
   `agy` use file token storage under that `HOME` (CodexBar ships this; agy's own log line says
   `Using file-based token storage because SSH session detected`). The risk we flagged is real
   and is now an upstream bug: a keyring-mode `agy` that refreshes deletes the file token in the
   same `HOME` (antigravity-cli #1514, filed 2026-11-11). A private `HOME` per account avoids
   that, because it is a different file.
5. No official multi-account support exists. Requests for it are open (antigravity-cli #381,
   #1069, #1354). Google staff answers point to `HOME` isolation (#1080, macOS) and `USERPROFILE`
   (#1261, Windows), and say on Windows that the credential entry is **not** namespaced, so an
   isolated worker reuses the shared sign-in.

## (a) Projects and exactly how each switches

| project | targets | what it writes | process handling | live claim |
|---|---|---|---|---|
| **lbjlaq/Antigravity-Manager** (33k★) `src-tauri/src/modules/integration.rs`, last change 2026-11-04 (058ab0cf) [code] | `agy`, Antigravity.app ("classic"/"Standalone"), "Antigravity IDE.app" | Keychain via `security delete-generic-password` then `add-generic-password … -w <value> -A` (**secret on argv; `-A` = any app may read**). Same JSON shape as agy (`token{access_token,token_type,refresh_token,expiry(RFC3339 µs, Z),expires_in:3599}`, `auth_method:"consumer"`, `id_token_email`, plus `id_token`). Also `~/.gemini/jetski-standalone-oauth-token`, `~/.gemini/antigravity-cli/antigravity-oauth-token` (agy target only, since #3728), `~/.gemini/oauth_creds.json` and `~/.gemini/google_accounts.json` (`{"active":email,"old":[]}`, Gemini-CLI files). IDE/legacy: `state.vscdb` `antigravityUnifiedStateSync.oauthToken` (replaces the `oauthTokenInfoSentinelKey` entry), `…userStatus` (`userStatusSentinelKey`), `…enterprisePreferences`, `telemetry.serviceMachineId`, deletes `jetskiStateSync.agentManagerInitState`; device profile into `storage.json`. | agy: writes, reads back and compares the refresh token, then only warns: "Running agy sessions may still use and write back their previous credentials". Classic app running: write, `kill -9` the `language_server` descendants of the main process, write again, wait up to 15 s for respawn, else full restart (issues #3503, #3637). IDE: SIGTERM main process, write, relaunch with the captured argv. | hot switch for the app only; none for agy |
| **jlcodes99/cockpit-tools** (20k★) `src-tauri/src/modules/antigravity_shared_switch{,_hub}.rs`, `antigravity_credential.rs`, 2026-11-05 (cc599042) [code] | Antigravity desktop, IDE, the IDE's agy hub | `security add-generic-password -U -s gemini -a antigravity -w <value>` (argv; deliberately no `-A`/`-T`, which would prompt on every switch); payload without `id_token_email`/`expires_in`. Same JSON to `~/.gemini/jetski-standalone-oauth-token` (atomic) and to `~/.gemini/antigravity-cli/antigravity-oauth-token` if that dir exists. | Closes the desktop app and IDE, writes, relaunches. On macOS also finds `~/.gemini/bin/agy --hub --app_data_dir antigravity` processes (Antigravity IDE 2.6+ runs its hub this way), SIGTERMs each, verifying uid, exe and start time, and respawns it with the same argv, waiting for a LISTEN socket via `lsof`. | no |
| **jlcodes99/vscode-antigravity-cockpit** (4.8k★) `src/services/accountSwitchService.ts`, 2026-07-08 [code] | old VS Code-fork Antigravity IDE | Calls `vscode.antigravityUnifiedStateSync.OAuthPreferences.setOAuthTokenInfo({accessToken, refreshToken, expiryDateSeconds, tokenType:"Bearer", isGcpTos:false})`, or legacy `vscode.antigravityAuth.setOAuthTokenInfo`, then `getOAuthTokenInfo()` to read back. cockpit-tools triggers it over `ws://127.0.0.1:19528` with `event.plugin_switch_account` / `switch_mode:"seamless"`. | none | **yes: "seamless"**, in-process, IDE only. The hub app's `app.asar` (2.31.2) has no `setOAuthTokenInfo` [code]. |
| **omid-io/antigravity-account-switcher** `src/sidecar/index.js`, `cdp-bridge.js`, 2026-11-06 [code] | Antigravity 2.x app (Windows) | Windows Credential Manager `gemini:antigravity`. | Connects to the app's DevTools port (`%APPDATA%/Antigravity/DevToolsActivePort`), and in the renderer runs `fetch('https://127.0.0.1:'+location.port+'/exa.language_server_pb.LanguageServerService/Restart', {headers:{'x-codeium-csrf-token': window.__APP_CONFIG__.csrfToken}})`, then `location.reload()`. | "native in-app" switch: LS restart plus window reload |
| **ooks37/agy-accounts** `scripts/account_manager.py` [code] | agy on Windows (an agy plugin, `!zh`) | `CredWriteW("gemini:antigravity")`, plus `~/.gemini/oauth_creds.json`. Pings cockpit-tools' websocket. | "reload": reads the last `conversationId` from `~/.gemini/antigravity-cli/history.jsonl`, `Stop-Process -Force` on agy, then opens a new Windows Terminal tab with `agy --conversation <id>` (or `--continue`). | no, but automates quit and resume |
| **adalenv/zerog** `lib/runner.js`, `lib/account-manager.js` [code] | agy | `security` write/delete on `gemini`/`antigravity`; captures accounts by deleting the slot and running `agy --print hi` to force a login. | Wraps agy as a child. On a quota pattern in its output it SIGTERMs it, swaps the slot, and respawns with `--continue`. | "zero-downtime" only in the restart-and-continue sense |
| **MiloudiMohamed/agy-account-rotator** `internal/proxy/{proxy,spawn}.go`, `cmd/agy-rotator/main.go`, 2026-08-28 [code] | agy | Writes `~/.gemini/antigravity-cli/antigravity-oauth-token`, rotates per launch. | A shim `agy` exports `HTTP(S)_PROXY=127.0.0.1:8999`, `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, `GRPC_DEFAULT_SSL_ROOTS_FILE_PATH` and execs the real agy. The proxy MITMs `*cloudcode*` and `oauth2.googleapis.com`, sets `Authorization: Bearer <vaulted account>` on every request, and retries on 429/529 with the next account. | **per request, for processes it launched** |
| **decolua/9router** `src/mitm/{dns/dnsConfig.js,cert/install.js,handlers/antigravity.js}` [code] | Antigravity app and IDE traffic | `/etc/hosts` entries for the cloudcode hosts, root CA via `security add-trusted-cert -k /Library/Keychains/System.keychain`. | Routes generateContent to its own router (other providers or accounts). | system-wide, affects running clients |
| **WellWells/agentswap** (Show HN 2026-10-09) `internal/antigravity/{antigravity,keyring}.go`, 2026-11-10 [code] | agy | `security -i` with `add-generic-password -U -s gemini -a antigravity -w go-keyring-base64:…` on stdin (same as tokeman). | Refuses: `agy is running; close every agy process before switching accounts` (`pgrep -x agy`). | no |
| **tnduyh5/agy-tools** `bin/agy-account` [code] | agy, macOS | Profiles stored as Keychain items `gemini-profile`/`<name>`. Switch: `security delete-generic-password`, then `add-generic-password -U -w` with the value fed twice on stdin (keeps it off argv). | Prints "restart any running agy session (token is read at startup)". | no |
| **m4tinbeigi-official/antigravity-account-switcher** `switcher_core.py`, `editor-extension/extension.js` [code] | Antigravity.app / IDE, macOS | Keychain (`security` or python `keyring.set_password('gemini','antigravity',…)`). | `pkill -9 -f …/Contents/MacOS/Antigravity`, `pkill -9 -f …language_server`, then `open` the app. | no |
| **Andreusd/agy-account** `agy_account.py` [code] | agy, Windows/Linux | `CredWriteW` / Secret Service; writes `google_accounts.json`. | none | no |
| **Surya54p/antigravity-multi-account-usage-token-info** `run_antigravity_5x.fish` [code] | agy, Linux | none shared | **Side by side**: each tab runs `HOME=~/agy_profileN`, its own `gnome-keyring-daemon` under `dbus-run-session`. | n/a (5 concurrent accounts) |
| **steipete/CodexBar** `docs/antigravity.md`, 2026-11-06 [doc] | quota reading, not switching | For account-scoped `agy -p` runs: temporary `0700` `HOME` with a `0600` token file, env reduced to PATH/locale/tmp/proxy, **`SSH_TTY` set to select file storage**, `AGY_CLI_DISABLE_AUTO_UPDATE=true` (lowercase `true` required). Says this leaves "the ambient CLI login and Keychain untouched". Also reuses an already-running same-user `agy` server, but notes 1.2.2+ answers `401 missing CSRF token` and "do[es] not expose the generated token". | n/a | n/a |
| **router-for-me/CLIProxyAPI** `internal/auth/antigravity/filename.go` [code] | its own proxy | Per-account files `antigravity-<email>.json`; it is the client, so there is no switch. | n/a | n/a |
| Besty0728/Gemini-CLI-Auth-Manager, RyanWeb31110/gemini-multi [doc] | Gemini CLI | Swap `~/.gemini/oauth_creds.json` + `google_accounts.json`, or symlink `~/.gemini` to per-account dirs. | Restart. | no |

Not looked at in source (README only, or not reached): Muriel-Gasparini, ChillingWombat/AntigravitySwissKnife
(has `.rpiv/artifacts/research/language-server-api.md`, possibly a useful RPC catalogue),
at2008/antigravity-cockpit, BoyGR extension, MonchiLin/antigravity-agent, Doomd, Ximeeek, Nook,
AIUsage (the last two are quota readers, already in `antigravity.md`).

## (b) Making a RUNNING agy adopt a new account

| method | applies to | evidence | verdict |
|---|---|---|---|
| Rewrite slot, then wait | interactive, acp | ours [live]; Antigravity-Manager's warning text [code]; tnduyh5, agentswap [code] | does not work; the process writes its old account back at its next refresh |
| Kill + relaunch with `--conversation <id>` / `--continue` | interactive | ooks37 (`history.jsonl` → `conversationId`), zerog (`--continue`) [code] | works, as a restart. The `history.jsonl` lookup is a lead for tokeman's "resume with" message (not read on this machine). |
| TLS MITM proxy rewriting `Authorization` | any process whose traffic it sees | agy-account-rotator (env set at launch), 9router (hosts + system CA) [code] | changes the paying account per request without a restart, **only if the proxy was in place when the process started** (env), or system-wide (hosts + trusted root). The process keeps its own identity, quota UI and refresh; it keeps writing its own account to the slot. On macOS, Go's `crypto/x509` uses the system verifier, so `SSL_CERT_FILE` probably has no effect and the CA would have to be trusted in a keychain [inference]. agy has `AGY_CA_CERTS`, `AGY_PROXY_STRICT_SSL` and `DETECT_AND_USE_PROXY_*` strings [code], untested. Heavy, and very close to what HN describes Google acting against (below). |
| `LanguageServerService/Restart` | hub app (omid-io); the agy binary has `language_server.(*Server).Restart` and logs `Restart requested on language server process %d`, `Restarting language server...`, `Language server shutting down` [code] | omid-io [code], untested by us | lead. For the hub it probably does the same as killing the LS child, which `GetAuthStatus` already makes unnecessary. For `agy acp` (tokenless port) the LS is in-process, so it would likely end the whole server [inference]. **Do not try it against a live session.** |
| `setOAuthTokenInfo` host API | old VS Code-fork IDE only | vscode-antigravity-cockpit [code] | real live switch, not available in agy or the 2.31 hub app |
| ACP `logout` / `authenticate` | `agy acp` | ours [live] | works, but only from the ACP client (the editor) |

Nothing found makes an interactive `agy` or an `agy acp` re-read the slot without its own client's
cooperation. Our conclusion in `antigravity.md` stands.

## (c) Side-by-side (per-process account pinning)

- **`HOME=<private dir>` + `SSH_TTY=<anything>`** → file storage at
  `$HOME/.gemini/antigravity-cli/antigravity-oauth-token`.
  - agy strings: `Using file-based token storage because %s detected`, `… because the caller
    requested it`, `… because a keyring timeout was recorded within the last %v` (a keyring
    "marker file" records timeouts) [code].
  - The hub log in issue #868 shows `Using file-based token storage because SSH session detected`
    [issue].
  - CodexBar uses this combination on macOS and documents that it does not touch the Keychain [doc].
  - Seed the file with the agy JSON for the account. Refresh tokens do not rotate, so tokeman's
    copy stays valid.
  - **Risk, confirmed upstream**: antigravity-cli #1514 (2026-11-11, Linux, 1.5.5). A keyring-mode
    `agy models` that refreshed its token `unlinkat`'d the still-valid SSH file token in the same
    `HOME` [issue]. Our `antigravity.md` saw the same deletion on macOS. With a private `HOME`, the
    keyring-mode processes delete their own `HOME`'s file, not the worker's.
  - The reverse risk (a file-mode process deleting the Keychain item: `Failed to clear stale
    keyring credential after file fallback`) is untested. The string suggests it happens on the
    *timeout* fallback path, not the SSH-detected path [inference]. CodexBar's claim covers the
    SSH path only.
- **`HOME` alone on macOS** is not enough. Staff advice in #1080 (macOS 1.2.7) says to isolate
  `HOME` and sign in there [issue], but on Windows staff say in #1261 that the
  `gemini:antigravity` credential "is not namespaced by profile… your worker will automatically
  reuse the existing sign-in, so do not run /logout or sign into another account from the worker"
  [issue]. On macOS the login keychain is per user, not per `$HOME` [inference], so the slot is
  shared there too unless file storage is forced.
- **Linux**: a private `HOME` plus a private Secret Service (`dbus-run-session` +
  `gnome-keyring-daemon`) per account (Surya54p) [code]. With no D-Bus session bus agy uses file
  storage by itself (1.x changelog: "bypasses the keyring when no D-Bus session bus is present")
  [code].
- **`AGY_ADC_AUTH`** (Application Default Credentials) is a per-process credential, but the
  changelog treats it as the *Business / Google Cloud* sign-in (telemetry forced off, "Business
  accounts (such as Google Cloud project and Application Default Credentials sign-in via
  `AGY_ADC_AUTH`)") [code]. It would not use a consumer Ultra subscription [inference].
- **`--app_data_dir` / `--gemini_dir` / `--cli_dir_name`** exist as flags (`--app_data_dir=antigravity`
  is used by the IDE hub; #868 shows `agy --app_data_dir=antigravity models`) [code/issue]. They
  move the data directory. Whether the keyring account name follows `app_data_dir` is unknown. The
  CLI's dir is `antigravity-cli` while its keyring account is `antigravity`, so probably not
  [inference]. The separate `agy_acp_server` product "deliberately distinguishes ACP keychain
  accounts" and keeps `antigravity-acp/acp_token.json` (#1354) [issue], so per-product slots do
  exist elsewhere.
- No `GEMINI_CLI_HOME`, `GEMINI_FORCE_FILE_STORAGE`, `GEMINI_CONFIG_DIR`, `ANTIGRAVITY_HOME` or
  `--profile` in agy 1.5.6 [code]. The first two are Gemini CLI's (`packages/cli/index.ts`,
  `packages/core/src/services/keychainService.ts`). `AGY_ACCOUNT` is unrelated: it selects a
  *plugin* account for sandboxed commands.

## (d) RPCs and mechanisms we did not know about

- **Interactive agy hands its LS address and a CSRF token to its own children.** The embedded
  plugin-manager skill says: "The address and CSRF token are normally already in your shell
  environment", using `ANTIGRAVITY_LS_ADDRESS` (default `localhost:5387`) and
  `ANTIGRAVITY_CSRF_TOKEN`. Sidecars get them plus `ANTIGRAVITY_SIDECAR_WEB_PORT`,
  `ANTIGRAVITY_EXECUTABLE_DATA_DIR`, `ANTIGRAVITY_CONVERSATION_ID` [code]. So a command run
  *inside* agy (`!cmd`, a hook, a sidecar) can call that agy's LS. There is a flag
  `enable-scoped-csrf-tokens: true`, and some RPCs are "restricted in run_command"
  (`JetboxWriteState`, `DeletePlugin`, `InstallCustomization`), so the token is scoped [code].
  CLI mode refuses the auth RPCs anyway, which limits this to `GetUserStatus` and the like.
  Reading it from outside via `ps -E` of an agy child was not tried [inference].
- **The hub LS's CSRF token is on its argv**: `language_server --standalone --subclient_type hub …
  --csrf_token <t> --app_data_dir antigravity … --enable_sidecars --host_bridge_url=http://127.0.0.1:<p>
  --host_bridge_token=<t>`. So the Codeium/Windsurf `ps` trick works for the hub [code, ps on this
  machine]. It does not apply to interactive agy: there the LS is in-process and nothing on argv
  carries the token. The daemon file is the cleaner source.
- **HostBridgeService** (`exa.host_bridge_pb`: `GetUpdateStatus`, `ApplyUpdate`, **`GetOAuthToken`**).
  The LS side has `language_server.connectHostBridge` and
  `codeassistclient.HostBridgeOAuthTokenProvider`. agy has `main.cliHostBridge` (a server). Flags:
  `--host_bridge_url` + `--host_bridge_token` (or the token on stdin: `Failed to read host bridge
  token from stdin`) [code]. The hub app's JS host bridge implements only the update RPCs [code].
  This suggests an LS can be told to *ask its host* for OAuth tokens, which would be a clean
  injection point for an LS that tokeman launches itself. It does nothing for an agy already
  running.
- **More auth RPCs on `LanguageServerService`** (beyond GetAuthStatus/GetUserStatus/AuthLogout/
  GetLoginSession): `Login`, `LoginWithBrowser`, `CancelLogin`, `SubmitLoginAuthCode`,
  `SwitchLoginToAuthCode`, `HasAuthToken`, `FetchUserInfo`, `GetLocalUserInfo`, `SetUserInfo`,
  `RegisterGdmUser`, `SelfAssignLicense`, `Restart`, `ResetOnboarding`,
  `JetboxSubscribeToOAuthState`. `ExtensionServerService` has `GetSecretValue`,
  `StoreSecretValue`, `RestartUserStatusUpdater` [code]. There is **no** `SetAuthToken`,
  `SaveOAuthTokenInfo`, `RegisterOAuthToken`, `UpdateAuth`, `ReloadAuth`, `SwitchAccount` or
  `RefreshUserStatus` on any service (the one `SetAuthToken` string is clearcut logging) [code].
  The token setter that exists is the IDE's JS host API `setOAuthTokenInfo`, not an RPC.
- agy CLI log `Rebinding LanguageServer backend for auth change (mode=%s -> %s)` [code]: the CLI
  can rebind on an in-process auth change (its own `/login`), not on an external one.
- `~/.gemini/jetski-standalone-oauth-token`: the desktop app's file fallback for its
  `compositeTokenStorage` (cockpit-tools comment, "Standalone 2.13.0 / 2.25.0") [code]. If it
  exists here, tokeman should keep it in step with the slot, as cockpit-tools does, or a keyring
  timeout could bring back an old account [inference].
- `~/.gemini/antigravity-cli/history.jsonl` lines carry `conversationId` (ooks37) [code]. This is
  the source for "resume with `agy --conversation <id>`".
- `AGY_CLI_DISABLE_AUTO_UPDATE=true` for any agy tokeman spawns (CodexBar) [doc].
- **ToS signal**: HN 46998972 (2026-02-13): "Google is dealing with a wave of abuse over its
  Antigravity IDE, with 'account switching' tools designed to use a ton (20+) of free or pro
  accounts… banning clients that seem to be accessing 'private' APIs outside of a Google
  product" [discussion]. HN 47118849: the Cockpit extension stored refresh tokens in plaintext
  [discussion]. Antigravity-Manager's `-A` flag (any app can read the item) is a similar
  weakening that tokeman should not copy.
- Quota display after a switch: #1085 / #1491. agy's quota manager throttles refreshes
  (`doRefreshQuota: skipped (throttled)`), and accounts in one Google One family group report the
  same quota [issue]. This is relevant if tokeman ever compares agy's `/usage` with its own
  readings.

## (e) Looked for and not found

- Any reload signal, socket or RPC that makes CLI-mode agy re-read the slot. None in agy 1.5.6
  strings or in any project.
- An auth/login/account subcommand. Still none (#1069 asks for `agy whoami`; staff in #1080 say
  `HOME=… agy login`, but no `login` subcommand appears in our `agy help`) [issue/inference].
- Per-process keyring service or account override (env or flag) in agy. None.
- Official multi-account or profile support. Only open requests (#381, #1069, #1354, #1476) and
  workaround advice (#1080, #1261).
- Reddit (popular-comments corpus): no matching discussion of Antigravity account switching.
- A project that calls the hub's `GetAuthStatus` after a slot write, as tokeman does. None. Every
  app-side switcher restarts the LS or the window instead.
- The ChillingWombat LS API notes and other READMEs were not read in source. Read
  `ChillingWombat/AntigravitySwissKnife/.rpiv/artifacts/research/language-server-api.md` before
  testing any new RPC.

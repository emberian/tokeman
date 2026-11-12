# Test audit, tokeman 0.4.0

Audit of every unit test on `rewrite` (base 0248920, then 7f69918 and c93d6fb as they merged), by inventory,
mutation of the code each test names, and repair. Four groups of modules were audited in parallel, each in its own
clone; their sections follow this summary unabridged. Classes: (a) pins behaviour that matters, (b) meaningful but
weak, (c) hollow, (d) redundant.

## Summary

| group | modules | tests before | a | b | c | d | deleted | strengthened | added | tests after |
|---|---|---|---|---|---|---|---|---|---|---|
| g1 | policy, quota, forecast, settings, util, observe, attention, store | 107 | 73 | 34 | 0 | 0 | 0 | 30 | 16 | 123 |
| g2 | account, anthropic, claude, claims, history, fs, launch | 94 | 76 | 14 | 3 | 1 | 2 (+6 hollow tails cut) | 19 | 6 | 97 |
| g3 | codex, providers | 116 | 95 | 18 | 2 | 0 | 0 | 16 | 13 | 129 |
| g4 | commands, daemon, ui, cli, drift | 102 | 85 | 13 | 1 | 3 | 3 | 13 | 11 | 110 |
| root | paths | 0 | | | | | | | 1 | 1 |
| **all** | | **419** | **329** | **79** | **6** | **4** | **5** | **78** | **47** | **460** |

"Before" counts each group's files on the merged tree it audited (the Codex follow-up and `compact` added tests while
the audit ran; the three feature-gated egui tray tests went with the tray). After: 439 (a) + the paths test, 17 (b),
2 (c), 0 (d). The 17 weak tests left are small display helpers (chart, TUI, text), one parse smoke over the command
tree, one doctor control and two provider tests; each is listed in its group's inventory with the reason. The 2 hollow
ones kept assert external contracts no other test names (the Codex control-socket path, the consume-code spellings).
`cargo test --lib` on the merged lane: 459 passed, 2 ignored (one live-network provider test, one child-process helper).

**One test in five was weak.** The suite was not tautological in the way feared (6 hollow tests and a handful of
serde round-trip tails in 419), but it was shallower than its names: 757 mutations were run, and of the 664 run
against the tests as they stood, 168 survived (25%). The survivors were concentrated in what matters most: the
rotation decision (`force`, three of the switch triggers, the landing floors), the notified-once ledger, viewer mode,
error paths of every provider, file modes, and the transcript scan. 147 of the 168 are now killed by a strengthened or
new test, each seen to fail under its mutation; 21 are left, each named in the group tables (equivalent mutants,
cosmetic output, or behaviour with no seam).

### Mutation totals

| group | mutations run | first pass | survived first pass | now killed | left alive |
|---|---|---|---|---|---|
| g1 | 231 | 231 | 84 | 75 | 9 (7 equivalent or nothing to pin, 2 residual) |
| g2 | 204 | 158 | 33 | 25 | 8 (3 equivalent, 5 unreachable or display-only) |
| g3 | 184 | 137 | 32 | 30 | 2 (one indistinguishable in the server's answer, one equivalent) |
| g4 | 138 | 138 | 19 | 17 | 2 (cosmetic) |
| **all** | **757** | **664** | **168** | **147** | **21** |

### Bugs and hazards found

Tests that touched the machine they ran on (all closed):

1. **A test wrote the live profile cache.** `anthropic::probe::tests::a_bare_429_...` did a read-modify-write of
   `~/.config/tokeman/claude-profile-reads.json` through the default path on every `cargo test`, leaving an entry
   `l|usage` in the daemon's cache. Found by running the suite with HOME and XDG_* pointed at an empty directory.
   The entry is still in the live file on the machine the audit ran on; the audit did not touch that file.
2. **Three `daemon::codex` tests swept the real `~/.codex` with `dry_run = false`.** With a Codex running there with
   a control socket and below its floor, `cargo test` would have pushed a fixture token into it. On the audit machine
   the socket was a dangling link and every run answered "no socket".
3. **Every test that built an Anthropic client ran the installed `claude --version`.**
4. Thirteen more tests resolved a real home, config or data location without reading or writing it.

   Closed structurally: under `cfg(test)`, `paths::home()` is a directory that does not exist and the XDG and
   `CLAUDE_SETTINGS` variables are not read; no `dirs::` call exists outside `src/paths.rs`; the daemon's mover is
   given its homes; the profile cache is injected; the `claude` lookup is skipped under test. Tests pin each.

Bugs in the product, fixed:

5. **The default chart limit flipped with the account read last** in a fleet mixing 5-hour and weekly-only accounts.
   It is now the shortest account-wide window in the history. (A history of scoped limits only still takes the newest
   reading's first limit.)
6. **`tokeman tray` was removed without a `cli::REMOVED` row**, so it answered with clap's error instead of naming
   what replaced it.
7. **Credential-holding types derived `Debug` with the secret in it** (`Login`, `Grant`, `Account`, `Credential`,
   `TokenResponse`, `Pkce`, `LoginBackup`, a secret `EnvVar`, `ServiceAccount`, `TokenBundle`, `AuthFile`, agy's
   `Slot`, `Backup`, `Prepared`, `CachedToken`). Nothing printed one, but one `{:?}` would have.
8. `agy use` built a new HTTP client for every loopback call.

Found and not fixed (each is a decision, not a slip):

9. `seat::ClientReport` serializes two `account` keys; no surface prints it as JSON today.
10. Kimi has no fallback to its legacy usage shape, which `research/zai-kimi.md` recommends; a legacy-only answer
    reads as an error.
11. A Codex usage answer with no `rate_limit`, including an HTTP-200 error envelope, is shown as "ok" with no
    windows. It is never a landing (pinned), but the word is wrong.
12. `Store::insert` writes no closing row when reads stop mid-hold and resume with the same value. Judged within
    the growth rule (the same value on both sides of a gap is one hold) and pinned as such; whether a chart should
    break the line across such a gap is open.
13. A paused daemon holds `daemon.lock` and surfaces still read every account themselves
    (`Monitor::Paused.keeps_fresh()` is false). DESIGN.md says so; the new viewer test pins it. If a paused daemon
    should make surfaces viewers too, the test and `keeps_fresh` change together.
14. A reset message's named time zone is ignored and the machine's local zone used (unpinned either way).

### Nothing spends a reset by itself

`anthropic::resets::claim` and `codex::resets::claim` each have one caller, the `reset` command. Pinned three ways:
behaviourally on the Codex mover (daemon and `codex watch`) and the half-hour sweep, with credits on offer and no
consume request allowed; a CLI test that `reset` needs confirmation and `--clear` cannot ride with a claim; and one
structural test over the source for the rotation cycle and `launch`, which talk to the live API and cannot be run.

### What is still not tested and matters

- **`daemon::cycle::run` as a whole**: paused, not due, dry run writes nothing, backoff only for accounts asked,
  a switch installs. It has no seam for its client, its paths or the Keychain. Its decisions (`policy::decide`,
  cadence) are tested; the wiring is not. This is the largest gap.
- **`daemon::refresh::refresh_where`**, where a refresh token is redeemed: its two halves are tested (the locked
  read-modify-write, the request sent once), the sweep is not.
- `keychain::suppress` / `restore` orchestration (real Keychain); `doctor::gather`; the `Fleet::update` side of the
  commands; `main.rs` dispatch (binary crate; only the confirmation gate is pinned, by reading the source).
- `observe::record_session_from_hook` and `scan_transcripts` end to end; `observe::apply` and `active_limits`
  (clock and state file). Their pure steps are tested.
- `providers::read_all`'s timeout branch; `probe_all`'s "first probe alone" ordering; `launch::run` removing an
  inherited `CLAUDE_CODE_OAUTH_TOKEN`; `resets::status` when one of its two reads fails; a forecast over a window
  whose reset passed since the read; the free-space check of `history compact`.
- The purity rules of `policy.rs` (no clock, never matches on `Service`) are structural and have no test.
- Two notifier mutations were not run because the suite would have posted a real desktop notification under them.
- Codex refresh has no lock (a design decision, unlike the Claude side's locked write).

### Hygiene of the suite

- Full suite, three runs on the final tree: 459 passed, 2 ignored each time, 8.2 to 8.5 s wall in parallel, 27 s
  single-threaded. Ten earlier full runs during the audit: no flake.
- Time zone: run under `Pacific/Kiritimati`, `America/Los_Angeles` and `Asia/Kolkata` with no difference.
- No test sets or removes an environment variable. Wall-clock reads in tests are relative ("now", "now + 8 days").
- Run with HOME and XDG_* pointed at an empty directory, the final suite creates nothing there.
- Run in a sandbox with `security`, `osascript`, `launchctl`, `ps`, `lsof` and non-loopback network denied, the
  suite passes; the tests that exercise those run shell-script fakes from their own temporary directory.
- Slowest tests: the interrupted-compaction test 2.4 s; two tests whose subject is a 2 s bound 2.0 s each; the rest
  under 1 s. The Codex challenge-retry tests went from 2.7 s to under 0.3 s and the agy hub tests from 3.8 s to 0.7 s.
- The child-process compaction test's wait on its child is not bounded.


---

## g1: policy, quota, forecast, settings, util, observe, attention, store (store.rs, store/compact.rs, store/legacy.rs)

Branch `lane/tests-g1` (clone `scratchpad/g1`), on top of lane/tests e7e1973 (rewrite c93d6fb with the Codex follow-up and
`compact` merged, plus the paths seam). Six commits, c15b056..9877e5e. "Before" is the merged code, not 0248920: the
follow-up added one attention test and `compact` replaced store's nine tests with seven and added thirteen in compact.rs.

No bug found in these modules. No test was hollow or redundant; 34 of 107 were weak in the specific sense that a
mutation of the rule they name passed them. 231 mutations were run: 145 were killed by the tests as they stood, 2 did not compile, 84
survived, 75 of those are killed after the repairs, 9 are left (7 equivalent, 2 residual: F11, C22).

### Counts

| file | before | a | b | c | d | deleted | strengthened | added | after |
|---|---|---|---|---|---|---|---|---|---|
| policy.rs | 26 | 20 | 6 | 0 | 0 | 0 | 5 | 3 | 29 |
| quota.rs | 21 | 13 | 8 | 0 | 0 | 0 | 8 | 1 | 22 |
| forecast.rs | 11 | 6 | 5 | 0 | 0 | 0 | 3 | 2 | 13 |
| settings.rs | 4 | 3 | 1 | 0 | 0 | 0 | 3 | 0 | 4 |
| util.rs | 4 | 2 | 2 | 0 | 0 | 0 | 2 | 0 | 4 |
| observe.rs | 14 | 7 | 7 | 0 | 0 | 0 | 7 | 5 | 19 |
| attention.rs | 8 | 7 | 1 | 0 | 0 | 0 | 1 | 0 | 8 |
| store.rs | 7 | 4 | 3 | 0 | 0 | 0 | 0 | 3 | 10 |
| store/compact.rs | 12 (+1 child-process helper, `#[ignore]`) | 11 | 1 | 0 | 0 | 0 | 1 | 2 | 14 (+1) |
| store/legacy.rs | 0 | | | | | | | | 0 (tested through compact.rs) |
| **total** | **107 (+1)** | **73** | **34** | **0** | **0** | **0** | **30** | **16** | **123 (+1)** |

After: 122 class a, 1 class b (`forecast::a_window_that_is_not_running_starts_when_it_is_first_spent`, see F20).

### Inventory

Class is the class BEFORE repair. Action: `-` none, `S` strengthened, mutation ids in brackets.

| file | test | claim | class | action |
|---|---|---|---|---|
| policy | exact_normal_floor_is_not_viable | exactly at a normal floor is not viable | a | - |
| policy | known_opus_floor_is_part_of_shared_default_viability | a model limit at the weekly floor gates that model | a | - |
| policy | exhausted_opus_48_bucket_does_not_poison_opus_5_or_fable | Opus 4.8 wall leaves `opus` and Fable viable | a | - |
| policy | the_fable_limit_gates_fable_sessions_only | family-wide Fable limit gates Fable only | a | - |
| policy | the_opus_alias_obeys_the_opus_5_bucket | `opus[1m]` is limited by the Opus 5 limit | a | - |
| policy | unrelated_family_bucket_does_not_affect_selection | Sonnet wall does not gate Opus | a | - |
| policy | sip_mode_unlocks_the_reserve_band | no account above normal floors: sip, best is above sip floors | a | - |
| policy | exhausted_fleet_stays_in_fast_sip_mode | an exhausted fleet is sip and sampled fast | b | S: asserted a getter (`interval_secs(Sip)`); now `next_interval_secs` [P26] |
| policy | sip_floors_are_hard_boundaries | exactly at a sip floor is not viable | a | - |
| policy | replacement_ranks_by_dwell_then_headroom_then_the_limiting_reset | the landing order, case by case | b | S: two cases named the limiting-reset tie-break and were decided by the expiring weekly reset, by dwell, or by name; fixed, plus raw-use and name tie-breaks [P15 P16 P17 P41] |
| policy | fable_landing_ranks_on_fable_room_in_weekly_terms | Fable landing ranks on share-scaled room | a | - |
| policy | landing_spends_expiring_weekly_room_first_and_otherwise_the_longest_dwell | expiring weekly room first, else dwell | b | S: unreported reset is not expiring; expiring scrap does not beat real room [P12 P19] |
| policy | a_weekly_only_account_is_ranked_on_its_weekly_room_alone | no 5h window: ranked and gated on weekly | a | - |
| policy | weekly_first_moves_where_a_switch_lands_never_whether_it_happens | healthy default stays however soon another resets | a | - |
| policy | the_default_stays_until_its_own_floor_however_roomy_the_rest | default leaves only at its own normal floor | a | - |
| policy | below_its_floor_the_default_drains_in_place_rather_than_land_on_scraps | no landing: drain to the sip floor | a | - |
| policy | landing_prefers_real_room_and_falls_back_to_the_roomiest_scrap | landing floors first, then sip floors, else none | a | - |
| policy | a_model_bucket_is_a_weekly_floor_like_any_other | a model limit at its floor makes the default leave | a | - |
| policy | cadence_is_fast_when_degraded_or_near_a_floor_and_normal_otherwise | fast after a partial probe or near a floor | b | S: weekly near-floor, no default reading, unread default [P25 P28] |
| policy | a_failed_probe_is_not_a_reading_but_a_refusal_is_definitive | network failure is incomplete; 401 is complete | a | - |
| policy | an_account_reporting_only_its_weekly_window_is_a_complete_reading | weekly-only reading is complete, viable, rankable | a | - |
| policy | partial_fleet_view_cannot_enter_sip_and_drain | partial view keeps the previous mode | b | S: a normal candidate in a partial view is Normal; a complete view stands [P7] |
| policy | decisions_follow_the_floors | decide: switch, stay, hold, none viable, drain | b | kept; `force`, three triggers and DefaultUnreadable were unreached: new test below [P30-P36] |
| policy | the_same_ranking_serves_codex_readings | the one ranking on Codex-shaped readings | a | - |
| policy | a_seat_leaves_at_its_floor_for_real_room_and_a_spent_one_for_any | relocate: stay, move, drain, spent seat takes any room | a | - |
| policy | the_same_ranking_serves_antigravity_readings | ranking on scoped_to group readings | a | - |
| quota | model_keys_agree_between_ids_and_display_names | ids and display names canonicalise alike | a | - |
| quota | a_model_is_limited_by_its_own_bucket_and_its_family_wide_one | own, major-only and family-wide limits apply | a | - |
| quota | the_opus_alias_obeys_opus_5_not_opus_4_8 | bare `opus` is Opus 5 | a | - |
| quota | keys_outside_any_family_match_by_equality | non-family keys match exactly; empty never | a | - |
| quota | the_exact_floor_is_not_above_it | boundary of `above` | a | - |
| quota | a_limiting_bucket_is_a_weekly_floor_and_an_unrelated_one_is_not | model limit gates its model only | a | - |
| quota | a_window_the_api_omits_limits_nothing | omitted window limits nothing; no window is no quota | a | - |
| quota | errors_and_rejections_are_never_above_a_floor | rejected or failed is never above | b | S: windows beside an error are no reading; unreadable credential is definitive [Q9 Q18] |
| quota | a_shared_window_rejected_by_its_own_status_is_not_above_any_floor | a window's own rejection gates the account | a | - |
| quota | fable_room_is_its_share_of_the_weekly_and_never_more_than_the_weekly_left | share-scaled room capped by the weekly | b | S: the cap was only seen through `model_room`; `Limit::room` asserted [Q14] |
| quota | a_shared_bucket_is_viable_on_its_room_in_weekly_terms | shared limit judged in weekly terms | b | S: exactly at the floor [Q16] |
| quota | mythos_is_a_family | Mythos has a family-wide limit | a | - |
| quota | wire_enums_parse_known_values_and_keep_the_unknown_ones | unknown header word becomes Unrecognized | a | - |
| quota | readings_without_the_optional_fields_still_parse_and_do_not_grow_on_write | old/bare stored readings parse; empty fields not written | a | - |
| quota | read_errors_group_by_cause | reason(), definitive, unauthorized, stored spelling | b | S: expired and org-disallowed causes [Q25] |
| quota | limit_serde_keeps_the_stored_spelling | the JSON every stored row uses | b | S: span names (`five_hour` ...) were not pinned [Q33] |
| quota | spans_are_named_by_length | span from seconds, labels | b | S: 27/31/32 days [Q19] |
| quota | the_accessors_read_a_claude_reading | accessors on a Claude-shaped reading | b | S: a model limit stays out of `scoped_to(None)` [Q22] |
| quota | the_accessors_read_a_codex_shaped_reading | accessors, spans held to the weekly floor, scoped feature | b | S: an account-wide limit is never matched by a scope [Q23] |
| quota | the_accessors_read_an_antigravity_shaped_reading | group-only reading has no quota until scoped | a | - |
| quota | setting_a_window_keeps_what_else_is_known_about_the_limit | set_window keeps status; remove_limit | a | - |
| forecast | one_account_runs_dry_when_its_room_is_spent_at_the_measured_rate | room / rate | a | - |
| forecast | the_floor_is_kept_back_from_every_account | floor subtracted from room | b | S: and from the window a reset gives back [F10] |
| forecast | the_fleet_spends_every_account_at_the_sum_of_the_rates | rates and rooms add | a | - |
| forecast | a_reset_inside_the_horizon_refills_its_account | reset loses unspent room, refills | a | - |
| forecast | the_account_that_resets_soonest_is_spent_first | spend order | a | - |
| forecast | room_that_outlasts_the_week_has_no_time_and_no_use_is_idle | Beyond and Idle | b | S: resets on offer add nothing when nothing runs dry [F17] |
| forecast | resets_on_offer_are_reported_apart_and_never_counted_as_room | refills only in `with_resets` | a | - |
| forecast | a_model_limit_that_is_a_share_of_the_weekly_caps_the_room | Fable share caps the room | b | S: and the refill after a reset [F16] |
| forecast | no_forecast_without_a_measured_rate | None without an hour of one window | b | kept; its cross-reset case was decided by the falling value, not the reset: new rate test [F2 F5] |
| forecast | accounts_counted_through_windows_of_different_lengths_are_not_added | mixed spans: None | a | - |
| forecast | a_window_that_is_not_running_starts_when_it_is_first_spent | unused window starts at first use | b | kept as is: asserts the unused account's room is counted; the start-at-first-use it names cannot be observed inside the horizon [F20] |
| settings | set_parses_each_key_and_rejects_the_rest | `settings set` per key | a | S: `on`/`0` [S15] |
| settings | dangerous_mode_adds_the_flag_once | flag added once | a | - |
| settings | floors_must_nest | sip <= normal <= landing | b | S: range and zero-interval refusals, each by its message [S5 S9] |
| settings | the_codex_floors_are_set_by_name_and_must_nest | codex floors: set, nest, refuse, old file | a | S: a `[settings]` table without the newer keys [S14]. Two lines assert the 0.20 defaults; kept as the statement that they equal `codex watch`'s |
| util | one_line_flattens_and_caps_on_characters | cap in characters | b | S: within the cap in characters, over in bytes [U1] |
| util | a_quoted_word_survives_an_apostrophe | shell quoting | a | - |
| util | a_missing_or_unreadable_reset_is_none_pending | reset parse, 0 when absent | b | S: zone offset [U5] |
| util | backoff_doubles_from_the_first_failure_to_the_cap | backoff | a | - |
| observe | anthropic_base_urls_are_recognised_and_third_parties_are_not | which base URL is Anthropic | b | S: userinfo before `@`, IPv6 [O2] |
| observe | env_flag_parses_the_usual_spellings | flag spellings | a | - |
| observe | reset_messages_parse_dates_and_roll_bare_clocks_to_tomorrow | reset text to epoch | b | S: a date already past rolls to next year; 12pm, 11:30PM [O6] |
| observe | observed_opus_limit_stays_separate_from_general_weekly | family wall is a model limit, not the weekly | a | - |
| observe | general_and_session_rejections_mark_the_reading_rejected | account-wide walls reject; failed read untouched | b | S: general weekly on a failed read [O8] |
| observe | weekly_rejection_uses_the_bound_sessions_model_family | rejection takes the session's model | b | S: 6 h fallback of a session limit [O20] |
| observe | exact_model_rejection_becomes_a_scoped_bucket | concrete model wall is that model's | a | - |
| observe | fable_weekly_rejection_is_a_fable_bucket_not_the_general_weekly | Fable wall leaves the account allowed | a | - |
| observe | a_family_wide_fable_limit_lands_on_the_shared_fable_bucket | family wall replaces the header's limit | a | - |
| observe | a_repeated_rejection_for_the_same_reset_is_not_new_but_a_new_reset_replaces_it | merge rule | b | S: older observation does not replace; other model/account/window are their own [O12 O13] |
| observe | transcript_cursor_ignores_forked_history_and_observes_only_new_failure | cursor, fork, partial line, reattribution, rewrite | b | S: the forked record was before the byte boundary, so the timestamp rule it names never ran; the limit words from a user or a non-error record [O14 O21] |
| observe | the_state_file_keeps_its_stored_spelling | admission state JSON | a | - |
| observe | limits_describe_themselves_and_bindings_match_by_start_time | summary, labels, reset spec, --model | b | renamed `..._and_the_command_line_names_the_model`: nothing in it matched a binding; that is a new test [O25] |
| observe | todays_state_file_loads_and_an_entry_it_cannot_read_is_dropped_alone | lenient load of today's file | a | - |
| attention | a_pushed_codex_seat_nothing_renewed_needs_the_user_before_it_ends | CodexSeatExpires by monitor, grace, cause, key | a | - |
| attention | a_healthy_fleet_needs_nothing | nothing for a healthy or unread fleet | a | - |
| attention | a_login_is_reported_at_three_days_and_again_at_one | thresholds are separate keys | a | - |
| attention | a_refused_login_and_an_old_one_without_a_date | refusal wins; ageing | a | - |
| attention | a_service_is_below_its_floor_only_when_no_account_read_above_it | BelowFloor: parked, failed, mixed | a | - |
| attention | resets_on_offer_are_reported_as_they_near_their_end | grants and credits near expiry | b | S: a Codex credit already expired [A11] |
| attention | a_fleet_forecast_under_twelve_hours_and_a_stopped_service | RunsDry under 12 h; DaemonStopped | a | - |
| attention | items_serialize_with_their_kind | the JSON the Swift app reads | a | - |
| store | a_reading_round_trips_through_the_store | insert/recent equality; series | a | - |
| store | a_database_held_by_another_connection_is_an_error_not_an_empty_history | held database is an error | a | - |
| store | the_tables_of_earlier_builds_are_not_read | old tables unread, untouched, flagged | a | - |
| store | every_service_shares_the_table_and_the_latest_is_per_account | (service, account) keys | b | kept; a hold leaking across services passed it: new test [T8] |
| store | a_read_only_open_sees_the_rows_and_creates_nothing | read-only creates no file | a | - |
| store | an_unchanged_reading_adds_no_row_until_the_hold_is_ten_minutes_old | hold rule, closing row, held tail | b | kept; new tests for T4b T5b T7b T18 |
| store | burn_rates_are_the_same_from_dense_and_sparse_rows | dense == sparse stats | b | kept; its reset drops the value, so the reset-window rule was not what excluded it: new test [T9 T9b T11] |
| compact | old_snapshots_become_readings_that_say_what_they_said | snapshots decode, original kept | b | S: database and original are 0600 [C20 C21] |
| compact | a_table_from_before_a_column_existed_is_rewritten_without_it | 0.2 schema | a | - |
| compact | old_service_and_codex_rows_become_readings | service/codex rows decode | a | - |
| compact | dense_old_rows_are_kept_as_a_live_store_would_have_kept_them | walk == Store::insert | a | - |
| compact | rows_readings_already_has_follow_the_old_ones | new rows last; index | a | - |
| compact | a_run_interrupted_anywhere_is_finished_by_the_next | kill at every step, resume, same bytes | a | - |
| compact | a_dry_run_counts_and_writes_nothing | rehearsal writes nothing | a | - |
| compact | the_database_is_held_exclusively_or_the_run_refuses | refuse when open elsewhere; keep others out | a | - |
| compact | another_process_is_kept_out_for_the_whole_run | a real child process is refused at every step | a | - (killed by C1, C2, C3) |
| compact | other_process | child-process helper, `#[ignore]` | - | - |
| compact | a_rewrite_that_differs_is_thrown_away_and_the_old_tables_stay | six damages, each refused by its reason | a | - |
| compact | a_file_in_the_originals_place_is_never_overwritten | foreign `.pre-0.4` refused | a | - |
| compact | steps_are_the_changes_of_value | step function of a history | a | - |

### Mutations

Each row is one mutation, applied alone by `scratchpad/g1work/mut.py`, the named filter run, and the file restored with
`git checkout`. Ids ending in a letter are variants. "after repair" is the same mutation rerun against the committed
tests. Logs: `scratchpad/g1work/res*.tsv` and `*.log`.

| id | file | change | tests run | outcome | follow-up |
|---|---|---|---|---|---|
| A1 | attention.rs | `left if left <= DAY_SECS => Some(Self::OneDay),` → `left if left < 0 => None, left if left <= DAY_SECS…` | attention:: | killed (a_login_is_reported_at_three_days_and_again_at_one) |  |
| A2 | attention.rs | `left if left <= 3 * DAY_SECS => Some(Self::ThreeDays),` → `left if left <= 7 * DAY_SECS => Some(Self::ThreeDa…` | attention:: | killed (a_login_is_reported_at_three_days_and_again_at_one) |  |
| A3 | attention.rs | `.filter(\|account\| !account.parked)` → `(removed)` | attention:: | killed (a_service_is_below_its_floor_only_when_no_account_read_above_it) |  |
| A4 | attention.rs | `.is_some_and(\|reading\| reading.error.is_none())` → `.is_some()` | attention:: | killed (a_service_is_below_its_floor_only_when_no_account_read_above_it) |  |
| A5 | attention.rs | `if !verdicts.is_empty() && verdicts.iter().all(\|viable\| !v…` → `if verdicts.iter().any(\|viable\| !viable) {` | attention:: | killed (a_service_is_below_its_floor_only_when_no_account_read_above_it +1) |  |
| A6 | attention.rs | `&& at - now < DRY_WITHIN_SECS` → `(removed)` | attention:: | killed (a_fleet_forecast_under_twelve_hours_and_a_stopped_service) |  |
| A7 | attention.rs | `pub const DRY_WITHIN_SECS: i64 = 12 * 3_600;` → `pub const DRY_WITHIN_SECS: i64 = 9 * 3_600;` | attention:: | killed (a_fleet_forecast_under_twelve_hours_and_a_stopped_service) |  |
| A8 | attention.rs | `if login.refused { found.push(Attention::LoginRefused { acco…` → `if login.refused { found.push(Attention::LoginRefu…` | attention:: | killed (a_refused_login_and_an_old_one_without_a_date) |  |
| A9 | attention.rs | `if let Some(expires_at) = offer.expires_at.filter(\|at\| *at…` → `if let Some(expires_at) = offer.expires_at` | attention:: | killed (resets_on_offer_are_reported_as_they_near_their_end) |  |
| A10 | attention.rs | `.filter(\|credits\| credits.available > 0);` → `;` | attention:: | killed (resets_on_offer_are_reported_as_they_near_their_end) |  |
| A11 | attention.rs | `.and_then(\|credits\| credits.expires_at) .filter(\|at\| *at…` → `.and_then(\|credits\| credits.expires_at)` | attention:: | SURVIVED | after repair: killed (resets_on_offer_are_reported_as_they_near_their_end) |
| A12 | attention.rs | `found.sort_by_key(\|item\| item.level() != Level::Critical);` → `(removed)` | attention:: | killed (a_refused_login_and_an_old_one_without_a_date +1) |  |
| A13 | attention.rs | `} => format!("login-ends:{account}:{}", within.name()),` → `} => format!("login-ends:{account}"),` | attention:: | killed (a_login_is_reported_at_three_days_and_again_at_one) |  |
| A14 | attention.rs | `"reset-expires:{}:{account}:{expires_at}:{}",` → `"reset-expires:{}:{account}:0:{}",` | attention:: daemon::notify:: | killed (resets_on_offer_are_reported_as_they_near_their_end) |  |
| A15 | attention.rs | `Within::OneDay => Level::Critical, Within::ThreeDays => Leve…` → `Within::OneDay => Level::Low, Within::ThreeDays =>…` | attention:: | killed (a_login_is_reported_at_three_days_and_again_at_one +1) |  |
| A16 | attention.rs | `if overview.monitor == Monitor::Stopped {` → `if overview.monitor != Monitor::Running {` | attention:: | killed (a_refused_login_and_an_old_one_without_a_date +5) |  |
| A17 | attention.rs | `if left.is_some_and(\|left\| left < -DAY_SECS) { continue; }` → `(removed)` | attention:: | killed (a_pushed_codex_seat_nothing_renewed_needs_the_user_before_it_ends) |  |
| A18 | attention.rs | `(None, _) if !due => continue,` → `(removed)` | attention:: | killed (a_pushed_codex_seat_nothing_renewed_needs_the_user_before_it_ends) |  |
| A19 | attention.rs | `if left.is_some_and(\|left\| left > RENEW_WITHIN_SECS - grac…` → `(removed)` | attention:: | killed (a_pushed_codex_seat_nothing_renewed_needs_the_user_before_it_ends) |  |
| A20 | attention.rs | `(Some(trouble), _) => Unkept::Tried(trouble.cause),` → `(removed)` | attention:: | COMPILE-ERROR | mutation did not compile; replaced by A20b |
| A20b | attention.rs | `(Some(trouble), _) => Unkept::Tried(trouble.cause),` → `(Some(_), _) if !due => continue, (Some(trouble), …` | attention:: | killed (a_pushed_codex_seat_nothing_renewed_needs_the_user_before_it_ends) |  |
| A21 | attention.rs | `} else if login.warning {` → `} else if true {` | attention:: | killed (a_healthy_fleet_needs_nothing +4) |  |
| A22 | attention.rs | `"codex-seat:{home}:{}:{}", expires_at.unwrap_or(0),` → `"codex-seat:{home}:{}:{}", 0,` | attention:: | killed (a_pushed_codex_seat_nothing_renewed_needs_the_user_before_it_ends) |  |
| A23 | attention.rs | `#[serde(rename = "1d")]` → `(removed)` | attention::items_serialize | killed (items_serialize_with_their_kind) |  |
| A24 | attention.rs | `#[serde(tag = "kind", rename_all = "kebab-case")] pub enum A…` → `#[serde(tag = "kind", rename_all = "snake_case")] …` | attention::items_serialize | killed (items_serialize_with_their_kind) |  |
| C1 | store/compact.rs | `take_exclusively(&conn, tuning.wait)?;` → `(removed)` | store::compact:: | killed (the_database_is_held_exclusively_or_the_run_refuses +1) |  |
| C2 | store/compact.rs | `conn.pragma_update(None, "locking_mode", "EXCLUSIVE")?;` → `(removed)` | store::compact:: | killed (the_database_is_held_exclusively_or_the_run_refuses +1) |  |
| C3 | store/compact.rs | `self.0.push(file); Ok(&self.0[self.0.len() - 1])` → `self.0.push(file); let dropped = self.0.pop().unwr…` | store::compact:: | killed (a_file_in_the_originals_place_is_never_overwritten +3) |  |
| C4 | store/compact.rs | `if started && original.exists() { return Ok(()); }` → `(removed)` | store::compact:: | killed (a_rewrite_that_differs_is_thrown_away_and_the_old_tables_stay +1) |  |
| C5 | store/compact.rs | `if same_bytes(database, original)? { return Ok(()); } bail!(` → `if true { return Ok(()); } bail!(` | store::compact:: | killed (a_file_in_the_originals_place_is_never_overwritten) |  |
| C6 | store/compact.rs | `Some(kept) if reading.at - kept < HOLD_SECS => {} _ => retur…` → `_ => {}` | store::compact:: | killed (a_rewrite_that_differs_is_thrown_away_and_the_old_tables_stay) | first spec did not match; rerun in batch 7 |
| C7 | store/compact.rs | `if before.steps != after.steps {` → `if false {` | store::compact:: | killed (a_rewrite_that_differs_is_thrown_away_and_the_old_tables_stay) |  |
| C8 | store/compact.rs | `if *kept != reading.at { return Err(differs("a kept row stan…` → `(removed)` | store::compact:: | killed (a_rewrite_that_differs_is_thrown_away_and_the_old_tables_stay) |  |
| C9 | store/compact.rs | `if before.passed != after.times.len() \|\| after.times.last(…` → `if false {` | store::compact:: | killed (a_rewrite_that_differs_is_thrown_away_and_the_old_tables_stay) |  |
| C10 | store/compact.rs | `if let Some((service, account)) = staged.keys().next() { bai…` → `(removed)` | store::compact:: | killed (a_rewrite_that_differs_is_thrown_away_and_the_old_tables_stay) |  |
| C11 | store/compact.rs | `if let Some(open) = self.open.take() && open.confirmed > ope…` → `if let Some(open) = self.open.take() && false {` | store::compact:: | killed (dense_old_rows_are_kept_as_a_live_store_would_have_kept_them +6) |  |
| C12 | store/compact.rs | `open.confirmed = reading.at; if kept.row {` → `if kept.row {` | store::compact:: | killed (dense_old_rows_are_kept_as_a_live_store_would_have_kept_them +6) |  |
| C13 | store/compact.rs | `progress.save(conn)?; tx.commit()?;` → `tx.commit()?; progress.save(conn)?;` | store::compact:: | SURVIVED | equivalent at the test's kill points (no step between the commit and the save); residual |
| C14 | store/compact.rs | `if progress.done { return Ok(()); }` → `(removed)` | store::compact:: | SURVIVED | equivalent: a finished table walks zero rows again |
| C15 | store/compact.rs | `"INSERT INTO {STAGING} (service, account, at, reading) SELEC…` → `"DROP TABLE readings;"` | store::compact:: | killed (rows_readings_already_has_follow_the_old_ones) | first spec did not match; rerun in batch 7 |
| C16 | store/compact.rs | `while take_batch(&store.conn, source, &mut progress, &mut wa…` → `while take_batch(&store.conn, source, &mut progres…` | store::compact:: | killed (a_dry_run_counts_and_writes_nothing) |  |
| C17 | store/compact.rs | `conn.execute_batch(&format!( "BEGIN; DROP TABLE {STAGING}; D…` → `return Err(error.context(` | store::compact:: | killed (a_rewrite_that_differs_is_thrown_away_and_the_old_tables_stay) |  |
| C18 | store/compact.rs | `Some(last) if last.1 == said => {}` → `(removed)` | store::compact:: | killed (steps_are_the_changes_of_value +7) |  |
| C19 | store/compact.rs | `if reading.at < open.confirmed {` → `if false {` | store::compact:: | SURVIVED | after repair: killed (old_rows_out_of_time_order_stop_the_run_with_nothing_dropped) |
| C20 | store/compact.rs | `conn.close().map_err(\|(_, error)\| error)?; crate::fs::set_…` → `conn.close().map_err(\|(_, error)\| error)?;` | store::compact:: | SURVIVED | after repair: killed (old_snapshots_become_readings_that_say_what_they_said) |
| C21 | store/compact.rs | `crate::fs::set_mode(&partial, 0o600)?;` → `(removed)` | store::compact:: | SURVIVED | after repair: killed (old_snapshots_become_readings_that_say_what_they_said) |
| C22 | store/compact.rs | `room_for(path)?;` → `(removed)` | store::compact:: | SURVIVED | free-space check has no seam (fs2::available_space); residual |
| F1 | forecast.rs | `.max_by_key(\|limit\| limit.span.seconds())` → `.min_by_key(\|limit\| limit.span.seconds())` | forecast:: | SURVIVED | after repair: killed (an_account_counts_through_its_longest_window) |
| F2 | forecast.rs | `(secs > 0 && a.resets_at == b.resets_at && b.used >= a.used)` → `(secs > 0 && b.used >= a.used)` | forecast:: | SURVIVED | after repair: killed (the_rate_is_the_use_gained_inside_one_reset_window_over_the_time_it_covers) |
| F3 | forecast.rs | `(secs > 0 && a.resets_at == b.resets_at && b.used >= a.used)` → `(secs > 0 && a.resets_at == b.resets_at)` | forecast:: | SURVIVED | after repair: killed (the_rate_is_the_use_gained_inside_one_reset_window_over_the_time_it_covers) |
| F4 | forecast.rs | `if covered >= MIN_COVER_SECS {` → `if covered > 0 {` | forecast:: | killed (no_forecast_without_a_measured_rate) |  |
| F5 | forecast.rs | `if covered >= MIN_COVER_SECS {` → `if covered > MIN_COVER_SECS {` | forecast:: | SURVIVED | after repair: killed (the_rate_is_the_use_gained_inside_one_reset_window_over_the_time_it_covers) |
| F6 | forecast.rs | `if measured == 0 { return None; }` → `(removed)` | forecast:: | killed (no_forecast_without_a_measured_rate) |  |
| F7 | forecast.rs | `if counted.iter().any(\|(_, limit)\| limit.span != span) { r…` → `(removed)` | forecast:: | killed (accounts_counted_through_windows_of_different_lengths_are_not_added) |  |
| F8 | forecast.rs | `.filter(\|account\| account.latest.error.is_none())` → `(removed)` | forecast:: | SURVIVED | equivalent in practice: a failed read carries no limits (Reading::failed); not pursued |
| F9 | forecast.rs | `room: (room - floor).max(0.0),` → `room: room.max(0.0),` | forecast:: | killed (the_floor_is_kept_back_from_every_account) |  |
| F10 | forecast.rs | `full: (full - floor).max(0.0),` → `full: full.max(0.0),` | forecast:: | SURVIVED | after repair: killed (the_floor_is_kept_back_from_every_account) |
| F11 | forecast.rs | `reset: Some(limit.window.resets_at).filter(\|reset\| *reset …` → `reset: Some(limit.window.resets_at),` | forecast:: | SURVIVED | left: whether a window whose reset passed since the read counts as read or as full is a judgment the design does not state; residual |
| F12 | forecast.rs | `.min_by_key(\|(_, pool)\| pool.reset.unwrap_or(i64::MAX))` → `.max_by_key(\|(_, pool)\| pool.reset.unwrap_or(i64…` | forecast:: | killed (the_account_that_resets_soonest_is_spent_first) |  |
| F13 | forecast.rs | `pool.room = pool.full; pool.reset = None;` → `pool.reset = None;` | forecast:: | killed (a_reset_inside_the_horizon_refills_its_account +2) |  |
| F14 | forecast.rs | `let refill = use_refills .then(\|\| soonest(&pools, \|pool\|…` → `let refill = soonest(&pools, \|pool\| pool.refills…` | forecast:: | killed (resets_on_offer_are_reported_apart_and_never_counted_as_room) |  |
| F15 | forecast.rs | `pools[index].refills -= 1; pools[index].room = pools[index].…` → `pools[index].refills -= 1; pools[index].room = 2.0…` | forecast:: | killed (resets_on_offer_are_reported_apart_and_never_counted_as_room) |  |
| F16 | forecast.rs | `.filter_map(\|limit\| limit.share) .fold(1.0, f64::min),` → `.filter_map(\|limit\| limit.share) .fold(1.0, f64:…` | forecast:: | SURVIVED | after repair: killed (a_model_limit_that_is_a_share_of_the_weekly_caps_the_room) |
| F17 | forecast.rs | `with_resets: (refills > 0 && matches!(plain, Outlook::Dry { …` → `with_resets: (refills > 0).then(\|\| outlook(true)…` | forecast:: | SURVIVED | after repair: killed (room_that_outlasts_the_week_has_no_time_and_no_use_is_idle) |
| F18 | forecast.rs | `if rate < IDLE_RATE {` → `if rate < 0.0 {` | forecast:: | killed (room_that_outlasts_the_week_has_no_time_and_no_use_is_idle) |  |
| F19 | forecast.rs | `if t >= end { return None; }` → `(removed)` | forecast:: | SURVIVED | equivalent: the walk's iteration bound returns None anyway |
| F20 | forecast.rs | `pools[index] .reset .get_or_insert(t.round() as i64 + span_s…` → `(removed)` | forecast:: | SURVIVED | equivalent inside the 7-day horizon for weekly and longer windows (the first-use reset lands at or past the horizon); the test stays class b |
| F21 | forecast.rs | `rate += used / (covered as f64 / 3_600.0);` → `rate += used / (BURN_HOURS as f64);` | forecast:: | SURVIVED | after repair: killed (the_rate_is_the_use_gained_inside_one_reset_window_over_the_time_it_covers) |
| L1 | store/legacy.rs | `if text == "no usable credential" { return ReadError::NoCred…` → `(removed)` | store:: | SURVIVED | after repair: killed (the_errors_old_snapshots_held_as_text_keep_their_kind) |
| L2 | store/legacy.rs | `status: status.parse().ok()?, body: body.to_owned(),` → `status: status.parse().ok()?, body: text.to_owned(…` | store:: | killed (old_snapshots_become_readings_that_say_what_they_said) |  |
| L3 | store/legacy.rs | `NaiveDateTime::parse_from_str(&at, "%Y-%m-%dT%H:%M:%S%.f").m…` → `NaiveDateTime::parse_from_str(&at, "%Y-%m-%dT%H:%M…` | store:: | killed (a_table_from_before_a_column_existed_is_rewritten_without_it) |  |
| L4 | store/legacy.rs | `get::<f64>(row, used).map(\|used\| Window::new(used, get(row…` → `get::<f64>(row, used).map(\|used\| Window::new(use…` | store:: | killed (a_table_from_before_a_column_existed_is_rewritten_without_it +1) |  |
| L5 | store/legacy.rs | `if let Some(message) = text.strip_prefix("network: ") { retu…` → `(removed)` | store:: | SURVIVED | after repair: killed (the_errors_old_snapshots_held_as_text_keep_their_kind) |
| O1 | observe.rs | `host == "anthropic.com" \|\| host.ends_with(".anthropic.com"…` → `host.contains("anthropic.com")` | observe:: | killed (anthropic_base_urls_are_recognised_and_third_parties_are_not) |  |
| O2 | observe.rs | `.rsplit_once('@') .map_or(authority, \|(_, host)\| host);` → `.split_once('@') .map_or(authority, \|(host, _)\| …` | observe:: | SURVIVED | after repair: killed (anthropic_base_urls_are_recognised_and_third_parties_are_not) |
| O3 | observe.rs | `(12, "AM") => 0,` → `(12, "AM") => 12,` | observe:: | killed (reset_messages_parse_dates_and_roll_bare_clocks_to_tomorrow) | first spec did not match; rerun in batch 6 |
| O4 | observe.rs | `(hour, "PM") => hour + 12,` → `(hour, "PM") => hour,` | observe:: | killed (reset_messages_parse_dates_and_roll_bare_clocks_to_tomorrow) | first spec did not match; rerun in batch 6 |
| O5 | observe.rs | `if local.timestamp() > observed_at { return Some(local.times…` → `Some(local.timestamp())` | observe:: | killed (reset_messages_parse_dates_and_roll_bare_clocks_to_tomorrow) |  |
| O6 | observe.rs | `for year in [observed.year(), observed.year() + 1] {` → `for year in [observed.year()] {` | observe:: | SURVIVED | after repair: killed (reset_messages_parse_dates_and_roll_bare_clocks_to_tomorrow) |
| O7 | observe.rs | `(LimitWindow::Weekly, _, Some(model)) => reject_bucket(readi…` → `(LimitWindow::Weekly, family, Some(_)) if family.m…` | observe:: | killed (exact_model_rejection_becomes_a_scoped_bucket +1) |  |
| O8 | observe.rs | `(LimitWindow::Weekly, Family::General, None) => { if reading…` → `(LimitWindow::Weekly, Family::General, None) => { …` | observe:: | SURVIVED | after repair: killed (general_and_session_rejections_mark_the_reading_rejected) |
| O9 | observe.rs | `(LimitWindow::Session, _, _) => { if reading.error.is_none()…` → `(LimitWindow::Session, _, _) => { if true {` | observe:: | killed (general_and_session_rejections_mark_the_reading_rejected) |  |
| O10 | observe.rs | `claude.verdict = Verdict::Rejected;` → `(removed)` | observe:: | killed (general_and_session_rejections_mark_the_reading_rejected) |  |
| O11 | observe.rs | `if incoming.resets_at != existing.resets_at && incoming.obse…` → `if incoming.observed_at >= existing.observed_at` | observe:: | killed (a_repeated_rejection_for_the_same_reset_is_not_new_but_a_new_reset_replaces_it) |  |
| O12 | observe.rs | `if incoming.resets_at != existing.resets_at && incoming.obse…` → `if incoming.resets_at != existing.resets_at` | observe:: | SURVIVED | after repair: killed (a_repeated_rejection_for_the_same_reset_is_not_new_but_a_new_reset_replaces_it) |
| O13 | observe.rs | `&& existing.model == incoming.model` → `(removed)` | observe:: | SURVIVED | after repair: killed (a_repeated_rejection_for_the_same_reset_is_not_new_but_a_new_reset_replaces_it) |
| O14 | observe.rs | `if observed_at + 2 < binding.observed_at { continue; }` → `(removed)` | observe:: | SURVIVED | after repair: killed (transcript_cursor_ignores_forked_history_and_observes_only_new_failure) |
| O15 | observe.rs | `reader.seek(SeekFrom::Start(line_start))?; break;` → `break;` | observe:: | killed (transcript_cursor_ignores_forked_history_and_observes_only_new_failure) |  |
| O16 | observe.rs | `binding.scan_offset = len; return Ok((Vec::new(), true));` → `return Ok((Vec::new(), true));` | observe:: | killed (transcript_cursor_ignores_forked_history_and_observes_only_new_failure) |  |
| O17 | observe.rs | `Some(None) => continue, None => {}` → `Some(None) => {} None => {}` | observe:: | killed (transcript_cursor_ignores_forked_history_and_observes_only_new_failure) |  |
| O18 | observe.rs | `.filter(\|model\| !model.is_empty() && *model != "<synthetic…` → `.filter(\|model\| !model.is_empty())` | observe:: | killed (transcript_cursor_ignores_forked_history_and_observes_only_new_failure) |  |
| O19 | observe.rs | `model: (window == LimitWindow::Weekly) .then(\|\| last_model…` → `model: last_model.map(ModelKey::new)` | observe:: | killed (weekly_rejection_uses_the_bound_sessions_model_family) |  |
| O20 | observe.rs | `LimitWindow::Session => SESSION_FALLBACK.num_seconds(),` → `LimitWindow::Session => WEEKLY_FALLBACK.num_second…` | observe:: | SURVIVED | after repair: killed (weekly_rejection_uses_the_bound_sessions_model_family) |
| O21 | observe.rs | `if value.get("type")?.as_str()? != "assistant" \|\| value.ge…` → `(removed)` | observe:: | SURVIVED | after repair: killed (transcript_cursor_ignores_forked_history_and_observes_only_new_failure) |
| O22 | observe.rs | `state.limits.retain(\|limit\| limit.active_at(now));` → `(removed)` | observe:: | SURVIVED | after repair: killed (the_admission_state_forgets_passed_resets_and_old_sessions) |
| O23 | observe.rs | `.retain(\|_, session\| session.observed_at >= oldest);` → `.retain(\|_, _\| true);` | observe:: | SURVIVED | after repair: killed (the_admission_state_forgets_passed_resets_and_old_sessions) |
| O24 | observe.rs | `limit.account != account \|\| family.is_some_and(\|family\| …` → `limit.account != account` | observe:: commands::rotate:: | SURVIVED | after repair: killed (clearing_a_quarantine_takes_one_accounts_limits_or_one_family_of_them) |
| O25 | observe.rs | `.is_some_and(\|started\| (started - started_at).abs() <= 2)` → `.is_some_and(\|_\| true)` | observe:: claude::sessions:: | SURVIVED | after repair: killed (a_live_process_is_matched_to_its_hook_record_by_pid_and_start_time) |
| O26 | observe.rs | `.any(\|flag\| env_flag(flag).unwrap_or(false)) { return fals…` → `.any(\|flag\| env_flag(flag).unwrap_or(false)) { r…` | observe:: | SURVIVED | after repair: killed (only_a_session_that_reaches_anthropic_is_bound_to_an_account) |
| O27 | observe.rs | `self.resets_at > now` → `self.resets_at >= now` | observe:: | killed (limits_describe_themselves_and_bindings_match_by_start_time) |  |
| O28 | observe.rs | `.map(\|model\| ModelKey::new(&model)) .filter(\|key\| !key.i…` → `.map(\|model\| ModelKey::family_wide(quota::Family…` | observe:: | killed (the_state_file_keeps_its_stored_spelling +1) |  |
| O29 | observe.rs | `&& Family::of(model.as_str()) != family` → `&& false` | observe:: commands::rotate:: | SURVIVED | after repair: killed (a_quarantine_needs_a_reset_ahead_and_a_model_of_its_family) |
| O30 | observe.rs | `if resets_at <= now { bail!("quarantine reset must be in the…` → `(removed)` | observe:: commands::rotate:: | SURVIVED | after repair: killed (a_quarantine_needs_a_reset_ahead_and_a_model_of_its_family) |
| O31 | observe.rs | `if let Some(explicit) = explicit { return explicit; }` → `(removed)` | observe:: | killed (only_a_session_that_reaches_anthropic_is_bound_to_an_account) |  |
| O32 | observe.rs | `.max_by_key(\|binding\| binding.observed_at)` → `.min_by_key(\|binding\| binding.observed_at)` | observe:: | killed (a_live_process_is_matched_to_its_hook_record_by_pid_and_start_time) |  |
| O33 | observe.rs | `"1" \| "true" \| "yes" \| "on" => Some(true),` → `"1" \| "true" \| "on" => Some(true),` | observe::env_flag | killed (env_flag_parses_the_usual_spellings) |  |
| O34 | observe.rs | `match raw.trim().to_ascii_lowercase().as_str() {` → `match raw.to_ascii_lowercase().as_str() {` | observe::env_flag | killed (env_flag_parses_the_usual_spellings) |  |
| O35 | observe.rs | `#[serde(rename = "five-hour")]` → `(removed)` | observe::the_state_file observe::todays_state | killed (the_state_file_keeps_its_stored_spelling) |  |
| O36 | observe.rs | `#[serde(default, deserialize_with = "crate::fs::lenient_vec"…` → `#[serde(default)] limits: Vec<ObservedLimit>,` | observe::todays_state | killed (todays_state_file_loads_and_an_entry_it_cannot_read_is_dropped_alone) |  |
| O37 | observe.rs | `(Some(family), Some(version)) if key.as_str() != family.name…` → `(Some(family), Some(_)) if false => {` | observe:: | COMPILE-ERROR | mutation did not compile; model_label is asserted directly in three observe tests |
| P1 | policy.rs | `{ Mode::Normal } else { Mode::Sip }` → `{ Mode::Sip } else { Mode::Normal }` | policy:: | killed (sip_mode_unlocks_the_reserve_band +1) |  |
| P2 | policy.rs | `Mode::Sip => Floors { session: self.sip_min_5h_remaining, we…` → `Mode::Sip => Floors { session: self.sip_min_7d_rem…` | policy:: | killed (sip_floors_are_hard_boundaries) |  |
| P3 | policy.rs | `Floors { session: self.land_min_5h_remaining, weekly: self.l…` → `Floors { session: self.land_min_7d_remaining, week…` | policy:: | SURVIVED | after repair: killed (a_landing_is_held_to_the_landing_floor_of_each_window) |
| P4 | policy.rs | `session: self.codex_min_5h_remaining, weekly: self.codex_min…` → `session: self.codex_land_min_5h_remaining, weekly:…` | policy:: codex::mover:: daemon::codex:: | killed (one_leaving_floor_raises_the_landing_floor_to_it) |  |
| P5 | policy.rs | `readings.len() == expected && readings` → `readings` | policy:: | killed (partial_fleet_view_cannot_enter_sip_and_drain +1) |  |
| P6 | policy.rs | `.all(\|reading\| reading.has_quota() \|\| reading.is_refused…` → `.all(\|reading\| reading.has_quota())` | policy:: | killed (a_failed_probe_is_not_a_reading_but_a_refusal_is_definitive) |  |
| P7 | policy.rs | `if complete(readings, expected) \|\| normal_candidate {` → `if complete(readings, expected) {` | policy:: | SURVIVED | after repair: killed (partial_fleet_view_cannot_enter_sip_and_drain) |
| P8 | policy.rs | `if complete(readings, expected) \|\| normal_candidate {` → `if true {` | policy:: | killed (partial_fleet_view_cannot_enter_sip_and_drain) |  |
| P9 | policy.rs | `\|\| (!landing_exists && above(current, policy.floors(Mode::…` → `\|\| above(current, policy.floors(Mode::Sip), mode…` | policy:: | killed (a_model_bucket_is_a_weekly_floor_like_any_other +2) |  |
| P10 | policy.rs | `above(current, policy.floors(Mode::Normal), model) \|\| (!la…` → `above(current, policy.landing_floors(), model) \|\…` | policy:: | killed (the_default_stays_until_its_own_floor_however_roomy_the_rest +1) |  |
| P11 | policy.rs | `choose_above(readings, policy.landing_floors(), model) .or_e…` → `choose_above(readings, policy.landing_floors(), mo…` | policy:: | killed (landing_prefers_real_room_and_falls_back_to_the_roomiest_scrap +1) |  |
| P12 | policy.rs | `choose_above(readings, policy.landing_floors(), model) .or_e…` → `choose_above(readings, policy.floors(Mode::Sip), m…` | policy:: | SURVIVED | after repair: killed (landing_spends_expiring_weekly_room_first_and_otherwise_the_longest_dwell) |
| P13 | policy.rs | `compare_reset(expiring_reset(a), expiring_reset(b)) .then_wi…` → `Ordering::Equal .then_with(\|\| compare_f64` | policy:: | killed (weekly_first_moves_where_a_switch_lands_never_whether_it_happens +3) |  |
| P14 | policy.rs | `.then_with(\|\| compare_f64(dwell_score(b, floors, model), d…` → `(removed)` | policy:: | killed (landing_spends_expiring_weekly_room_first_and_otherwise_the_longest_dwell +4) |  |
| P15 | policy.rs | `.then_with(\|\| compare_reset(reset_delay(a, floors, model),…` → `(removed)` | policy:: | SURVIVED | after repair: killed (replacement_ranks_by_dwell_then_headroom_then_the_limiting_reset) |
| P16 | policy.rs | `.then_with(\|\| compare_f64(used(a).0, used(b).0)) .then_wit…` → `(removed)` | policy:: | SURVIVED | after repair: killed (replacement_ranks_by_dwell_then_headroom_then_the_limiting_reset) |
| P17 | policy.rs | `.then_with(\|\| a.account.cmp(&b.account))` → `(removed)` | policy:: | SURVIVED | after repair: killed (replacement_ranks_by_dwell_then_headroom_then_the_limiting_reset) |
| P18 | policy.rs | `.filter(\|&reset\| reset > 0 && reset - reading.at <= EXPIRI…` → `.filter(\|&reset\| reset > 0 && reset <= EXPIRING_…` | policy:: | killed (landing_spends_expiring_weekly_room_first_and_otherwise_the_longest_dwell) |  |
| P19 | policy.rs | `.filter(\|&reset\| reset > 0 && reset - reading.at <= EXPIRI…` → `.filter(\|&reset\| reset - reading.at <= EXPIRING_…` | policy:: | SURVIVED | after repair: killed (landing_spends_expiring_weekly_room_first_and_otherwise_the_longest_dwell) |
| P20 | policy.rs | `pub const EXPIRING_WITHIN_SECS: i64 = 24 * 3600;` → `pub const EXPIRING_WITHIN_SECS: i64 = 7 * 24 * 360…` | policy:: | killed (landing_spends_expiring_weekly_room_first_and_otherwise_the_longest_dwell) |  |
| P21 | policy.rs | `room_margin(limit.room(weekly.as_ref()), floors.weekly),` → `room_margin(limit.window.remaining(), floors.weekl…` | policy:: | killed (fable_landing_ranks_on_fable_room_in_weekly_terms) |  |
| P22 | policy.rs | `Span::FiveHour => floors.session, _ => floors.weekly, }; (ma…` → `Span::FiveHour => floors.weekly, _ => floors.sessi…` | policy:: | SURVIVED | after repair: killed (dwell_is_room_above_each_windows_own_floor_as_a_share_of_what_the_floor_leaves) |
| P23 | policy.rs | `(remaining - floor) / (1.0 - floor)` → `remaining - floor` | policy:: | SURVIVED | after repair: killed (dwell_is_room_above_each_windows_own_floor_as_a_share_of_what_the_floor_leaves) |
| P24 | policy.rs | `.is_some_and(\|left\| left <= policy.normal_min_5h_remaining…` → `.is_some_and(\|left\| left <= policy.normal_min_5h…` | policy:: | killed (cadence_is_fast_when_degraded_or_near_a_floor_and_normal_otherwise) |  |
| P25 | policy.rs | `.is_some_and(\|left\| left <= policy.normal_min_7d_remaining…` → `.is_some_and(\|_\| false)` | policy:: | SURVIVED | after repair: killed (cadence_is_fast_when_degraded_or_near_a_floor_and_normal_otherwise) |
| P26 | policy.rs | `if mode == Mode::Sip \|\| !complete` → `if !complete` | policy:: | SURVIVED | after repair: killed (exhausted_fleet_stays_in_fast_sip_mode) |
| P27 | policy.rs | `if mode == Mode::Sip \|\| !complete` → `if mode == Mode::Sip` | policy:: | killed (cadence_is_fast_when_degraded_or_near_a_floor_and_normal_otherwise) |  |
| P28 | policy.rs | `\|\| current.is_none_or(\|reading\| !reading.has_quota()) \|…` → `\|\| current.is_some_and(near_floor)` | policy:: | SURVIVED | after repair: killed (cadence_is_fast_when_degraded_or_near_a_floor_and_normal_otherwise) |
| P29 | policy.rs | `&& current.is_none_or(\|reading\| !reading.has_quota() && !r…` → `&& current.is_none_or(\|reading\| !reading.has_quo…` | policy:: | killed (a_failed_probe_is_not_a_reading_but_a_refusal_is_definitive) |  |
| P30 | policy.rs | `if !complete(readings, expected) && !force {` → `if !complete(readings, expected) {` | policy:: | SURVIVED | after repair: killed (force_decides_on_what_was_read_and_every_switch_says_why) |
| P31 | policy.rs | `if current_unreadable(current, current_reading) && !force { …` → `(removed)` | policy:: | SURVIVED | after repair: killed (force_decides_on_what_was_read_and_every_switch_says_why) |
| P32 | policy.rs | `if staying && !force {` → `if staying {` | policy:: | SURVIVED | after repair: killed (force_decides_on_what_was_read_and_every_switch_says_why) |
| P33 | policy.rs | `if current == Some(best.account.as_str()) { return Decision:…` → `(removed)` | policy:: | SURVIVED | after repair: killed (force_decides_on_what_was_read_and_every_switch_says_why) |
| P34 | policy.rs | `Some(_) if healthy && force => Trigger::Forced,` → `(removed)` | policy:: | SURVIVED | after repair: killed (force_decides_on_what_was_read_and_every_switch_says_why) |
| P35 | policy.rs | `Some(reading) if reading.is_refused() => Trigger::Refused,` → `(removed)` | policy:: | SURVIVED | after repair: killed (force_decides_on_what_was_read_and_every_switch_says_why) |
| P36 | policy.rs | `Some(reading) if !viable(reading, Mode::Sip, policy, model) …` → `(removed)` | policy:: | SURVIVED | after repair: killed (force_decides_on_what_was_read_and_every_switch_says_why) |
| P37 | policy.rs | `if above(seat, leave, None) { return Relocation::Stay;` → `if above(seat, land, None) { return Relocation::St…` | policy:: | killed (a_seat_leaves_at_its_floor_for_real_room_and_a_spent_one_for_any) |  |
| P38 | policy.rs | `(!above(seat, ANY_ROOM, None)) .then(\|\| choose_above(other…` → `choose_above(others, ANY_ROOM, None)` | policy:: | killed (a_seat_leaves_at_its_floor_for_real_room_and_a_spent_one_for_any) |  |
| P39 | policy.rs | `let landing = choose_above(others, land, None).or_else(\|\| …` → `let landing = choose_above(others, leave, None).or…` | policy:: | killed (a_seat_leaves_at_its_floor_for_real_room_and_a_spent_one_for_any) |  |
| P40 | policy.rs | `let healthy = current_reading.is_some_and(\|reading\| viable…` → `let healthy = current_reading.is_some_and(\|readin…` | policy:: | killed (decisions_follow_the_floors) |  |
| P41 | policy.rs | `compare_f64(*a_margin, *b_margin).then_with(\|\| match (*a_r…` → `compare_f64(*b_margin, *a_margin).then_with(\|\| m…` | policy:: | SURVIVED | after repair: killed (replacement_ranks_by_dwell_then_headroom_then_the_limiting_reset) |
| P42 | policy.rs | `self.sip_probe_interval_secs .min(self.normal_probe_interval…` → `self.normal_probe_interval_secs` | policy:: | killed (cadence_is_fast_when_degraded_or_near_a_floor_and_normal_otherwise) |  |
| Q1 | quota.rs | `self.used < 1.0 - floor` → `self.used <= 1.0 - floor` | policy::exact_normal_floor_is_not_viable quota::the_exact_fl | killed (sip_floors_are_hard_boundaries +2) |  |
| Q2 | quota.rs | `.all(\|limit\| self.limit_above(limit, weekly_floor))` → `.all(\|_\| true)` | policy::known_opus_floor policy::the_fable_limit_gates polic | killed (unrelated_family_bucket_does_not_affect_selection +5) |  |
| Q3 | quota.rs | `(mine, theirs) => mine == theirs,` → `(_, _) => true,` | policy::exhausted_opus_48 quota::the_opus_alias quota::a_mod | killed (exhausted_opus_48_bucket_does_not_poison_opus_5_or_fable +2) |  |
| Q4 | quota.rs | `(_, None) => true,` → `(_, None) => false,` | policy::the_fable_limit_gates quota::a_model_is_limited quot | killed (mythos_is_a_family +2) |  |
| Q5 | quota.rs | `(family == Family::Opus && self.0 == "opus").then_some("5")` → `None` | policy::the_opus_alias_obeys quota::the_opus_alias quota::mo | killed (the_opus_alias_obeys_the_opus_5_bucket +2) |  |
| Q6 | quota.rs | `if theirs.len() == 1 => mine.starts_with(theirs)` → `if theirs.len() == 1 => mine == theirs` | quota::a_model_is_limited policy:: | killed (a_model_is_limited_by_its_own_bucket_and_its_family_wide_one) |  |
| Q7 | quota.rs | `_ => !self.0.is_empty() && self.0 == bucket.0,` → `_ => self.0 == bucket.0,` | quota::keys_outside | killed (keys_outside_any_family_match_by_equality) |  |
| Q8 | quota.rs | `Some(rest) if !rest.is_empty() => rest.to_owned(),` → `Some(rest) => rest.to_owned(),` | quota::model_keys_agree | killed (model_keys_agree_between_ids_and_display_names) |  |
| Q9 | quota.rs | `self.error.is_none() && self.shared().next().is_some()` → `self.shared().next().is_some()` | quota:: policy:: | SURVIVED | after repair: killed (errors_and_rejections_are_never_above_a_floor) |
| Q10 | quota.rs | `self.error.is_none() && self.shared().next().is_some()` → `self.error.is_none() && !self.limits.is_empty()` | quota:: policy:: | killed (a_window_the_api_omits_limits_nothing +2) |  |
| Q11 | quota.rs | `Detail::Codex(codex) => codex.allowed != Some(false),` → `Detail::Codex(codex) => codex.allowed == Some(true…` | quota:: policy:: | killed (the_accessors_read_a_codex_shaped_reading) |  |
| Q12 | quota.rs | `.all(\|limit\| !limit.is_rejected() && limit.window.above(fl…` → `.all(\|limit\| limit.window.above(floor(limit.span…` | quota:: policy:: | killed (a_shared_window_rejected_by_its_own_status_is_not_above_any_floor) |  |
| Q13 | quota.rs | `Span::FiveHour => session_floor, _ => weekly_floor,` → `Span::FiveHour => session_floor, Span::Weekly => w…` | quota:: policy:: | killed (the_accessors_read_a_codex_shaped_reading +1) |  |
| Q14 | quota.rs | `(Some(share), Some(weekly)) => (own * share).min(weekly.rema…` → `(Some(share), Some(_)) => own * share,` | quota:: policy:: forecast:: | SURVIVED | after repair: killed (fable_room_is_its_share_of_the_weekly_and_never_more_than_the_weekly_left) |
| Q15 | quota.rs | `(Some(share), None) => own * share,` → `(Some(_), None) => own,` | quota:: policy:: forecast:: | killed (fable_room_is_its_share_of_the_weekly_and_never_more_than_the_weekly_left) |  |
| Q16 | quota.rs | `Some(_) => limit.room(self.weekly().as_ref()) > floor,` → `Some(_) => limit.room(self.weekly().as_ref()) >= f…` | quota:: policy:: | SURVIVED | after repair: killed (a_shared_bucket_is_viable_on_its_room_in_weekly_terms) |
| Q17 | quota.rs | `Self::Http { status, .. } => matches!(status, 401 \| 403),` → `Self::Http { status, .. } => matches!(status, 401)…` | quota:: policy:: | killed (read_errors_group_by_cause) |  |
| Q18 | quota.rs | `Self::NoCredential \| Self::Credential { .. } => true, Self:…` → `Self::NoCredential => true, Self::Credential { .. …` | quota:: policy:: | SURVIVED | after repair: killed (errors_and_rejections_are_never_above_a_floor) |
| Q19 | quota.rs | `(28 * Self::DAY_SECS..=31 * Self::DAY_SECS)` → `(28 * Self::DAY_SECS..=30 * Self::DAY_SECS)` | quota::spans_are_named | SURVIVED | after repair: killed (spans_are_named_by_length) |
| Q20 | quota.rs | `Some(seconds) if seconds > 0 => Self::Other(seconds),` → `Some(seconds) => Self::Other(seconds),` | quota::spans_are_named | killed (spans_are_named_by_length) |  |
| Q21 | quota.rs | `self.shared() .max_by(most_used) .or_else(\|\| self.limits.i…` → `self.limits.iter().max_by(most_used)` | quota:: | killed (the_accessors_read_a_codex_shaped_reading) |  |
| Q22 | quota.rs | `None => limit.key().is_none(),` → `None => true,` | quota:: policy:: | SURVIVED | after repair: killed (the_accessors_read_a_claude_reading) |
| Q23 | quota.rs | `Scope::All => return false,` → `Scope::All => "",` | quota:: policy:: | SURVIVED | after repair: killed (the_accessors_read_a_codex_shaped_reading) |
| Q24 | quota.rs | `existing.window = window; existing.source = source;` → `*existing = Limit::new(span, scope, window, source…` | quota::setting_a_window | killed (setting_a_window_keeps_what_else_is_known_about_the_limit) |  |
| Q25 | quota.rs | `} else if lower.contains("has expired") { Some("expired")` → `} else if lower.contains("has expired") { Some("re…` | quota::read_errors | SURVIVED | after repair: killed (read_errors_group_by_cause) |
| Q26 | quota.rs | `.map(\|limit\| limit.window.remaining().clamp(0.0, 1.0))` → `.map(\|limit\| limit.window.remaining())` | quota:: | SURVIVED | after repair: killed (a_window_past_its_limit_has_no_room_rather_than_negative_room) |
| Q27 | quota.rs | `if remaining > 0.50 {` → `if remaining > 0.60 {` | quota:: | SURVIVED | after repair: killed (a_window_past_its_limit_has_no_room_rather_than_negative_room) |
| Q28 | quota.rs | `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub…` → `#[serde(skip_serializing_if = "Vec::is_empty")] pu…` | quota::readings_without | killed (readings_without_the_optional_fields_still_parse_and_do_not_grow_on_write) |  |
| Q29 | quota.rs | `#[serde(default)] pub limits: Vec<Limit>,` → `pub limits: Vec<Limit>,` | quota::readings_without | killed (readings_without_the_optional_fields_still_parse_and_do_not_grow_on_write) |  |
| Q30 | quota.rs | `.chain(weekly.map(\|window\| window.remaining())) .reduce(f6…` → `.chain(weekly.map(\|window\| window.remaining())) …` | quota:: forecast:: | killed (a_model_limit_that_is_a_share_of_the_weekly_caps_the_room +2) |  |
| Q31 | quota.rs | `SlotBusy, WeeklyLimit, BudgetExhausted, Ineligible, Off, #[s…` → `SlotBusy, WeeklyLimit, BudgetExhausted, Ineligible…` | quota::wire_enums | killed (wire_enums_parse_known_values_and_keep_the_unknown_ones) |  |
| Q32 | quota.rs | `#[serde(rename = "observed-rejection")]` → `(removed)` | quota::limit_serde | killed (limit_serde_keeps_the_stored_spelling) |  |
| Q33 | quota.rs | `#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord,…` → `#[derive(Debug, Clone, Copy, PartialEq, Eq, Partia…` | quota::limit_serde | SURVIVED | after repair: killed (limit_serde_keeps_the_stored_spelling) |
| Q34 | quota.rs | `#[serde(tag = "kind", rename_all = "snake_case")] pub enum R…` → `#[serde(tag = "type", rename_all = "snake_case")] …` | quota::read_errors | killed (read_errors_group_by_cause) |  |
| S1 | settings.rs | `self.services_interval_secs.max(MIN_SERVICES_INTERVAL_SECS)` → `self.services_interval_secs` | settings:: | killed (set_parses_each_key_and_rejects_the_rest) |  |
| S2 | settings.rs | `if self.dangerous_mode && !args.iter().any(\|arg\| arg == SK…` → `if self.dangerous_mode {` | settings:: | killed (dangerous_mode_adds_the_flag_once) |  |
| S3 | settings.rs | `if self.dangerous_mode && !args.iter().any(\|arg\| arg == SK…` → `if !args.iter().any(\|arg\| arg == SKIP) {` | settings:: launch:: commands::launch:: | killed (a_vendor_launch_carries_the_endpoint_key_and_models_and_drops_anthropic_credentials) |  |
| S4 | settings.rs | `Ok(secs) if secs >= MIN_SERVICES_INTERVAL_SECS => secs,` → `Ok(secs) => secs,` | settings:: | killed (set_parses_each_key_and_rejects_the_rest) |  |
| S5 | settings.rs | `if !(0.0..1.0).contains(&value) { bail!("rotation.{name} mus…` → `(removed)` | settings:: account:: | SURVIVED | after repair: killed (floors_must_nest) |
| S6 | settings.rs | `if self.sip_min_5h_remaining > self.normal_min_5h_remaining …` → `(removed)` | settings:: account:: | killed (floors_must_nest) |  |
| S7 | settings.rs | `if self.land_min_5h_remaining < self.normal_min_5h_remaining…` → `(removed)` | settings:: account:: | killed (floors_must_nest) |  |
| S8 | settings.rs | `if self.codex_land_min_5h_remaining < self.codex_min_5h_rema…` → `(removed)` | settings:: account:: | killed (the_codex_floors_are_set_by_name_and_must_nest) |  |
| S9 | settings.rs | `if self.normal_probe_interval_secs == 0 \|\| self.sip_probe_…` → `(removed)` | settings:: account:: | SURVIVED | after repair: killed (floors_must_nest) |
| S10 | settings.rs | `next.validate()?; *self = next;` → `*self = next;` | settings:: | killed (the_codex_floors_are_set_by_name_and_must_nest) |  |
| S11 | settings.rs | `Ok(fraction) if (0.0..1.0).contains(&fraction) => Ok(fractio…` → `Ok(fraction) => Ok(fraction),` | settings:: | SURVIVED | equivalent: `validate` refuses the same values one line later |
| S12 | settings.rs | `fn d_codex_auto_move() -> bool { true` → `fn d_codex_auto_move() -> bool { false` | settings:: | killed (the_codex_floors_are_set_by_name_and_must_nest) |  |
| S13 | settings.rs | `fn d_land_5h() -> f64 { 0.25` → `fn d_land_5h() -> f64 { 0.15` | settings:: policy:: | SURVIVED | a default's value; no rule to pin |
| S14 | settings.rs | `fn yes() -> bool { true` → `fn yes() -> bool { false` | settings:: account:: | SURVIVED | after repair: killed (the_codex_floors_are_set_by_name_and_must_nest) |
| S15 | settings.rs | `"true" \| "on" \| "1" => Ok(true), "false" \| "off" \| "0" =…` → `"true" \| "1" => Ok(true), "false" \| "off" \| "0"…` | settings::set_parses | SURVIVED | after repair: killed (set_parses_each_key_and_rejects_the_rest) |
| S16 | settings.rs | `"terminal" => self.terminal = (!value.is_empty()).then(\|\| …` → `"terminal" => self.terminal = Some(value.to_owned(…` | settings::set_parses | killed (set_parses_each_key_and_rejects_the_rest) |  |
| S17 | settings.rs | `Ok(secs) if secs >= 1 => secs,` → `Ok(secs) => secs,` | settings::set_parses | killed (set_parses_each_key_and_rejects_the_rest) |  |
| T1 | store.rs | `row: at - row >= HOLD_SECS,` → `row: at - row > HOLD_SECS,` | store:: | killed (an_unchanged_reading_adds_no_row_until_the_hold_is_ten_minutes_old +7) |  |
| T2 | store.rs | `.is_some_and(\|(_, stored)\| same_but_when(stored, &json));` → `.is_some_and(\|(_, stored)\| stored == &json);` | store:: | killed (an_unchanged_reading_adds_no_row_until_the_hold_is_ten_minutes_old +4) |  |
| T3 | store.rs | `closing: confirmed.filter(\|seen\| *seen > row && *seen < at…` → `closing: None,` | store:: | killed (an_unchanged_reading_adds_no_row_until_the_hold_is_ten_minutes_old +2) |  |
| T4 | store.rs | `closing: confirmed.filter(\|seen\| *seen > row && *seen < at…` → `closing: confirmed.filter(\|seen\| *seen < at),` | store:: | killed (old_snapshots_become_readings_that_say_what_they_said +4) |  |
| T4b | store.rs | `closing: confirmed.filter(\|seen\| *seen > row && *seen < at…` → `closing: confirmed.filter(\|seen\| *seen > row),` | store:: | SURVIVED | after repair: killed (the_growth_rule_case_by_case) |
| T5 | store.rs | `ON CONFLICT (service, account) DO UPDATE SET at = MAX(at, ex…` → `ON CONFLICT (service, account) DO NOTHING` | store:: | killed (a_read_only_open_sees_the_rows_and_creates_nothing +4) |  |
| T5b | store.rs | `ON CONFLICT (service, account) DO UPDATE SET at = MAX(at, ex…` → `ON CONFLICT (service, account) DO UPDATE SET at = …` | store:: | SURVIVED | after repair: killed (a_hold_is_one_accounts_of_one_service_and_its_confirmation_never_moves_back) |
| T6 | store.rs | `if let Some(held) = self.held(*service, Some(account))?.into…` → `(removed)` | store:: | killed (a_read_only_open_sees_the_rows_and_creates_nothing +2) |  |
| T7 | store.rs | `.filter(\|reading\| reading.at >= since), );` → `.filter(\|_\| false), );` | store:: | killed (an_unchanged_reading_adds_no_row_until_the_hold_is_ten_minutes_old +3) |  |
| T7b | store.rs | `.filter(\|reading\| reading.at >= since), );` → `.filter(\|_\| true), );` | store:: | SURVIVED | after repair: killed (a_hold_is_one_accounts_of_one_service_and_its_confirmation_never_moves_back) |
| T8 | store.rs | `WHERE c.service = ?1 AND (?2 IS NULL OR c.account = ?2) AND …` → `WHERE (?2 IS NULL OR c.account = ?2) AND c.at > r.…` | store:: | SURVIVED | after repair: killed (a_hold_is_one_accounts_of_one_service_and_its_confirmation_never_moves_back) |
| T8b | store.rs | `WHERE c.service = ?1 AND (?2 IS NULL OR c.account = ?2) AND …` → `WHERE c.service = ?1 AND c.at > r.at AND (?2 IS NU…` | store:: | killed (an_unchanged_reading_adds_no_row_until_the_hold_is_ten_minutes_old +1) |  |
| T9 | store.rs | `(hours > 0.0 && a.resets_at == b.resets_at && b.used >= a.us…` → `(hours > 0.0 && b.used >= a.used && rate.is_finite…` | store:: | SURVIVED | after repair: killed (a_burn_rate_counts_only_use_inside_one_reset_window) |
| T9b | store.rs | `(hours > 0.0 && a.resets_at == b.resets_at && b.used >= a.us…` → `(hours > 0.0 && a.resets_at == b.resets_at && rate…` | store:: | SURVIVED | after repair: killed (a_burn_rate_counts_only_use_inside_one_reset_window) |
| T10 | store.rs | `.map(\|(hours, rate)\| hours * rate) .sum::<f64>() / hours` → `.map(\|(_, rate)\| rate) .sum::<f64>() / intervals…` | store:: | killed (burn_rates_are_the_same_from_dense_and_sparse_rows +1) |  |
| T11 | store.rs | `known.sort_by_key(\|(series, _)\| series.scope != Scope::All…` → `(removed)` | store:: | SURVIVED | after repair: killed (a_burn_rate_counts_only_use_inside_one_reset_window) |
| T12 | store.rs | `conn.pragma_update(None, "synchronous", "NORMAL")?; crate::f…` → `conn.pragma_update(None, "synchronous", "NORMAL")?…` | store:: | SURVIVED | after repair: killed (a_hold_is_one_accounts_of_one_service_and_its_confirmation_never_moves_back) |
| T13 | store.rs | `readings.sort_by_key(\|reading\| std::cmp::Reverse(reading.a…` → `readings.sort_by_key(\|reading\| std::cmp::Reverse…` | store:: | killed (an_unchanged_reading_adds_no_row_until_the_hold_is_ten_minutes_old) |  |
| T14 | store.rs | `let hours_left = latest.filter(\|rate\| *rate > 0.0).and_the…` → `let hours_left = latest.and_then(\|rate\| {` | store:: | killed (burn_rates_are_the_same_from_dense_and_sparse_rows) |  |
| T15 | store.rs | `legacy::present(&self.conn).is_ok_and(\|tables\| !tables.is_…` → `legacy::present(&self.conn).is_ok()` | store:: | killed (old_snapshots_become_readings_that_say_what_they_said +1) |  |
| T16 | store.rs | `let conn = if wal.exists() { Connection::open_with_flags(pat…` → `let conn = if true { Connection::open_with_flags(p…` | store:: | killed (a_read_only_open_sees_the_rows_and_creates_nothing +2) |  |
| T17 | store.rs | `let conn = if wal.exists() { Connection::open_with_flags(pat…` → `let conn = if false { Connection::open_with_flags(…` | store:: | killed (a_database_held_by_another_connection_is_an_error_not_an_empty_history +1) |  |
| T18 | store.rs | `value.as_object_mut()?.remove("at"); Some(value)` → `value.as_object_mut()?.remove("at"); value.as_obje…` | store:: | SURVIVED | after repair: killed (a_hold_is_one_accounts_of_one_service_and_its_confirmation_never_moves_back) |
| U1 | util.rs | `if single.chars().count() <= max_chars {` → `if single.len() <= max_chars {` | util:: | SURVIVED | after repair: killed (one_line_flattens_and_caps_on_characters) |
| U2 | util.rs | `value.replace('\'', "'\\''")` → `value` | util:: | killed (a_quoted_word_survives_an_apostrophe) |  |
| U3 | util.rs | `let doublings = consecutive.saturating_sub(1).min(16);` → `let doublings = consecutive.min(16);` | util:: | killed (backoff_doubles_from_the_first_failure_to_the_cap) |  |
| U4 | util.rs | `first.saturating_mul(1 << doublings).min(cap)` → `first.saturating_mul(1 << doublings)` | util:: | killed (backoff_doubles_from_the_first_failure_to_the_cap) |  |
| U5 | util.rs | `.map_or(0, \|time\| time.timestamp())` → `.map_or(0, \|time\| time.naive_local().and_utc().t…` | util:: | SURVIVED | after repair: killed (a_missing_or_unreadable_reset_is_none_pending) |

### Deleted

None. Nothing in these modules restated its fixture or could not fail.

### Strengthened

Listed in the inventory (action `S`), each with the mutation it now catches: 30 tests.

### Added

| test | rule it pins | mutation seen to fail it |
|---|---|---|
| policy::force_decides_on_what_was_read_and_every_switch_says_why | `--force` decides on a partial view and over a healthy default; a default with no reading holds; each switch carries Forced / NoDefault / Refused / SipFloor | P30 P31 P32 P33 P34 P35 P36 |
| policy::dwell_is_room_above_each_windows_own_floor_as_a_share_of_what_the_floor_leaves | each window is ranked against its own floor, normalised | P22 P23 |
| policy::a_landing_is_held_to_the_landing_floor_of_each_window | "lands only on an account with real room", per window | P3 |
| quota::a_window_past_its_limit_has_no_room_rather_than_negative_room | a header window over 1.0 reads as no room; display levels | Q26 Q27 |
| forecast::the_rate_is_the_use_gained_inside_one_reset_window_over_the_time_it_covers | no rate across a reset, none from a value taken back, an hour is the least | F2 F3 F5 F21 |
| forecast::an_account_counts_through_its_longest_window | the weekly governs beside a 5h at its wall | F1 |
| observe::only_a_session_that_reaches_anthropic_is_bound_to_an_account | Bedrock / Vertex / third-party sessions are not charged to an account; the override | O26 O31 |
| observe::the_admission_state_forgets_passed_resets_and_old_sessions | prune | O22 O23 O27 |
| observe::clearing_a_quarantine_takes_one_accounts_limits_or_one_family_of_them | `rotate clear-quarantine [--family]` | O24 |
| observe::a_quarantine_needs_a_reset_ahead_and_a_model_of_its_family | `rotate quarantine` refusals | O29 O30 |
| observe::a_live_process_is_matched_to_its_hook_record_by_pid_and_start_time | pid reuse is not the same process | O25 O32 |
| store::the_growth_rule_case_by_case | `keep`: hold, row, closing row, each edge | T1 T3 T4 T4b |
| store::a_hold_is_one_accounts_of_one_service_and_its_confirmation_never_moves_back | hold per (service, account); 0600; failures that differ | T5b T7b T8 T12 T18 |
| store::a_burn_rate_counts_only_use_inside_one_reset_window | Stats intervals; account-wide first | T9 T9b T11 |
| compact::the_errors_old_snapshots_held_as_text_keep_their_kind | old error text keeps definitive / transient | L1 L5 |
| compact::old_rows_out_of_time_order_stop_the_run_with_nothing_dropped | text order is not time order: refuse, drop nothing | C19 |

Production code changed for testability only, no behaviour change (commit 213fb23): `observe::spends`,
`check_quarantine`, `clear_limits`, `bound_to` are the pure cores of `spends_anthropic_quota`, `quarantine`,
`clear_quarantine` and `binding_for_pid`.

### Bugs

None found.

Judged, not a bug: the `compact` lane's report that `Store::insert` writes no closing row when reads stop mid-hold and
resume with the same value. DESIGN.md's growth rule writes a row when the reading differs or the newest row is ten
minutes old, and "a history holds the first and last row of every hold". The same value on both sides of a gap is one
hold (within a reset window use only rises, and a reset changes `reset`, so "same" means nothing moved); a line drawn
through it is the value. When the value differs after the gap the closing row is written at the last confirmation.
Pinned in `store::the_growth_rule_case_by_case`. What a chart cannot show is that nobody was reading; that is a chart
question (g4), not a store one.

### Not tested and matters

| what | why it matters | why not done |
|---|---|---|
| `observe::record_session_from_hook` and `scan_transcripts` end to end (lock, state file, credential-timeline attribution, unconfigured account skipped) | this is the path that charges a wall to an account | they resolve `paths::admission_state` and `history::load` themselves; with the test home they open nothing. Needs a path argument or a state-dir seam in observe.rs and history.rs (g2). The pure steps under them are tested |
| `observe::apply` / `active_limits` | overlay of active limits on a probe | read the clock and the state file; `apply_limit` and `active_at` are tested |
| reset text's zone: `resets 11pm (America/New_York)` is read in the machine's local zone, the named zone is ignored | wrong by the offset if Claude Code's zone and the daemon's differ | no evidence they can differ on one machine; not pinned either way |
| forecast: a window whose reset passed since the read (F11) counts with the room it was read with | a stale stored view under-reports room | the design does not say which is right; left |
| forecast: first-use start of an idle window (F20) | none inside a 7-day horizon | unobservable for weekly windows |
| compact: free-space refusal (`room_for`, C22) | a full disk mid-rewrite | `fs2::available_space` has no seam |
| compact: rows and progress in one transaction (C13) | a kill between them would double rows | the test's kill points are between steps, not inside one; `verify` would refuse the result |
| policy purity ("never matches on Service", no clock, imports) | DESIGN rule | structural; not written |
| `LaunchSettings::command` with `TOKEMAN_CLAUDE_BIN` | which binary is launched | reads the process environment; the test ignores the binary |

### Cross-group

- g2: `history::load` / `paths::admission_state` injection would let observe's two IO entry points be tested (above).
- g4: `ui/chart` decides how a gap in reads is drawn; the store gives it first and last row of each hold only.

### Hygiene notes

- No test in these files sleeps, uses the network, or reads a real home. store and compact use tempdirs.
- `observe::reset_messages_parse_dates_and_roll_bare_clocks_to_tomorrow` computes and formats in `chrono::Local` on both
  sides, so it holds in any zone; its fixed local times (16:00 on 2026-07-27 and 2026-12-30) are in no DST gap anywhere.
- `observe::env_flag_parses_the_usual_spellings` reads one environment variable that nothing sets
  (`TOKEMAN_TEST_FLAG_UNSET_XYZ`); `settings::dangerous_mode_adds_the_flag_once` goes through `command()`, which reads
  `TOKEMAN_CLAUDE_BIN`, and ignores the binary it returns.
- `compact::another_process_is_kept_out_for_the_whole_run` re-runs the test binary as a child (`current_exe`,
  `--exact ... other_process --ignored`) with `TOKEMAN_TEST_OTHER_PROCESS_DB` set on the child only; the child opens
  with a zero busy timeout and exits. It is real: removing the exclusive lock (C1, C2) or closing a descriptor of the
  database mid-run (C3) fails it. The wait on the child is not bounded.
- observe's fixtures name the account `ember`; not an address.

---

## g2: account, anthropic/*, claude/*, claims, history, fs, launch (paths, testing)

Branch `lane/tests-g2` in `<scratchpad>/g2`, on lane/tests 55fc9a1 (merged at clean points; head 568d1e6).
Commits (oldest first): 9432639 tests no longer touch the live profile cache; c640d38 ask paths for the home directory
(keychain); 27a950e deletions; 468550f unit tests run no installed claude; 6407ad7 strengthened; 3adddb9 new tests; 568d1e6 redacting Debug for every credential-holding type.

**Top findings**
1. A test WROTE the live `~/.config/tokeman/claude-profile-reads.json` on every `cargo test`
   (`anthropic::probe::tests::a_bare_429_...` through `probe_all`'s default profile cache: entry `l|usage`,
   `fetched_at: 2`, still in the live file; not removed, that file is not ours to edit). Fixed in 9432639.
2. Every test that built an Anthropic `Client` ran the installed `claude --version` from PATH (up to 5 s bound,
   real binary). Fixed in 468550f.
3. `history::tests::a_revocation_is_not_an_offer` was vacuous: deleting the rule it names left it green (H1).
4. Nothing tested `Fleet::update` / `save` at all: the locked read-modify-write that DESIGN.md's credential rule
   rests on, and the 0600 mode of tokens.toml, had no test (A20). `history` compaction and
   `ResetGrant::weekly_refills` (what the forecast counts a Fable grant as) had none either.
5. `Login`, `Grant`, `Account`, `Credential`, `TokenResponse`, `Pkce`, `LoginBackup` and a secret `EnvVar` derived
   `Debug` with the token in it (no caller prints one today). Redacted in 568d1e6, one test per module.
6. No bug in what the code does was found in this group.

### Counts

Before = base 0248920. "part c" = a real test carrying a hollow tail (a serde round trip or a constant assertion).

| file | before | a | b | c | d | deleted | strengthened | added | after |
|---|---|---|---|---|---|---|---|---|---|
| account.rs | 12 | 9 | 3 | 0 | 0 | 0 | 3 | 2 | 14 |
| anthropic/mod.rs | 3 | 1 | 2 | 0 | 0 | 0 | 2 (1 replaced) | 0 | 3 |
| anthropic/oauth.rs | 6 | 4 (1 part c) | 2 (1 part c) | 0 | 0 | 0 (2 tails) | 1 | 1 | 7 |
| anthropic/probe.rs | 12 | 12 | 0 | 0 | 0 | 0 | 1 (+2 hygiene) | 1 | 13 |
| anthropic/profile.rs | 19 | 16 | 0 | 2 | 1 | 2 (+1 halved) | 2 | 1 | 18 |
| anthropic/resets.rs | 11 | 9 (2 part c) | 2 (2 part c) | 0 | 0 | 0 (4 tails) | 2 | 0 | 11 |
| claims.rs | 6 | 5 | 1 | 0 | 0 | 0 | 1 | 0 | 6 |
| history.rs | 5 | 3 | 1 | 1 | 0 | 0 | 2 (1 rewritten) | 1 | 6 |
| claude/settings.rs | 7 | 5 | 2 | 0 | 0 | 0 | 3 | 0 | 7 |
| claude/keychain.rs | 5 | 5 | 0 | 0 | 0 | 0 | 0 | 0 | 5 |
| claude/sessions.rs | 4 | 3 | 1 | 0 | 0 | 0 | 1 | 0 | 4 |
| fs.rs | 2 | 2 | 0 | 0 | 0 | 0 | 1 | 0 | 2 |
| launch.rs | 2 | 2 | 0 | 0 | 0 | 0 (1 gone with the compact merge) | 0 | 0 | 1 |
| **total** | **94** | **76** | **14** | **3** | **1** | **2 whole, 1 halved, 6 hollow tails** | **19** | **6** | **97** |

After: 97 tests, every one (a) by reading; 0 known b/c/d. paths.rs and testing.rs have no tests of mine (the
root auditor owns the paths.rs seam test).

Mutations: 204 runs (the last 12, S1-S12, put each credential back into its `Debug`; all killed). First pass 158 (m1 55, m2 103): 122 killed, 33 SURVIVED, 3 did not compile. Of the 33
survivors, 25 are now killed by a strengthened or new test (verification pass: 34 runs, 33 killed, 1 did not
compile), 3 are equivalent mutants, 5 are left and named in the table. 60 of the first-pass mutations were aimed
at tests I had judged (a).

### Inventory

Classes: (a) pins behaviour that matters, (b) meaningful but weak, (c) hollow, (d) redundant. "part" marks a test
whose hollow or weak part sits beside real assertions.

| file | test | claim | class | action |
|---|---|---|---|---|
| account.rs | a_live_login_is_preferred_over_the_setup_key | a live login outranks the setup key | a | kept |
| account.rs | a_refused_or_expiring_login_falls_back_to_the_setup_key | refused or <2 min login is not installed | a | kept |
| account.rs | a_captured_access_token_without_refresh_is_never_installed | a token with no refresh reads the profile only | b | strengthened (profile token expiry) |
| account.rs | refresh_is_due_inside_the_margin_but_not_for_a_fresh_token | when a refresh is due | b | strengthened (expired but young) |
| account.rs | a_grant_without_a_refresh_token_keeps_the_old_one | single-use refresh token survives a grant without one | a | kept |
| account.rs | rendering_keeps_comments | comments survive a write | a | kept |
| account.rs | an_empty_key_is_no_setup_key_and_an_unknown_rotation_key_is_ignored | empty key, unknown [rotation] key | a | kept |
| account.rs | re_adding_an_account_keeps_its_login_grant | `claude add` keeps the grant | a | kept |
| account.rs | a_login_remembers_its_date_and_ages_out_of_warning_range | login date, 21-day warning | a | kept |
| account.rs | services_keep_their_comments_and_replace_by_name | [[services]] upsert/remove/render | b | strengthened (order kept) |
| account.rs | a_reported_refresh_expiry_wins_over_the_login_age_and_never_slides | earlier refresh expiry is kept | a | kept |
| account.rs | render_round_trips_every_table | every table survives render -> parse | a | kept (a field dropped from render fails it) |
| anthropic/mod.rs | the_user_agent_is_claude_codes_shape | UA shape | b | replaced: ran the installed `claude`; now the pure version parser |
| anthropic/mod.rs | describe_joins_the_cause_chain_once | cause chain joined without repeats | b | strengthened (fixture had no repeat) |
| anthropic/mod.rs | credentials_pick_their_auth_scheme | Bearer for OAuth tokens, x-api-key otherwise | a | kept |
| anthropic/oauth.rs | authorize_url_carries_profile_scope_and_a_challenge | PKCE URL | a | kept |
| anthropic/oauth.rs | a_foreign_or_empty_paste_is_refused_before_any_request | state checked before any request | a | kept |
| anthropic/oauth.rs | only_revoked_or_redeemed_grants_are_permanent | which refusals end a grant | a | kept |
| anthropic/oauth.rs | token_responses_round_trip_into_grants | TokenResponse -> Grant | a, part c | serde round trip removed |
| anthropic/oauth.rs | a_refresh_states_its_scopes_and_returns_a_grant | refresh body, permanent refusal | b | strengthened (one request per refresh) |
| anthropic/oauth.rs | an_exchange_waits_out_the_token_endpoints_429s | exchange retries 429 three times | b, part c | constant assertion removed; only a 429 is retried |
| anthropic/probe.rs | a_healthy_account_may_report_only_its_weekly_window | weekly-only headers (2026-10-31) | a | kept |
| anthropic/probe.rs | unified_headers_become_a_quota_and_their_absence_none | header parse, missing reset, garbage | a | kept |
| anthropic/probe.rs | a_fable_wall_keeps_its_overshoot_and_every_signal_is_recorded | 1.02 kept, per-window status, signals | a | kept |
| anthropic/probe.rs | a_reported_5h_window_marks_the_limit_present | 5h header -> Present | a | kept |
| anthropic/probe.rs | only_full_ids_of_header_only_families_are_sampled | which models are sampled | a | kept |
| anthropic/probe.rs | the_model_weekly_window_becomes_a_fable_bucket_unless_the_profile_has_one | 7d_oi -> Fable limit | a | kept |
| anthropic/probe.rs | an_account_without_a_credential_is_refused_without_a_request | no credential, no request | a | hygiene fix (resolved the live config dir) |
| anthropic/probe.rs | a_probe_reads_the_headers_and_classifies_refusals | end-to-end probe, 401, 429 with headers | a | kept |
| anthropic/probe.rs | a_bare_429_is_a_refusal_and_the_profile_row_outranks_the_headers | bare 429, profile row wins | a | hygiene fix (WROTE the live profile cache) |
| anthropic/probe.rs | a_timeout_is_a_network_error | timeout -> Network, not refused | a | kept |
| anthropic/probe.rs | every_header_seen_or_read_by_claude_code_is_known | known headers report nothing | a | strengthened (usage-credit window becomes a limit) |
| anthropic/probe.rs | a_header_a_window_or_a_word_it_does_not_know_lands_in_the_reading | unknown header/word -> unrecognized | a | kept |
| anthropic/profile.rs | rate_limits_back_off_exponentially_and_honor_retry_after | 429 backoff schedule | a | kept |
| anthropic/profile.rs | profile_usage_windows_normalize_percent_and_stay_unknown_when_incomplete | legacy window percent/ISO/null | d | deleted; its three cases moved into the_legacy_per_family_fields_are_not_read as real bodies |
| anthropic/profile.rs | the_legacy_per_family_fields_are_not_read | seven_day_opus ignored | a | strengthened (both legacy windows, a null one) |
| anthropic/profile.rs | dynamic_scoped_weekly_buckets_preserve_model_identity | scoped rows keyed by model id | a | kept |
| anthropic/profile.rs | idle_null_reset_and_odd_rows_do_not_hide_model_buckets | null reset, garbled row, unknown kind | a | kept |
| anthropic/profile.rs | a_body_without_a_session_row_has_no_5h_limit_and_fable_is_a_share_of_the_weekly | today's Ultra body | a | kept |
| anthropic/profile.rs | a_session_row_marks_the_5h_limit_present_and_a_model_id_keys_the_bucket | session row, model id, legacy fallback | a | kept |
| anthropic/profile.rs | merging_fills_only_what_the_headers_left_out_and_keeps_a_seen_5h_window | merge precedence | a | kept |
| anthropic/profile.rs | scoped_rows_with_the_same_key_collapse_to_the_first | duplicate keys | a | kept |
| anthropic/profile.rs | response_shapes_round_trip | serde derive round trip | c | deleted |
| anthropic/profile.rs | cache_files_round_trip_and_an_older_shape_reads_as_empty | cache file serde; 0.3 shape reads empty | c, part a | round trip removed, the 0.3-shape half kept under its own name |
| anthropic/profile.rs | merging_replaces_same_keyed_buckets_or_records_the_failure | merge replaces a carried bucket | a | kept |
| anthropic/profile.rs | a_young_answer_is_reused_and_a_second_accounts_429_holds_back_every_account | cache age, per-account and fleet 429 | a | strengthened (`fresh` skips a young answer) |
| anthropic/profile.rs | a_success_in_flight_during_another_accounts_429_keeps_the_backoff | concurrent success keeps backoff | a | kept |
| anthropic/profile.rs | other_failures_hold_back_only_their_account_and_the_file_is_private | 5xx holds one account; 0600 | a | kept |
| anthropic/profile.rs | usage_needs_a_profile_token_and_reports_parse_failures | no token -> None; bad body -> error | a | kept |
| anthropic/profile.rs | todays_cache_file_loads_and_an_entry_it_cannot_read_is_dropped_alone | real cache shape, lenient entries | a | kept |
| anthropic/profile.rs | todays_body_carries_nothing_unrecognised | today's keys are known | a | kept |
| anthropic/profile.rs | a_key_a_kind_or_a_scope_it_does_not_know_is_reported_and_lands_in_the_reading | drift paths | a | kept |
| anthropic/resets.rs | a_live_status_block_parses_and_describes | real cedar_ember block | a, part c | serde round trip removed |
| anthropic/resets.rs | a_grant_counts_only_while_unpaused_started_and_unexpired | grant state machine, Fable grant wording | a, part c | serde round trip removed; weekly_refills pinned |
| anthropic/resets.rs | a_program_the_server_did_not_evaluate_is_unknown_not_zero | surface/unknown -> Unknown | a | strengthened (a session reset on offer) |
| anthropic/resets.rs | claim_results_are_named_and_unknown_ones_survive | claim result words | b, part c | round trip removed |
| anthropic/resets.rs | grant_shapes_default_their_optional_fields | minimal grant | b, part c | round trip removed |
| anthropic/resets.rs | each_program_comes_from_its_own_read_through_the_profile_token | two reads, stored_grants | a | kept |
| anthropic/resets.rs | an_uncertain_claim_is_resent_as_itself_and_blocks_any_other_grant | the claim-journal rule over HTTP | a | kept |
| anthropic/resets.rs | a_claim_left_pending_by_an_earlier_build_is_honoured | 0.3 pending file | a | kept |
| anthropic/resets.rs | a_failed_organization_lookup_journals_nothing | nothing journaled before a request | a | kept |
| anthropic/resets.rs | the_program_blocks_seen_so_far_carry_nothing_unrecognised | known program keys | a | kept |
| anthropic/resets.rs | a_program_field_a_grant_type_or_a_window_it_does_not_know_is_reported | drift paths | a | kept |
| claims.rs | request_ids_are_v4_uuids_and_fit_claudes_format | id format | a | kept |
| claims.rs | one_claim_per_service_and_account_until_it_is_answered | the journal rule | a | kept |
| claims.rs | the_file_layout_is_one_list_of_entries | on-disk layout | a | kept |
| claims.rs | both_old_layouts_are_seen_without_a_write_and_folded_in_by_the_first_change | 0.3 layouts | a | kept |
| claims.rs | an_entry_of_the_new_file_wins_over_an_old_file_for_the_same_account | new entry wins | b | strengthened (the old entry must not come back) |
| claims.rs | an_old_file_that_does_not_parse_blocks_its_account_until_cleared | unreadable old file blocks | a | kept |
| history.rs | a_live_process_follows_a_switch_before_its_token_expires | account_at follows the newest offer | b | strengthened (the instant of the switch) |
| history.rs | a_revocation_is_not_an_offer | revoke ignored | c (vacuous: the revoked account was the offered one) | rewritten |
| history.rs | a_pause_offers_no_managed_credential_and_nothing_is_known_before_the_first_event | pause, before first event | a | kept |
| history.rs | fingerprints_are_stable_and_short | fingerprint 16 hex | a | kept |
| history.rs | events_keep_their_stored_spelling | on-disk spelling, unknown kind | a | kept |
| claude/settings.rs | installing_a_login_declares_its_scopes_and_a_401_wait | install env block and event | a | strengthened (event time in seconds) |
| claude/settings.rs | falling_back_to_a_setup_token_withdraws_the_scope_claim | fallback drops scopes | a | kept |
| claude/settings.rs | reinstall_reasons_distinguish_upgrade_refresh_and_repair | reinstall reasons | b | strengthened (marker and scopes repairs) |
| claude/settings.rs | clearing_removes_every_managed_key | clear | a | kept |
| claude/settings.rs | observer_hook_detection_does_not_match_unrelated_hooks | hook detection, quoting | a | kept |
| claude/settings.rs | observer_hook_install_is_idempotent_and_preserves_other_hooks | hook install/remove | a | kept |
| claude/settings.rs | settings_round_trip_as_an_owner_only_file | load/save | b | strengthened (name said owner-only, nothing asserted the mode) |
| claude/keychain.rs | keychain_suppression_preserves_mcp_and_unknown_fields | only the login leaves the item | a | kept |
| claude/keychain.rs | keychain_restore_never_overwrites_a_newer_manual_login | restore | a | kept |
| claude/keychain.rs | keychain_transform_rejects_non_object_documents | non-object refused | a | kept |
| claude/keychain.rs | an_empty_keychain_does_not_erase_the_captured_login_backup | settled keeps the capture | a | kept |
| claude/keychain.rs | the_backup_keeps_its_stored_spelling | backup file spelling | a | kept |
| claude/sessions.rs | recognizes_only_real_claude_process_rows | ps row parse | b | strengthened (start time compared with itself) |
| claude/sessions.rs | a_live_session_on_a_drained_account_is_not_reported_as_stranded | timeline beats binding | a | kept |
| claude/sessions.rs | the_session_start_record_stands_in_only_without_history | binding fallback | a | kept |
| claude/sessions.rs | summaries_count_per_account_and_mark_bindings | summary line | a | kept |
| fs.rs | a_held_lock_is_seen_as_contended_and_released_on_drop | exclusive lock | a | kept |
| fs.rs | atomic_writes_land_owner_only | 0600 file, 0700 dir | a | strengthened (append_line mode) |
| launch.rs | iterm_script_sets_the_badge_once_and_escaped | AppleScript escaping | a | gone with the `compact` merge (egui tray deleted), not by this audit |
| launch.rs | a_launch_seats_the_account_in_the_childs_environment_only | env set/unset in the child | a | kept |

### Mutations

| id | file | change | tests run | outcome (tests that failed) | follow-up |
|---|---|---|---|---|---|
| A1 | account.rs | `let login = self.login.as_ref().filter(\|login\| login.is_refreshable())?;` -> `let login = self.login.as_ref()?;` | account::tests:: | killed: a_captured_access_token_without_refresh_is_never_installed,a_refused_or_expiring_login_falls_back_to_the_setup_key |  |
| A2 | account.rs | `expires_at - now_ms < MIN_INSTALL_LIFETIME_MS` -> `expires_at - now_ms < 0` | account::tests:: | killed: a_refused_or_expiring_login_falls_back_to_the_setup_key |  |
| A3 | account.rs | `self.login_credential(now_ms) / .or_else(\|\| self.setup_key.as_deref().map(Credential::Setu` -> `self.setup_key / .as_deref() / .map(Credential::Setup) / .or_else(\|\| self.login_credential` | account::tests:: | killed: a_live_login_is_preferred_over_the_setup_key |  |
| A4 | account.rs | `.is_none_or(\|obtained_at\| now_ms - obtained_at >= MIN_REFRESH_AGE_MS);` -> `.is_none_or(\|obtained_at\| now_ms - obtained_at >= 0);` | account::tests:: | killed: refresh_is_due_inside_the_margin_but_not_for_a_fresh_token |  |
| A5 | account.rs | `expires_at <= now_ms \|\| (expires_at - now_ms <= REFRESH_MARGIN_MS && old_enough)` -> `(expires_at - now_ms <= REFRESH_MARGIN_MS && old_enough)` | account::tests:: | SURVIVED | strengthened: an expired but young token (A5r killed) |
| A6 | account.rs | `if grant.refresh_token.is_some() { / login.refresh_token = grant.refresh_token; / }` -> `login.refresh_token = grant.refresh_token;` | account::tests:: | killed: a_grant_without_a_refresh_token_keeps_the_old_one |  |
| A7 | account.rs | `Some(stored.min(reported))` -> `Some(stored.max(reported))` | account::tests:: | killed: a_reported_refresh_expiry_wins_over_the_login_age_and_never_slides |  |
| A8 | account.rs | `(stored, reported) => reported.or(stored),` -> `(_, reported) => reported,` | account::tests:: | killed: a_reported_refresh_expiry_wins_over_the_login_age_and_never_slides |  |
| A9 | account.rs | `login.refresh_expires_at = refresh_expires_at;` -> `(removed)` | account::tests:: | killed: a_reported_refresh_expiry_wins_over_the_login_age_and_never_slides |  |
| A10 | account.rs | `login.error = None;` -> `(removed)` | account::tests:: | killed: a_grant_without_a_refresh_token_keeps_the_old_one |  |
| A11 | account.rs | `if !grant.scopes.is_empty() { / login.scopes = grant.scopes; / }` -> `login.scopes = grant.scopes;` | account::tests:: | killed: a_grant_without_a_refresh_token_keeps_the_old_one |  |
| A12 | account.rs | `set_opt( / table, / "refresh_token", / record.refresh_token.as_deref().map(value), / );` -> `(removed)` | account::tests:: | killed: render_round_trips_every_table,rendering_keeps_comments |  |
| A13 | account.rs | `set_opt(table, "parked", account.parked.then(\|\| value(true)));` -> `(removed)` | account::tests:: | killed: render_round_trips_every_table |  |
| A14 | account.rs | `\|\| record.refresh_token.is_some() / \|\| record.login_error.is_some();` -> `\|\| record.refresh_token.is_some();` | account::tests:: | SURVIVED | left: a refused login always keeps its refresh token, so a record with only login_error is not written by this build |
| A15 | account.rs | `setup_key: record.key.filter(\|key\| !key.is_empty()),` -> `setup_key: record.key,` | account::tests:: | killed: an_empty_key_is_no_setup_key_and_an_unknown_rotation_key_is_ignored |  |
| A16 | account.rs | `Some(account) => account.setup_key = Some(key),` -> `Some(account) => { / account.setup_key = Some(key); / account.login = None; / }` | account::tests:: | killed: re_adding_an_account_keeps_its_login_grant |  |
| A17 | account.rs | `Some(expires_at) => now_ms >= expires_at - REFRESH_WARNING_MS,` -> `Some(expires_at) => now_ms > expires_at - REFRESH_WARNING_MS,` | account::tests:: | killed: a_reported_refresh_expiry_wins_over_the_login_age_and_never_slides |  |
| A18 | account.rs | `.is_some_and(\|expires_at\| expires_at <= now_ms)` -> `.is_some_and(\|expires_at\| expires_at <= i64::MIN)` | account::tests:: | SURVIVED | strengthened: profile token expiry (A18r killed) |
| A19 | account.rs | `Some(index) => by_name.remove(index).1,` -> `Some(_) => Table::new(),` | account::tests:: | killed: rendering_keeps_comments,services_keep_their_comments_and_replace_by_name |  |
| A20 | account.rs | `write_atomic(&path, self.render(&existing)?.as_bytes(), 0o600)` -> `write_atomic(&path, self.render(&existing)?.as_bytes(), 0o644)` | account::tests:: | SURVIVED | no test wrote the file at all; added the update_at test (A20r killed) |
| A21 | account.rs | `.is_some_and(\|days\| days >= LOGIN_WARNING_DAYS),` -> `.is_some_and(\|days\| days > LOGIN_WARNING_DAYS),` | account::tests:: | killed: a_login_remembers_its_date_and_ages_out_of_warning_range |  |
| A22 | account.rs | `None => self.services.push(account),` -> `None => self.services.insert(0, account),` | account::tests:: | SURVIVED | strengthened: order of [[services]] (A22r killed) |
| B1 | anthropic/mod.rs | `value.starts_with("sk-ant-oat01-")` -> `value.starts_with("sk-ant-")` | anthropic::tests:: | killed: credentials_pick_their_auth_scheme |  |
| B2 | anthropic/mod.rs | `if !parts.iter().any(\|part\| part.contains(&text)) {` -> `if true {` | anthropic::tests:: | SURVIVED | strengthened: fixture now repeats its cause (B2r killed) |
| B3 | anthropic/mod.rs | `"claude-cli/{} (external, cli)"` -> `"claude-cli/{} (external)"` | anthropic::tests:: | killed: the_user_agent_is_claude_codes_shape |  |
| C1 | anthropic/oauth.rs | `if status == 401 \|\| code.as_deref() == Some("invalid_grant") {` -> `if code.as_deref() == Some("invalid_grant") {` | anthropic::oauth::tests:: | killed: only_revoked_or_redeemed_grants_are_permanent |  |
| C2 | anthropic/oauth.rs | `if status == 401 \|\| code.as_deref() == Some("invalid_grant") {` -> `if status == 401 \|\| body.contains("invalid_grant") {` | anthropic::oauth::tests:: | killed: only_revoked_or_redeemed_grants_are_permanent |  |
| C3 | anthropic/oauth.rs | `if state.is_some_and(\|state\| state != pkce.state) {` -> `if false {` | anthropic::oauth::tests:: | killed: a_foreign_or_empty_paste_is_refused_before_any_request |  |
| C4 | anthropic/oauth.rs | `"code": code,` -> `"code": pasted,` | anthropic::oauth::tests:: | killed: an_exchange_waits_out_the_token_endpoints_429s |  |
| C5 | anthropic/oauth.rs | `Err(error) if error.is_rate_limited() => match waits.next() {` -> `Err(error) if false => match waits.next() {` | anthropic::oauth::tests:: | killed: an_exchange_waits_out_the_token_endpoints_429s |  |
| C6 | anthropic/oauth.rs | `let scope = if scopes.is_empty() {` -> `let scope = if !scopes.is_empty() {` | anthropic::oauth::tests:: | killed: a_refresh_states_its_scopes_and_returns_a_grant |  |
| C7 | anthropic/oauth.rs | `.map(\|seconds\| now_ms + seconds * 1000),` -> `.map(\|seconds\| now_ms + seconds),` | anthropic::oauth::tests:: | killed: a_refresh_states_its_scopes_and_returns_a_grant,token_responses_round_trip_into_grants |  |
| C8 | anthropic/oauth.rs | `URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));` -> `URL_SAFE_NO_PAD.encode(verifier.as_bytes());` | anthropic::oauth::tests:: | killed: authorize_url_carries_profile_scope_and_a_challenge |  |
| C9 | anthropic/oauth.rs | `Err(error) if error.is_rate_limited() => match waits.next() {` -> `Err(error) if matches!(error, Error::Refused { .. }) => match waits.next() {` | anthropic::oauth::tests:: | SURVIVED | added a_refused_code_or_refresh_token_is_sent_once (C9r killed) |
| C10 | anthropic/oauth.rs | `if code.is_empty() { / return Err(Error::Paste("no authorization code was pasted")); / }` -> `(removed)` | anthropic::oauth::tests:: | killed: a_foreign_or_empty_paste_is_refused_before_any_request |  |
| C11 | anthropic/oauth.rs | `"code_verifier": pkce.verifier,` -> `"code_verifier": pkce.state,` | anthropic::oauth::tests:: | killed: an_exchange_waits_out_the_token_endpoints_429s |  |
| D1 | anthropic/probe.rs | `Some(Window::new(used, resets_at))` -> `Some(Window::new(used.min(1.0), resets_at))` | anthropic::probe::tests:: | killed: a_fable_wall_keeps_its_overshoot_and_every_signal_is_recorded |  |
| D2 | anthropic/probe.rs | `session: header_window(headers, &format!("{PREFIX}-5h")),` -> `session: header_window(headers, &format!("{PREFIX}-7d")),` | anthropic::probe::tests:: | killed: a_fable_wall_keeps_its_overshoot_and_every_signal_is_recorded,a_healthy_account_may_report_only_its_weekly_window,a_probe_reads_the_headers_and_classifies_refusals,a_reported_5h_window_marks_the_limit_present,unified_headers_become_a_quota_and_their_absence_none |  |
| D3 | anthropic/probe.rs | `if !status.is_success() && (status.as_u16() != 429 \|\| quota.is_none()) {` -> `if !status.is_success() && status.as_u16() != 429 {` | anthropic::probe::tests:: | killed: a_bare_429_is_a_refusal_and_the_profile_row_outranks_the_headers |  |
| D4 | anthropic/probe.rs | `if quota.session.is_some() { / claude.session_limit = SessionLimit::Present; / }` -> `(removed)` | anthropic::probe::tests:: | killed: a_reported_5h_window_marks_the_limit_present |  |
| D5 | anthropic/probe.rs | `&& !reading.limits.iter().any(is_fable)` -> `(removed)` | anthropic::probe::tests:: | killed: a_bare_429_is_a_refusal_and_the_profile_row_outranks_the_headers,the_model_weekly_window_becomes_a_fable_bucket_unless_the_profile_has_one |  |
| D6 | anthropic/probe.rs | `limit.status = Some(status);` -> `let _ = (limit, status);` | anthropic::probe::tests:: | killed: a_fable_wall_keeps_its_overshoot_and_every_signal_is_recorded |  |
| D7 | anthropic/probe.rs | `(base.starts_with("claude-") && HEADER_ONLY_FAMILIES.contains(&family)).then_some(base)` -> `HEADER_ONLY_FAMILIES.contains(&family).then_some(base)` | anthropic::probe::tests:: | killed: only_full_ids_of_header_only_families_are_sampled |  |
| D8 | anthropic/probe.rs | `KNOWN_WINDOWS.contains(&window) && KNOWN_WINDOW_FIELDS.contains(&field)` -> `KNOWN_WINDOWS.contains(&window) && !field.is_empty()` | anthropic::probe::tests:: | killed: a_header_a_window_or_a_word_it_does_not_know_lands_in_the_reading |  |
| D9 | anthropic/probe.rs | `status,` -> `status: status.and(None),` | anthropic::probe::tests:: | killed: a_fable_wall_keeps_its_overshoot_and_every_signal_is_recorded |  |
| D10 | anthropic/probe.rs | `reading / .limits / .push(Limit::extra_usage(window, Source::Headers));` -> `let _ = window;` | anthropic::probe::tests:: | SURVIVED | strengthened every_header... (D10r killed) |
| D11 | anthropic/probe.rs | `fallback_available: fallback.as_deref() == Some("available"),` -> `fallback_available: fallback.is_some(),` | anthropic::probe::tests:: | SURVIVED | left: display-only flag, only the word `available` has been seen |
| D12 | anthropic/probe.rs | `claude.verdict = quota.verdict;` -> `(removed)` | anthropic::probe::tests:: | killed: a_probe_reads_the_headers_and_classifies_refusals |  |
| D13 | anthropic/probe.rs | `let text = response.text().await.unwrap_or_default(); / return Err(http_error(status, text` -> `return Err(http_error(status, String::new()));` | anthropic::probe::tests:: | killed: a_probe_reads_the_headers_and_classifies_refusals |  |
| D14 | anthropic/probe.rs | `profile::merge(&mut reading, usage);` -> `(removed)` | anthropic::probe::tests:: | killed: a_bare_429_is_a_refusal_and_the_profile_row_outranks_the_headers |  |
| D15 | anthropic/probe.rs | `model_weekly: header_window(headers, &format!("{PREFIX}-7d_oi")),` -> `model_weekly: None,` | anthropic::probe::tests:: | killed: a_bare_429_is_a_refusal_and_the_profile_row_outranks_the_headers,a_fable_wall_keeps_its_overshoot_and_every_signal_is_recorded,a_probe_reads_the_headers_and_classifies_refusals,the_model_weekly_window_becomes_a_fable_bucket_unless_the_profile_has_one,unified_headers_become_a_quota_and_their_absence_none |  |
| D16 | anthropic/probe.rs | `let Some(credential) = account.credential(now * 1000) else {` -> `let Some(credential) = account.credential(now) else {` | anthropic::probe::tests:: | SURVIVED | added an_expired_login_token_is_not_sent (D16r killed) |
| D17 | anthropic/probe.rs | `reading.unrecognized = quota.unrecognized.clone();` -> `(removed)` | anthropic::probe::tests:: | killed: a_header_a_window_or_a_word_it_does_not_know_lands_in_the_reading |  |
| D18 | anthropic/probe.rs | `(KNOWN_WINDOWS.contains(&window) && field == "status").then_some(KNOWN_VERDICTS)?` -> `(field == "status").then_some(KNOWN_VERDICTS)?` | anthropic::probe::tests:: | killed: every_header_seen_or_read_by_claude_code_is_known |  |
| D19 | anthropic/probe.rs | `(verdict.is_some() \|\| surpassed_threshold.is_some()).then(\|\| WindowStatus {` -> `verdict.is_some().then(\|\| WindowStatus {` | anthropic::probe::tests:: | SURVIVED | left: a threshold header without a status header has not been seen; display only |
| E1 | anthropic/profile.rs | `percent.max(0.0) / 100.0` -> `percent.clamp(0.0, 100.0) / 100.0` | anthropic::profile::tests:: | killed: a_body_without_a_session_row_has_no_5h_limit_and_fable_is_a_share_of_the_weekly,todays_cache_file_loads_and_an_entry_it_cannot_read_is_dropped_alone |  |
| E2 | anthropic/profile.rs | `profile.session_limit = if has_session {` -> `profile.session_limit = if !has_session {` | anthropic::profile::tests:: | killed: a_body_without_a_session_row_has_no_5h_limit_and_fable_is_a_share_of_the_weekly,a_session_row_marks_the_5h_limit_present_and_a_model_id_keys_the_bucket,merging_fills_only_what_the_headers_left_out_and_keeps_a_seen_5h_window,todays_cache_file_loads_and_an_entry_it_cannot_read_is_dropped_alone |  |
| E3 | anthropic/profile.rs | `if profile.session.is_some() { / profile.session_limit = SessionLimit::Present; / }` -> `profile.session_limit = if profile.session.is_some() { / SessionLimit::Present / } else { ` | anthropic::profile::tests:: | killed: a_session_row_marks_the_5h_limit_present_and_a_model_id_keys_the_bucket,idle_null_reset_and_odd_rows_do_not_hide_model_buckets |  |
| E4 | anthropic/profile.rs | `.filter_map(\|(index, row)\| Some((index, serde_json::from_value(row.clone()).ok()?)))` -> `.filter_map(\|row\| serde_json::from_value(row.clone()).ok())` | anthropic::profile::tests:: | killed: a_body_without_a_session_row_has_no_5h_limit_and_fable_is_a_share_of_the_weekly |  |
| E5 | anthropic/profile.rs | `if row.kind != LimitKind::WeeklyScoped { / return None; / }` -> `(removed)` | anthropic::profile::tests:: | SURVIVED | left: no session/weekly_all row carries a model scope in any recorded body |
| E6 | anthropic/profile.rs | `let key = model / .id / .as_deref() / .map(ModelKey::new) / .filter(\|key\| !key.is_empty())` -> `let key = ModelKey::new(&model.display_name);` | anthropic::profile::tests:: | killed: a_session_row_marks_the_5h_limit_present_and_a_model_id_keys_the_bucket |  |
| E7 | anthropic/profile.rs | `if let Some(window) = window.filter(\|window\| window.resets_at > at)` -> `if let Some(window) = window.filter(\|_\| at >= 0)` | anthropic::profile::tests:: | killed: merging_fills_only_what_the_headers_left_out_and_keeps_a_seen_5h_window |  |
| E8 | anthropic/profile.rs | `&& reading.window(span, &Scope::All).is_none()` -> `(removed)` | anthropic::profile::tests:: | killed: merging_fills_only_what_the_headers_left_out_and_keeps_a_seen_5h_window |  |
| E9 | anthropic/profile.rs | `if has_session { / claude.session_limit = SessionLimit::Present; / }` -> `(removed)` | anthropic::profile::tests:: | SURVIVED | equivalent: either of two guards alone keeps Present (redundant code, not a test gap) |
| E10 | anthropic/profile.rs | `&& (self.accounts.len() >= FLEET_429_ACCOUNTS` -> `&& (!self.accounts.is_empty()` | anthropic::profile::tests:: | killed: a_young_answer_is_reused_and_a_second_accounts_429_holds_back_every_account |  |
| E11 | anthropic/profile.rs | `if now >= self.retry_after { / *self = Self::default(); / }` -> `let _ = now; / *self = Self::default();` | anthropic::profile::tests:: | killed: a_success_in_flight_during_another_accounts_429_keeps_the_backoff,a_young_answer_is_reused_and_a_second_accounts_429_holds_back_every_account |  |
| E12 | anthropic/profile.rs | `if now >= self.retry_after { / self.accounts.clear(); / }` -> `let _ = now;` | anthropic::profile::tests:: | SURVIVED | added a_429_after_the_backoff_ran_out... (E12r killed) |
| E13 | anthropic/profile.rs | `&& !fresh` -> `(removed)` | anthropic::profile::tests:: | SURVIVED | strengthened: fresh read of a young answer (E13r killed) |
| E14 | anthropic/profile.rs | `match entry.body { / Some(body) if !fresh => Ok(body), / _ => bail!("{}", failure.message)` -> `bail!("{}", failure.message)` | anthropic::profile::tests:: | killed: a_young_answer_is_reused_and_a_second_accounts_429_holds_back_every_account |  |
| E15 | anthropic/profile.rs | `.max(retry_after.unwrap_or(0))` -> `.min(retry_after.unwrap_or(i64::MAX))` | anthropic::profile::tests:: | killed: a_young_answer_is_reused_and_a_second_accounts_429_holds_back_every_account,rate_limits_back_off_exponentially_and_honor_retry_after |  |
| E16 | anthropic/profile.rs | `let _ = write_atomic(&self.path, &bytes, 0o600);` -> `let _ = write_atomic(&self.path, &bytes, 0o644);` | anthropic::profile::tests:: | killed: other_failures_hold_back_only_their_account_and_the_file_is_private |  |
| E17 | anthropic/profile.rs | `#[serde(default, deserialize_with = "crate::fs::lenient_map")]` -> `#[serde(default)]` | anthropic::profile::tests:: | killed: todays_cache_file_loads_and_an_entry_it_cannot_read_is_dropped_alone |  |
| E18 | anthropic/profile.rs | `Self::AtWall => format!("{account}\|at_wall"),` -> `Self::AtWall => format!("{account}\|cedar_ember"),` | anthropic::profile::tests:: anthropic::resets::tests:: | killed: each_program_comes_from_its_own_read_through_the_profile_token |  |
| E19 | anthropic/profile.rs | `.filter(\|share\| share.allowance_percent_of_weekly > 0.0)` -> `(removed)` | anthropic::profile::tests:: | SURVIVED | left: a zero allowance has not been seen; the share would read 0 and room 0 |
| E20 | anthropic/profile.rs | `Some(body) if !fresh => Ok(body.clone()),` -> `Some(body) => Ok(body.clone()),` | anthropic::profile::tests:: | killed: a_young_answer_is_reused_and_a_second_accounts_429_holds_back_every_account |  |
| E21 | anthropic/profile.rs | `} else { / entry.retry_after = now + retry_delay_secs(false, 0, None);` -> `(removed)` | anthropic::profile::tests:: | killed: other_failures_hold_back_only_their_account_and_the_file_is_private |  |
| E22 | anthropic/profile.rs | `rate_limited: status.as_u16() == 429,` -> `rate_limited: !status.is_success(),` | anthropic::profile::tests:: | killed: other_failures_hold_back_only_their_account_and_the_file_is_private |  |
| E23 | anthropic/profile.rs | `models.dedup_by(\|a, b\| a.key() == b.key());` -> `(removed)` | anthropic::profile::tests:: | killed: scoped_rows_with_the_same_key_collapse_to_the_first |  |
| E24 | anthropic/profile.rs | `let young = now - entry.fetched_at < MAX_AGE_SECS;` -> `let young = now - entry.fetched_at <= MAX_AGE_SECS;` | anthropic::profile::tests:: | killed: a_young_answer_is_reused_and_a_second_accounts_429_holds_back_every_account |  |
| E25 | anthropic/profile.rs | `profile.weekly = first(LimitKind::WeeklyAll);` -> `profile.weekly = first(LimitKind::WeeklyScoped);` | anthropic::profile::tests:: | killed: a_body_without_a_session_row_has_no_5h_limit_and_fable_is_a_share_of_the_weekly,merging_fills_only_what_the_headers_left_out_and_keeps_a_seen_5h_window,todays_cache_file_loads_and_an_entry_it_cannot_read_is_dropped_alone |  |
| E26 | anthropic/profile.rs | `crate::drift::merge(&mut reading.unrecognized, profile.unrecognized);` -> `(removed)` | anthropic::profile::tests:: | killed: a_key_a_kind_or_a_scope_it_does_not_know_is_reported_and_lands_in_the_reading |  |
| F1 | anthropic/resets.rs | `if self.paused { / GrantState::Paused / } else if self.resets_left == 0 {` -> `if self.resets_left == 0 {` | anthropic::resets::tests:: | killed: a_grant_counts_only_while_unpaused_started_and_unexpired |  |
| F2 | anthropic/resets.rs | `.is_some_and(\|end\| now >= end) {` -> `.is_some_and(\|end\| now > end) {` | anthropic::resets::tests:: | killed: a_grant_counts_only_while_unpaused_started_and_unexpired |  |
| F3 | anthropic/resets.rs | `(false, Some(reason)) if UNKNOWN_REASONS.contains(&reason) => Standing::Unknown,` -> `(removed)` | anthropic::resets::tests:: | killed: a_program_the_server_did_not_evaluate_is_unknown_not_zero |  |
| F4 | anthropic/resets.rs | `.filter(\|grant\| grant.is_usable(now))` -> `(removed)` | anthropic::resets::tests:: | killed: a_live_status_block_parses_and_describes |  |
| F5 | anthropic/resets.rs | `let for_model = !self.gives_back_weekly / \|\| model.is_some_and(\|model\| model.family() == S` -> `let for_model = true \|\| model.is_some();` | anthropic::resets::tests:: ui::overview:: forecast:: attention:: | SURVIVED | strengthened: weekly_refills had no test anywhere (F5r killed) |
| F6 | anthropic/resets.rs | `` -> `&& false` | anthropic::resets::tests:: | SURVIVED | equivalent for the rule: Journal::begin refuses the same claim after the organization lookup; the pre-check only saves that request |
| F7 | anthropic/resets.rs | `if let Some(pending) = &pending / && response.result != ClaimResult::Unknown / {` -> `if let Some(pending) = &pending {` | anthropic::resets::tests:: | killed: a_claim_left_pending_by_an_earlier_build_is_honoured |  |
| F8 | anthropic/resets.rs | `journal.finish(Service::Claude, &account.name, &pending.request_id)?;` -> `(removed)` | anthropic::resets::tests:: | killed: a_claim_left_pending_by_an_earlier_build_is_honoured,an_uncertain_claim_is_resent_as_itself_and_blocks_any_other_grant |  |
| F9 | anthropic/resets.rs | `"request_id": pending.request_id,` -> `"request_id": crate::claims::request_id()?,` | anthropic::resets::tests:: | killed: a_claim_left_pending_by_an_earlier_build_is_honoured,an_uncertain_claim_is_resent_as_itself_and_blocks_any_other_grant |  |
| F10 | anthropic/resets.rs | `` -> `if let Some(pending) = &pending { / journal.finish(Service::Claude, &account.name, &pendin` | anthropic::resets::tests:: | killed: an_uncertain_claim_is_resent_as_itself_and_blocks_any_other_grant |  |
| F11 | anthropic/resets.rs | `.filter(\|grant\| grant.is_usable(now))` -> `(removed)` | anthropic::resets::tests:: | killed: each_program_comes_from_its_own_read_through_the_profile_token |  |
| F12 | anthropic/resets.rs | `} else if instant(self.starts_at.as_deref()).is_some_and(\|start\| now < start) {` -> `} else if instant(self.starts_at.as_deref()).is_some_and(\|start\| now <= start) {` | anthropic::resets::tests:: | SURVIVED | strengthened: start boundary (F12r killed) |
| F13 | anthropic/resets.rs | `self.eligible \|\| self.available,` -> `self.eligible && self.available,` | anthropic::resets::tests:: | SURVIVED | strengthened: session reset on offer (F13r killed) |
| F14 | anthropic/resets.rs | `"seven_day" if grant.gives_back_weekly => "Fable's use of 7d",` -> `(removed)` | anthropic::resets::tests:: | killed: a_grant_counts_only_while_unpaused_started_and_unexpired |  |
| F15 | anthropic/resets.rs | `let clears_weekly = self.clears.iter().any(\|window\| window == "seven_day");` -> `let clears_weekly = !self.clears.is_empty();` | anthropic::resets::tests:: ui::overview:: forecast:: attention:: | SURVIVED | same (F15r killed) |
| F16 | anthropic/resets.rs | `_ => status.juniper_tide = read.juniper_tide,` -> `_ => { / status.juniper_tide = read.juniper_tide; / status.cedar_ember = read.cedar_ember;` | anthropic::resets::tests:: | killed: each_program_comes_from_its_own_read_through_the_profile_token |  |
| F17 | anthropic/resets.rs | `let organization = organization_uuid(client, bearer).await?;` -> `(removed)` | anthropic::resets::tests:: | COMPILE | did not compile; not evidence |
| G1 | claims.rs | `` -> `&& false` | claims::tests:: anthropic::resets::tests:: | killed: both_old_layouts_are_seen_without_a_write_and_folded_in_by_the_first_change,one_claim_per_service_and_account_until_it_is_answered |  |
| G2 | claims.rs | `self.pending.service == service` -> `(self.pending.service == service \|\| true)` | claims::tests:: anthropic::resets::tests:: | killed: both_old_layouts_are_seen_without_a_write_and_folded_in_by_the_first_change,one_claim_per_service_and_account_until_it_is_answered |  |
| G3 | claims.rs | `!(entry.is_for(service, account) && entry.pending.request_id == request_id)` -> `!entry.is_for(service, account)` | claims::tests:: anthropic::resets::tests:: | killed: one_claim_per_service_and_account_until_it_is_answered |  |
| G4 | claims.rs | `Some(text) => serde_json::from_str(&text).with_context(\|\| {` -> `Some(text) => Ok(serde_json::from_str(&text).unwrap_or_default()).with_context(\|\| {` | claims::tests:: anthropic::resets::tests:: | COMPILE | did not compile; redone as G4r (killed) |
| G5 | claims.rs | `if !known { / file.pending.push(old.clone()); / }` -> `let _ = known; / file.pending.push(old.clone());` | claims::tests:: anthropic::resets::tests:: | SURVIVED | strengthened: answered claim frees the account (G5r killed) |
| G6 | claims.rs | `write_atomic(&self.path, &bytes, 0o600)` -> `write_atomic(&self.path, &bytes, 0o644)` | claims::tests:: anthropic::resets::tests:: | killed: one_claim_per_service_and_account_until_it_is_answered |  |
| G7 | claims.rs | `\|\| (self.sanitized && self.pending.account == sanitize(account)))` -> `\|\| (self.sanitized && self.pending.account == account))` | claims::tests:: anthropic::resets::tests:: | killed: an_old_file_that_does_not_parse_blocks_its_account_until_cleared,both_old_layouts_are_seen_without_a_write_and_folded_in_by_the_first_change |  |
| G8 | claims.rs | `let (file, legacy) = self.merged()?; / Self::refuse_unreadable(&legacy, service, account)?` -> `let (file, _legacy) = self.merged()?;` | claims::tests:: anthropic::resets::tests:: | killed: an_old_file_that_does_not_parse_blocks_its_account_until_cleared |  |
| G9 | claims.rs | `if entry.is_for(service, account) { / set_aside(path)?; / }` -> `let _ = (entry, path);` | claims::tests:: anthropic::resets::tests:: | killed: an_old_file_that_does_not_parse_blocks_its_account_until_cleared |  |
| G10 | claims.rs | `` -> `let _lock = self.locked()?;` | claims::tests:: anthropic::resets::tests:: | killed: both_old_layouts_are_seen_without_a_write_and_folded_in_by_the_first_change,one_claim_per_service_and_account_until_it_is_answered |  |
| G11 | claims.rs | `for source in &legacy.sources { / set_aside(source)?; / }` -> `let _ = legacy;` | claims::tests:: anthropic::resets::tests:: | killed: a_claim_left_pending_by_an_earlier_build_is_honoured,an_entry_of_the_new_file_wins_over_an_old_file_for_the_same_account,both_old_layouts_are_seen_without_a_write_and_folded_in_by_the_first_change |  |
| G12 | claims.rs | `sent_at: now,` -> `sent_at: 0 * now,` | claims::tests:: anthropic::resets::tests:: | killed: an_uncertain_claim_is_resent_as_itself_and_blocks_any_other_grant,the_file_layout_is_one_list_of_entries |  |
| G13 | claims.rs | `Err(_) => legacy / .unreadable / .push((entry(None, String::new(), 0), path)),` -> `Err(_) => {}` | claims::tests:: anthropic::resets::tests:: | killed: an_old_file_that_does_not_parse_blocks_its_account_until_cleared |  |
| G14 | claims.rs | `Self::refuse_unreadable(&legacy, service, account)?;` -> `(removed)` | claims::tests:: anthropic::resets::tests:: | killed: an_old_file_that_does_not_parse_blocks_its_account_until_cleared |  |
| G15 | claims.rs | `bytes[6] = (bytes[6] & 0x0f) \| 0x40;` -> `(removed)` | claims::tests:: anthropic::resets::tests:: | killed: request_ids_are_v4_uuids_and_fit_claudes_format |  |
| G16 | claims.rs | `sent_at: claim.started_at,` -> `sent_at: 0,` | claims::tests:: anthropic::resets::tests:: | killed: both_old_layouts_are_seen_without_a_write_and_folded_in_by_the_first_change |  |
| H1 | history.rs | `.rfind(\|event\| event.kind != EventKind::Revoke && event.at <= at)` -> `.rfind(\|event\| event.at <= at)` | history::tests:: claude::sessions::tests:: | SURVIVED | the test was vacuous (the revoked account was the offered one); rewritten (H1r killed) |
| H2 | history.rs | `.rfind(\|event\| event.kind != EventKind::Revoke && event.at <= at)` -> `.rfind(\|event\| event.kind != EventKind::Revoke && event.at < at)` | history::tests:: claude::sessions::tests:: | SURVIVED | strengthened: the instant of the switch (H2r killed) |
| H3 | history.rs | `.rposition(\|event\| event.at <= cutoff)` -> `.position(\|event\| event.at > cutoff)` | history::tests:: claude::sessions::tests:: | SURVIVED | compaction had no test; added (H3r killed) |
| H4 | history.rs | `digest[..8]` -> `digest[..4]` | history::tests:: claude::sessions::tests:: | killed: fingerprints_are_stable_and_short |  |
| H5 | history.rs | `#[serde(rename_all = "lowercase")]` -> `#[serde(rename_all = "UPPERCASE")]` | history::tests:: claude::sessions::tests:: | killed: events_keep_their_stored_spelling |  |
| H6 | history.rs | `#[serde(other)]` -> `(removed)` | history::tests:: claude::sessions::tests:: | killed: events_keep_their_stored_spelling |  |
| I1 | claude/settings.rs | `_ => { / env.remove(OAUTH_SCOPES); / }` -> `_ => {}` | claude::settings::tests:: | killed: falling_back_to_a_setup_token_withdraws_the_scope_claim |  |
| I2 | claude/settings.rs | `Credential::Login { .. } if installed == account.setup_key.as_deref() => {` -> `Credential::Login { .. } if false => {` | claude::settings::tests:: | killed: reinstall_reasons_distinguish_upgrade_refresh_and_repair |  |
| I3 | claude/settings.rs | `(!(scopes_ok && marker_ok && wait_ok)).then_some(EventKind::Repair)` -> `(!(scopes_ok && marker_ok)).then_some(EventKind::Repair)` | claude::settings::tests:: | killed: reinstall_reasons_distinguish_upgrade_refresh_and_repair |  |
| I4 | claude/settings.rs | `(!(scopes_ok && marker_ok && wait_ok)).then_some(EventKind::Repair)` -> `(!(scopes_ok && wait_ok)).then_some(EventKind::Repair)` | claude::settings::tests:: | SURVIVED | strengthened (I4r killed) |
| I5 | claude/settings.rs | `(!(scopes_ok && marker_ok && wait_ok)).then_some(EventKind::Repair)` -> `(!(marker_ok && wait_ok)).then_some(EventKind::Repair)` | claude::settings::tests:: | SURVIVED | strengthened (I5r killed) |
| I6 | claude/settings.rs | `const MANAGED_KEYS: [&str; 4] = [OAUTH_TOKEN, ACCOUNT, OAUTH_SCOPES, WAIT_401];` -> `const MANAGED_KEYS: [&str; 3] = [OAUTH_TOKEN, ACCOUNT, OAUTH_SCOPES];` | claude::settings::tests:: | killed: clearing_removes_every_managed_key |  |
| I7 | claude/settings.rs | `write_atomic(path, &bytes, 0o600)` -> `write_atomic(path, &bytes, 0o644)` | claude::settings::tests:: | SURVIVED | the test named owner-only and asserted no mode; strengthened (I7r killed) |
| I8 | claude/settings.rs | `if groups.is_empty() { / hooks.remove("SessionStart"); / }` -> `groups.clear();` | claude::settings::tests:: | killed: observer_hook_install_is_idempotent_and_preserves_other_hooks |  |
| I9 | claude/settings.rs | `remove_observer_hook(settings);` -> `(removed)` | claude::settings::tests:: | killed: observer_hook_install_is_idempotent_and_preserves_other_hooks |  |
| I10 | claude/settings.rs | `credential.expires_at().map(\|ms\| ms / 1000),` -> `credential.expires_at(),` | claude::settings::tests:: | killed: installing_a_login_declares_its_scopes_and_a_401_wait |  |
| I11 | claude/settings.rs | `if env.is_empty() { / settings.remove("env"); / }` -> `(removed)` | claude::settings::tests:: | killed: clearing_removes_every_managed_key |  |
| I12 | claude/settings.rs | `env.insert(WAIT_401.into(), Value::String(WAIT_401_MS.into()));` -> `(removed)` | claude::settings::tests:: | killed: installing_a_login_declares_its_scopes_and_a_401_wait |  |
| I13 | claude/settings.rs | `command.contains(" rotate observe-session ") \|\| command.ends_with(" rotate observe-session` -> `command.contains("observe-session") \|\| command.contains("hooks")` | claude::settings::tests:: | killed: observer_hook_detection_does_not_match_unrelated_hooks,observer_hook_install_is_idempotent_and_preserves_other_hooks |  |
| I14 | claude/settings.rs | `` -> `Value::Array(_) => Ok(Settings::new()),` | claude::settings::tests:: | killed: settings_round_trip_as_an_owner_only_file |  |
| I15 | claude/settings.rs | `now_ms / 1000,` -> `now_ms,` | claude::settings::tests:: | SURVIVED | strengthened: event time in seconds (I15r killed) |
| J1 | claude/keychain.rs | `if object.contains_key(LOGIN_KEY) { / return Ok(false); / }` -> `(removed)` | claude::keychain::tests:: | killed: keychain_restore_never_overwrites_a_newer_manual_login |  |
| J2 | claude/keychain.rs | `existing.and_then(\|backup\| backup.login),` -> `existing.and_then(\|_\| None),` | claude::keychain::tests:: | killed: an_empty_keychain_does_not_erase_the_captured_login_backup |  |
| J3 | claude/keychain.rs | `BackupState::Suppressed,` -> `BackupState::Captured,` | claude::keychain::tests:: | killed: an_empty_keychain_does_not_erase_the_captured_login_backup |  |
| J4 | claude/keychain.rs | `Ok(object_of(document)?.remove(LOGIN_KEY))` -> `Ok(std::mem::take(object_of(document)?).remove(LOGIN_KEY))` | claude::keychain::tests:: | killed: keychain_suppression_preserves_mcp_and_unknown_fields |  |
| J5 | claude/keychain.rs | `#[serde(rename = "claudeAiOauth")]` -> `#[serde(rename = "login")]` | claude::keychain::tests:: | killed: the_backup_keeps_its_stored_spelling |  |
| K1 | claude/sessions.rs | `fields[7].rsplit('/').next()? != "claude"` -> `!fields[7].contains("claude")` | claude::sessions::tests:: | killed: recognizes_only_real_claude_process_rows |  |
| K2 | claude/sessions.rs | `match history::account_at(timeline, now) {` -> `match history::account_at(timeline, now).filter(\|_\| binding.is_none()) {` | claude::sessions::tests:: | killed: a_live_session_on_a_drained_account_is_not_reported_as_stranded |  |
| K3 | claude/sessions.rs | `observe::local_time(naive)` -> `Some(naive.and_utc().with_timezone(&Local))` | claude::sessions::tests:: | SURVIVED | the start time was compared with the parser itself; strengthened (K3r killed) |
| K4 | claude/sessions.rs | `let prefix = if session.source == Attribution::SessionStart {` -> `let prefix = if session.source == Attribution::Timeline {` | claude::sessions::tests:: | killed: summaries_count_per_account_and_mark_bindings |  |
| K5 | claude/sessions.rs | `let tty = (fields[1] != "??").then(\|\| fields[1].to_owned());` -> `let tty = Some(fields[1].to_owned());` | claude::sessions::tests:: | killed: recognizes_only_real_claude_process_rows |  |
| K6 | claude/sessions.rs | `"%a %b %e %T %Y"` -> `"%a %b %e %T %G"` | claude::sessions::tests:: | killed: recognizes_only_real_claude_process_rows |  |
| L1 | fs.rs | `set_mode(parent, 0o700)?;` -> `set_mode(parent, 0o755)?;` | fs::tests:: | killed: atomic_writes_land_owner_only |  |
| L2 | fs.rs | `set_mode(path, mode)` -> `set_mode(path, 0o644)` | fs::tests:: | killed: atomic_writes_land_owner_only |  |
| L3 | fs.rs | `set_mode(temp.path(), mode)?;` -> `(removed)` | fs::tests:: | SURVIVED | equivalent: the mode is set again after the rename |
| L4 | fs.rs | `Ok(()) => false,` -> `Ok(()) => true,` | fs::tests:: daemon::lock:: | killed: the_daemon_lock_is_held_for_its_lifetime_and_admits_one_daemon |  |
| L5 | fs.rs | `set_mode(path, 0o600)?;` -> `(removed)` | fs::tests:: history:: | SURVIVED | strengthened: append_line mode (L5r killed) |
| L6 | fs.rs | `Err(error) if error.kind() == fs2::lock_contended_error().kind() => Ok(None), / Err(error)` -> `Err(_) => Ok(Some(Self { _file: file })),` | fs::tests:: | killed: a_held_lock_is_seen_as_contended_and_released_on_drop |  |
| L7 | fs.rs | `.filter_map(\|value\| serde_json::from_value(value).ok()) / .collect())` -> `.map(\|value\| serde_json::from_value(value).map_err(serde::de::Error::custom)) / .collect()` | fs::tests:: observe:: drift:: anthropic::profile:: | COMPILE | did not compile; not evidence |
| L8 | fs.rs | `Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),` -> `Err(_) => Ok(None),` | fs::tests:: claims:: | SURVIVED | strengthened: only NotFound reads as absent (L8r killed) |
| N1 | launch.rs | `for name in &self.unset { / command.env_remove(name); / }` -> `(removed)` | launch::tests:: | killed: a_launch_seats_the_account_in_the_childs_environment_only |  |
| N2 | launch.rs | `value.replace('\\', "\\\\").replace('"', "\\\"")` -> `value.replace('"', "\\\"")` | launch::tests:: | killed: iterm_script_sets_the_badge_once_and_escaped |  |
| N3 | launch.rs | `let command = apple_script_escape(&format!("{ITERM_BADGE}{command}"));` -> `let command = apple_script_escape(&format!("{ITERM_BADGE}{ITERM_BADGE}{command}"));` | launch::tests:: | killed: iterm_script_sets_the_badge_once_and_escaped |  |
| N4 | launch.rs | `command.env(var.name, &var.value);` -> `if !var.secret { command.env(var.name, &var.value); }` | launch::tests:: | killed: a_launch_seats_the_account_in_the_childs_environment_only |  |
| A5r | account.rs | `expires_at <= now_ms \|\| (expires_at - now_ms <= REFRESH_MARGIN_MS && old_enough)` -> `(expires_at - now_ms <= REFRESH_MARGIN_MS && old_enough)` | account::tests:: | killed: refresh_is_due_inside_the_margin_but_not_for_a_fresh_token |  |
| A18r | account.rs | `.is_some_and(\|expires_at\| expires_at <= now_ms)` -> `.is_some_and(\|expires_at\| expires_at <= i64::MIN)` | account::tests:: | killed: a_captured_access_token_without_refresh_is_never_installed |  |
| A22r | account.rs | `None => self.services.push(account),` -> `None => self.services.insert(0, account),` | account::tests:: | killed: services_keep_their_comments_and_replace_by_name |  |
| A20r | account.rs | `write_atomic(path, self.render(&existing)?.as_bytes(), 0o600)` -> `write_atomic(path, self.render(&existing)?.as_bytes(), 0o644)` | account::tests:: | killed: an_update_rereads_the_file_under_the_lock_and_leaves_it_owner_only |  |
| A23 | account.rs | `let lock = Self::lock_at(lock)?;` -> `let lock = Self::lock_at(&lock.with_extension("other"))?;` | account::tests:: | killed: an_update_rereads_the_file_under_the_lock_and_leaves_it_owner_only |  |
| A24 | account.rs | `let mut fleet = Self::load_from(path)?;` -> `let mut fleet = Self::default(); / let _ = Self::load_from(path)?;` | account::tests:: | killed: an_update_rereads_the_file_under_the_lock_and_leaves_it_owner_only |  |
| A25 | account.rs | `let result = mutate(&mut fleet)?;` -> `let result = mutate(&mut fleet); / let result = result?;` | account::tests:: | killed: an_update_rereads_the_file_under_the_lock_and_leaves_it_owner_only |  |
| B2r | anthropic/mod.rs | `if !parts.iter().any(\|part\| part.contains(&text)) {` -> `if true {` | anthropic::tests:: | killed: describe_joins_the_cause_chain_once |  |
| B4 | anthropic/mod.rs | `.filter(\|version\| version.chars().all(\|c\| c.is_ascii_digit() \|\| c == '.'))` -> `(removed)` | anthropic::tests:: | killed: the_user_agent_carries_the_installed_version_or_the_fallback |  |
| C9r | anthropic/oauth.rs | `Err(error) if error.is_rate_limited() => match waits.next() {` -> `Err(error) if matches!(error, Error::Refused { .. }) => match waits.next() {` | anthropic::oauth::tests:: | killed: a_refused_code_or_refresh_token_is_sent_once |  |
| C12 | anthropic/oauth.rs | `post_token(client, &body).await` -> `match post_token(client, &body).await { / Err(_) => post_token(client, &body).await, / ok ` | anthropic::oauth::tests:: | killed: a_refresh_states_its_scopes_and_returns_a_grant,a_refused_code_or_refresh_token_is_sent_once |  |
| D10r | anthropic/probe.rs | `reading / .limits / .push(Limit::extra_usage(window, Source::Headers));` -> `let _ = window;` | anthropic::probe::tests:: | killed: every_header_seen_or_read_by_claude_code_is_known |  |
| D16r | anthropic/probe.rs | `let Some(credential) = account.credential(now * 1000) else {` -> `let Some(credential) = account.credential(now) else {` | anthropic::probe::tests:: | killed: an_expired_login_token_is_not_sent |  |
| D20 | anthropic/probe.rs | `let Some(credential) = account.credential(now * 1000) else {` -> `let Some(credential) = account.credential(now) else {` | anthropic::probe::tests:: | killed: an_expired_login_token_is_not_sent |  |
| E27 | anthropic/profile.rs | `let bearer = account.profile_token(now * 1000)?;` -> `let bearer = account.profile_token(now)?;` | anthropic::probe::tests:: anthropic::profile::tests:: | killed: an_expired_login_token_is_not_sent |  |
| E28 | anthropic/profile.rs | `fraction(window.utilization?),` -> `window.utilization?,` | anthropic::profile::tests:: | killed: a_session_row_marks_the_5h_limit_present_and_a_model_id_keys_the_bucket,the_legacy_per_family_fields_are_not_read |  |
| E12r | anthropic/profile.rs | `if now >= self.retry_after { / self.accounts.clear(); / }` -> `let _ = now;` | anthropic::profile::tests:: | killed: a_429_after_the_backoff_ran_out_starts_a_new_one_for_its_account_alone |  |
| E13r | anthropic/profile.rs | `&& !fresh` -> `(removed)` | anthropic::profile::tests:: | killed: a_young_answer_is_reused_and_a_second_accounts_429_holds_back_every_account |  |
| F5r | anthropic/resets.rs | `let for_model = !self.gives_back_weekly / \|\| model.is_some_and(\|model\| model.family() == S` -> `let for_model = true \|\| model.is_some();` | anthropic::resets::tests:: ui::overview:: forecast:: attention:: | killed: a_grant_counts_only_while_unpaused_started_and_unexpired |  |
| F12r | anthropic/resets.rs | `} else if instant(self.starts_at.as_deref()).is_some_and(\|start\| now < start) {` -> `} else if instant(self.starts_at.as_deref()).is_some_and(\|start\| now <= start) {` | anthropic::resets::tests:: | killed: a_grant_counts_only_while_unpaused_started_and_unexpired |  |
| F13r | anthropic/resets.rs | `self.eligible \|\| self.available,` -> `self.eligible && self.available,` | anthropic::resets::tests:: | killed: a_program_the_server_did_not_evaluate_is_unknown_not_zero |  |
| F15r | anthropic/resets.rs | `let clears_weekly = self.clears.iter().any(\|window\| window == "seven_day");` -> `let clears_weekly = !self.clears.is_empty();` | anthropic::resets::tests:: ui::overview:: forecast:: attention:: | killed: a_grant_counts_only_while_unpaused_started_and_unexpired |  |
| G5r | claims.rs | `if !known { / file.pending.push(old.clone()); / }` -> `let _ = known; / file.pending.push(old.clone());` | claims::tests:: anthropic::resets::tests:: | killed: an_entry_of_the_new_file_wins_over_an_old_file_for_the_same_account |  |
| G4r | claims.rs | `Some(text) => serde_json::from_str(&text).with_context(\|\| {` -> `Some(text) => Ok(serde_json::from_str(&text).unwrap_or_default()).with_context(\|\| {` | claims::tests:: | COMPILE |  |
| H1r | history.rs | `.rfind(\|event\| event.kind != EventKind::Revoke && event.at <= at)` -> `.rfind(\|event\| event.at <= at)` | history::tests:: claude::sessions::tests:: | killed: a_revocation_is_not_an_offer |  |
| H2r | history.rs | `.rfind(\|event\| event.kind != EventKind::Revoke && event.at <= at)` -> `.rfind(\|event\| event.kind != EventKind::Revoke && event.at < at)` | history::tests:: claude::sessions::tests:: | killed: a_live_process_follows_a_switch_before_its_token_expires |  |
| H3r | history.rs | `.rposition(\|event\| event.at <= cutoff)` -> `.position(\|event\| event.at > cutoff)` | history::tests:: claude::sessions::tests:: | killed: compaction_keeps_the_month_and_the_offer_in_effect_at_its_start |  |
| I4r | claude/settings.rs | `(!(scopes_ok && marker_ok && wait_ok)).then_some(EventKind::Repair)` -> `(!(scopes_ok && wait_ok)).then_some(EventKind::Repair)` | claude::settings::tests:: | killed: reinstall_reasons_distinguish_upgrade_refresh_and_repair |  |
| I5r | claude/settings.rs | `(!(scopes_ok && marker_ok && wait_ok)).then_some(EventKind::Repair)` -> `(!(marker_ok && wait_ok)).then_some(EventKind::Repair)` | claude::settings::tests:: | killed: reinstall_reasons_distinguish_upgrade_refresh_and_repair |  |
| I7r | claude/settings.rs | `write_atomic(path, &bytes, 0o600)` -> `write_atomic(path, &bytes, 0o644)` | claude::settings::tests:: | killed: settings_round_trip_as_an_owner_only_file |  |
| I15r | claude/settings.rs | `now_ms / 1000,` -> `now_ms,` | claude::settings::tests:: | killed: installing_a_login_declares_its_scopes_and_a_401_wait |  |
| K3r | claude/sessions.rs | `observe::local_time(naive)` -> `Some(naive.and_utc().with_timezone(&Local))` | claude::sessions::tests:: | killed: recognizes_only_real_claude_process_rows |  |
| L5r | fs.rs | `set_mode(path, 0o600)?;` -> `(removed)` | fs::tests:: history:: | killed: atomic_writes_land_owner_only |  |
| L8r | fs.rs | `Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),` -> `Err(_) => Ok(None),` | fs::tests:: claims:: | killed: atomic_writes_land_owner_only |  |
| S1 | account.rs | `.field("access_token", &held(&self.access_token))` -> `.field("access_token", &self.access_token)` | account::tests:: | killed: no_credential_is_shown_when_an_account_is_formatted |  |
| S2 | account.rs | `.field("refresh_token", &held(&self.refresh_token))` -> `.field("refresh_token", &self.refresh_token)` | account::tests:: | killed: no_credential_is_shown_when_an_account_is_formatted |  |
| S3 | account.rs | `.field("setup_key", &held(&self.setup_key))` -> `.field("setup_key", &self.setup_key)` | account::tests:: | killed: no_credential_is_shown_when_an_account_is_formatted |  |
| S4 | account.rs | `Self::Setup(_) => f.write_str("Setup(<set>)"),` -> `Self::Setup(value) => write!(f, "Setup({value:?})"),` | account::tests:: | killed: no_credential_is_shown_when_an_account_is_formatted |  |
| S5 | account.rs | `.field("token", &"<set>")` -> `.field("token", &self.value())` | account::tests:: | killed: no_credential_is_shown_when_an_account_is_formatted |  |
| S6 | account.rs | `.field("access_token", &"<set>")` -> `.field("access_token", &self.access_token)` | account::tests:: | killed: no_credential_is_shown_when_an_account_is_formatted |  |
| S7 | account.rs | `.field("refresh_token", &held(&self.refresh_token))` -> `.field("refresh_token", &self.refresh_token)` | account::tests:: | killed: no_credential_is_shown_when_an_account_is_formatted |  |
| S8 | anthropic/oauth.rs | `.field("access_token", &"<set>")` -> `.field("access_token", &self.access_token)` | anthropic::oauth::tests:: | killed: a_token_response_becomes_a_grant_with_absolute_expiries |  |
| S9 | anthropic/oauth.rs | `&crate::providers::held(&self.refresh_token),` -> `&self.refresh_token,` | anthropic::oauth::tests:: | killed: a_token_response_becomes_a_grant_with_absolute_expiries |  |
| S10 | anthropic/oauth.rs | `.field("verifier", &"<set>")` -> `.field("verifier", &self.verifier)` | anthropic::oauth::tests:: | killed: authorize_url_carries_profile_scope_and_a_challenge |  |
| S11 | claude/keychain.rs | `.field("login", &crate::providers::held(&self.login))` -> `.field("login", &self.login)` | claude::keychain::tests:: | killed: the_backup_keeps_its_stored_spelling |  |
| S12 | launch.rs | `let value: &dyn std::fmt::Debug = if self.secret {` -> `let value: &dyn std::fmt::Debug = if false {` | launch::tests:: | killed: a_launch_seats_the_account_in_the_childs_environment_only |  |

### Deleted

| test | reason |
|---|---|
| profile: response_shapes_round_trip | built `UsageResponse` structs, serialized, parsed, compared: a serde derive round trip; its last three lines repeat idle_null_reset... |
| profile: profile_usage_windows_normalize_percent_and_stay_unknown_when_incomplete | redundant: called the private helper on hand-built structs; the same three cases now run through real legacy bodies in the_legacy_per_family_fields_are_not_read (E28 kills) |
| profile: cache_files_round_trip_and_an_older_shape_reads_as_empty (first half) | serde round trip of `CacheFile`; the 0.3-shape half is kept as a_cache_file_in_the_0_3_shape_reads_as_empty |
| oauth: round trip of `TokenResponse` inside token_responses_round_trip_into_grants | serde derive round trip; test renamed a_token_response_becomes_a_grant_with_absolute_expiries |
| oauth: `assert_eq!(EXCHANGE_RETRY_SECS, [5, 15, 30])` | asserts a constant equals its definition |
| resets: `to_string` -> `from_str` == original at the end of four tests | serde derive round trips of ResetStatus, ResetGrant (twice), ClaimResponse |

### Strengthened

| test | what was weak | mutation it now catches |
|---|---|---|
| account: refresh_is_due_inside_the_margin... | no expired-but-young token | A5r: drop `expires_at <= now` |
| account: a_captured_access_token... | profile token expiry untested | A18r: expiry check disabled |
| account: services_keep_their_comments... | order after a replace unpinned | A22r: new service inserted first |
| anthropic: describe_joins_the_cause_chain_once | fixture had no repeated cause, so "once" was never exercised | B2r: dedupe removed |
| anthropic: the_user_agent_... (replaced) | ran the real `claude`; asserted only a shape | B4: version filter removed; B3 format |
| oauth: a_refresh_states_its_scopes... / new sent-once test | nothing said a refresh token is sent once | C12: refresh retried on error |
| probe: every_header_seen_or_read... | the usage-credit window never checked to become a limit | D10r |
| profile: a_young_answer_is_reused... | `fresh` on a young answer untested | E13r |
| profile: the_legacy_per_family_fields_are_not_read | one window, tolerance compare | E28: percent not divided |
| resets: a_live_status_block..., a_grant_counts_only... | `weekly_refills` untested; start boundary | F5r, F15r, F12r |
| resets: a_program_the_server_did_not_evaluate... | `available` without `eligible` | F13r |
| claims: an_entry_of_the_new_file_wins... | the old entry could be folded in beside the new one | G5r |
| history: a_revocation_is_not_an_offer | vacuous | H1r: revoke filter removed |
| history: a_live_process_follows_a_switch... | no boundary | H2r: `<=` -> `<` |
| settings: settings_round_trip_as_an_owner_only_file | asserted no mode | I7r: 0644 |
| settings: reinstall_reasons_distinguish... | only the 401 wait repaired | I4r, I5r |
| settings: installing_a_login... | event time unit | I15r |
| sessions: recognizes_only_real_claude_process_rows | compared `parse_ps_session` with `parse_lstart` (itself) | K3r: local time read as UTC |
| fs: atomic_writes_land_owner_only | `append_line` mode, non-missing read error | L5r, L8r |

### Added

| test | rule it pins | mutation seen to fail it |
|---|---|---|
| account: an_update_rereads_the_file_under_the_lock_and_leaves_it_owner_only | every write to tokens.toml is a locked read-modify-write: lock held from read to write, starts from the file, 0600, comments kept, a failed change writes nothing | A23 (other lock file), A24 (starts from an empty fleet), A25 (saves a failed change), A20r (0644) |
| oauth: a_refused_code_or_refresh_token_is_sent_once | a single-use code / refresh token is never sent twice; only a 429 on the exchange retries | C9r (retry any refusal), C12 (refresh retried) |
| probe: an_expired_login_token_is_not_sent | the credential is chosen by expiry in the account file's unit (ms); an expired login is neither probed with nor used for the profile | D16r, D20, E27 (seconds passed as ms) |
| profile: a_429_after_the_backoff_ran_out_starts_a_new_one_for_its_account_alone | two 429s in different backoffs do not hold back the fleet | E12r |
| history: compaction_keeps_the_month_and_the_offer_in_effect_at_its_start | attribution survives compaction; the file stays 0600 | H3r (drops the offer in effect at the window start), L5r |
| account: no_credential_is_shown_when_an_account_is_formatted (+ assertions in oauth, keychain, launch tests) | no credential is printed or logged in any form: `{:?}` of a fleet, account, login, credential, grant, token response, PKCE verifier, Keychain backup, secret launch variable | S1-S12: each field shown again |

### Bugs

None in what the code does. One latent leak fixed (568d1e6: credential-holding types derived `Debug`). Two defects of the test suite itself, both fixed: the live-file write (9432639) and the
real `claude` spawn (468550f). Seams added to product code for tests, behaviour unchanged: `probe_all_with`
(probe.rs), `Fleet::update_at` / `load_from` / `save_to` / `lock_at` (account.rs, private), `history::load_from` /
`append_to` (private), `anthropic::version_word`, and `if cfg!(test) { return None }` in `installed_cli_version`.
The unused one-account `anthropic::probe::probe` was deleted (its only caller was a test).

### Not tested and matters

| what | why it matters | why not done |
|---|---|---|
| `daemon::refresh::refresh_where` (g4 file): the re-read under the fleet lock right before redeeming, save after the grant, permanent refusal recorded | this is where a refresh token is actually redeemed; a regression kills a login | it calls `Client::shared()`, `RotationLock`, `Fleet::load_locked`, `history::append` on default paths: needs a seam in g4's file. `Fleet::update_at` and oauth's sent-once test cover the two halves it is made of, not their composition |
| `claude::keychain::suppress` / `restore` orchestration (backup written `captured` before the Keychain write, kept if the write fails, removed only after a restore) | losing the user's /login record | talks to the real Keychain through security-framework; needs an injected store. The pure steps (`split_login`, `restore_login`, `settled`) are tested and mutation-checked (J1-J5); I did not refactor credential-moving code for a seam this late |
| `keychain::load_backup` version check, `save_backup` mode | backup holds a refresh token | default path only; `write_atomic(.., 0o600)` itself is tested |
| `launch::run`: `env_remove("CLAUDE_CODE_OAUTH_TOKEN")` on the claude child | an inherited token pins the session to the launch-time account | spawns claude after a real cycle; `Launch::command` (the other launches) is tested |
| an uncertain `juniper_tide` (session reset) claim | it carries no request id, so a resend after a lost answer could spend twice | by design nothing to journal; `claim(None)` is only reachable from `tokeman claude reset` |
| `Fleet::load_locked` + `save` as used directly (not through `update`) by the refresh sweep | same lock, same file | only the path-taking forms are tested; the public wrappers are one line each |
| `resets::status` when one of the two reads fails (errors kept, the other program still read) | a reset offer shown or hidden wrongly | not written; low risk next to the above |
| nothing forbids a new credential-holding type from deriving `Debug`/`Serialize` | the next one leaks the same way | a convention, not a test; the existing types are pinned (S1-S12). `claims::Pending` shows its request id (an idempotency key, not a credential) |
| profile cache `update` is an unlocked read-modify-write | two processes can lose an entry | documented as best-effort in the code; costs one extra read |

### Deferred (audit-only files)

None in g2.

### Cross-group

- g4: `daemon/refresh.rs::refresh_where` needs a seam (client, paths) and a test: grant due -> one token request,
  new refresh token on disk, old access token recorded as revoked; `invalid_grant` -> `login.error` set and saved;
  a concurrent writer's change to another account survives. See "Not tested".
- root: `probe_all_with(client, cache, ..)` exists now; g4's `daemon/cycle.rs` and `commands/view.rs` call
  `probe_all` (default cache) and need no change. Any g4 test that reaches `probe_all` writes through the default
  cache path (now under TEST_HOME, so the write silently fails).
- root: the whole-suite detector I used is worth keeping in the report: run the test binary with
  `HOME`, `XDG_*`, `CLAUDE_SETTINGS`, `TMPDIR` pointed into an empty directory and list what appears outside
  `TMPDIR`.

### Hygiene notes

- probe.rs: two tests resolved the live config dir through `profile::Cache::default()`; one wrote it (above). Fixed
  by injecting the cache; verified with the paths.rs panic patch (0 REALPATH panics in g2's modules, patch reverted).
- anthropic/mod.rs: `user_agent()` spawned `claude --version` once per test process. Fixed.
- claude/keychain.rs:183 `dirs::home_dir()` -> `paths::home()` (c640d38). No g2 test reaches `suppress`/`restore`
  (real Keychain) or `sessions::live` / `ps` (real binary).
- oauth.rs: `a_refresh_states_its_scopes_and_returns_a_grant` reads `Utc::now()` only to bound a value from below
  (`grant_of(.., now)` inside `post_token`); not order- or zone-dependent. `begin()` reads /dev/urandom.
- probe.rs `a_timeout_is_a_network_error`: a real 200 ms client timeout against a handler that sleeps 5 s; the
  test takes ~0.2 s. The only timing-dependent test in the group; it cannot flake short of a 200 ms scheduler stall
  making the request *succeed*, which the 5 s sleep rules out.
- sessions.rs: `summaries_count...` uses `Utc::now()` as a filler field; `recognizes_only...` now converts through
  the local zone and back, which is zone-independent for the date used.
- launch.rs `a_launch_seats_the_account...` runs `/bin/sh` (a fixed system binary, no user state).
- No `set_var`/`remove_var`, no sleeps, no real hostnames (loopback only; `dead_client` is 127.0.0.1:9) in g2 tests.
- Mutation run incident: batch 2 was stopped (by its task id) before mutation F5, whose filter would have run
  `commands::` tests while the real-`~/.codex` hazard was open in this clone; filters were narrowed and the batch
  resumed after merging d5b0bbb. No `daemon::codex` / `commands::codex` test was run by g2.

---

## g3: src/codex/*.rs (incl. the merged follow-up: mover, appserver, seats, keep) and src/providers/**

Branch `lane/tests-g3` in `<scratchpad>/g3`, on top of lane/tests 5030bef. Commits (oldest first):
`2252160` codex tests, `2059f3a` provider tests, `536643a` one loopback RPC client for agy, `fdb38d7` ask paths
for the home directory, `2e66ea2` secret-bearing types redact Debug, `96d5c76` mover/sweep spend no reset credit,
`1545c40` clippy fix in a test.

Blunt summary: these 116 tests were the good part of the suite. None was hollow in the sense of restating a
fixture; two assert constants that are external contracts and were kept on purpose. But 32 of 135 compiling
mutations survived the first round (24%), concentrated in error paths, "either/or" conditions where the
fixtures always set both sides, and file modes. 30 of the 32 are now killed. One real hazard was found through
this group's code (a g4 test could push a test token into ember's running Codex), and one latent credential
leak (derived `Debug` on types that hold tokens).

### Counts

| file | before | a | b | c | d | deleted | strengthened | added | after |
|---|---|---|---|---|---|---|---|---|---|
| codex/appserver.rs | 4 | 2 | 1 | 1 | 0 | 0 | 1 | 0 | 4 |
| codex/auth.rs | 6 | 5 | 1 | 0 | 0 | 0 | 1 | 1 | 7 |
| codex/keep.rs | 7 | 6 | 1 | 0 | 0 | 0 | 1 | 0 | 7 |
| codex/maintenance.rs | 3 | 2 | 1 | 0 | 0 | 0 | 1 | 0 | 3 |
| codex/mover.rs | 8 | 6 | 2 | 0 | 0 | 0 | 1 | 3 | 11 |
| codex/probe.rs | 11 | 11 | 0 | 0 | 0 | 0 | 0 | 2 | 13 |
| codex/refresh.rs | 3 | 3 | 0 | 0 | 0 | 0 | 0 | 1 | 4 |
| codex/resets.rs | 9 | 7 | 1 | 1 | 0 | 0 | 1 | 0 | 9 |
| codex/seats.rs | 1 | 0 | 1 | 0 | 0 | 0 | 1 | 0 | 1 |
| providers/mod.rs | 2 | 1 | 1 | 0 | 0 | 0 | 0 | 1 | 3 |
| providers/zai.rs | 8 | 7 | 1 | 0 | 0 | 0 | 0 | 3 | 11 |
| providers/kimi.rs | 7 | 6 | 1 | 0 | 0 | 0 | 1 | 0 | 7 |
| providers/antigravity/clients.rs | 6 | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 6 |
| providers/antigravity/grant.rs | 4 | 2 | 2 | 0 | 0 | 0 | 2 | 0 | 4 |
| providers/antigravity/mod.rs | 12 | 9 | 2 | 0 | 0 | 0 | 2 | 2 | 14 |
| providers/antigravity/run.rs | 8 | 7 | 1 | 0 | 0 | 0 | 1 | 0 | 8 |
| providers/antigravity/seat.rs | 17 | 15 | 2 | 0 | 0 | 0 | 3 | 0 | 17 |
| **total** | 116 | 95 | 18 | 2 | 0 | 0 | 16 | 13 | 129 |

### Inventory

Every test name in this group is a sentence stating its claim, so the name is the claim column.

| file | test (= claim) | class | action |
|---|---|---|---|
| codex/appserver.rs | the_client_reads_limits_and_the_account_and_a_login_moves_the_server | b | strengthened (fake gates on experimentalApi) |
| codex/appserver.rs | the_account_wide_windows_are_read_and_named_by_length | a | kept |
| codex/appserver.rs | account_info_takes_the_outermost_email_and_plan | a | kept |
| codex/appserver.rs | the_control_socket_is_not_the_ipc_socket | c | kept: asserts a constant, but it is Codex's socket path and the only test that fails when it changes (#60e) |
| codex/auth.rs | id_claims_come_from_the_namespaced_payload | a | kept |
| codex/auth.rs | writing_tokens_keeps_unmodelled_keys_and_lands_owner_only | a | kept |
| codex/auth.rs | a_non_object_tokens_slot_is_replaced | a | kept |
| codex/auth.rs | an_empty_refresh_token_is_absent_and_blocks_seating | a | kept |
| codex/auth.rs | the_account_id_falls_back_to_the_id_token_claim | b | strengthened |
| codex/auth.rs | slugs_keep_the_domain_head_only | a | kept |
| codex/auth.rs | debug_output_of_a_bundle_or_an_auth_file_shows_no_token | new | added |
| codex/keep.rs | a_token_near_expiry_is_renewed_through_the_profile_and_pushed | a | kept |
| codex/keep.rs | a_token_with_time_left_is_left_alone_and_a_newer_one_on_disk_is_pushed | a | kept |
| codex/keep.rs | a_refused_refresh_is_recorded_as_trouble_and_nothing_is_pushed | a | kept |
| codex/keep.rs | no_socket_or_another_account_ends_the_seat_and_sends_nothing | b | strengthened |
| codex/keep.rs | a_token_the_backend_rejects_early_is_renewed_at_the_next_look | a | kept |
| codex/keep.rs | a_seat_whose_profile_left_the_fleet_is_trouble | a | kept |
| codex/keep.rs | a_token_is_due_a_day_before_it_expires | a | kept |
| codex/maintenance.rs | maintenance_refreshes_rejected_accounts_and_reports_login_needs_once | b | strengthened |
| codex/maintenance.rs | maintenance_reports_transient_failures_every_sweep_and_a_reused_token_once | a | kept |
| codex/maintenance.rs | a_challenged_probe_does_not_force_a_refresh | a | kept |
| codex/mover.rs | a_healthy_seat_is_left_alone_and_nothing_is_probed | a | kept |
| codex/mover.rs | a_seat_below_its_floor_moves_to_the_best_profile_with_landing_room | a | kept |
| codex/mover.rs | a_low_seat_drains_when_nowhere_has_landing_room_and_a_spent_one_takes_scraps | b | kept; gap closed by a new test |
| codex/mover.rs | a_landing_that_cannot_be_read_is_no_landing_and_a_dead_socket_is_reported | a | kept |
| codex/mover.rs | the_same_account_in_another_home_is_not_a_landing | a | kept |
| codex/mover.rs | moves_are_appended_read_back_and_compacted | a | kept |
| codex/mover.rs | one_leaving_floor_raises_the_landing_floor_to_it | a | kept |
| codex/mover.rs | the_default_home_comes_first_and_each_home_once | b | strengthened (no real home; no-default case) |
| codex/mover.rs | a_refused_seat_moves_with_room_in_its_windows_and_one_without_windows_is_left | new | added |
| codex/mover.rs | a_seat_is_matched_by_account_id_before_email | new | added |
| codex/mover.rs | a_codex_with_nowhere_to_go_drains_and_no_reset_credit_is_spent | new | added |
| codex/probe.rs | a_full_body_decodes_to_a_reading | a | kept |
| codex/probe.rs | todays_weekly_only_body_names_its_one_window_weekly | a | kept |
| codex/probe.rs | newer_fields_decode_with_their_alternative_spellings | a | kept |
| codex/probe.rs | a_cosmetic_shape_change_falls_back_to_the_rate_limit_subtree | a | kept |
| codex/probe.rs | viability_follows_the_limit_block | a | kept |
| codex/probe.rs | a_probe_sends_codex_identity_and_reads_a_reading | a | kept |
| codex/probe.rs | a_challenge_is_retried_with_the_cookie_and_a_401_is_unauthorized | a | kept |
| codex/probe.rs | a_standing_challenge_is_not_an_auth_failure | a | kept |
| codex/probe.rs | local_failures_make_no_request | a | kept |
| codex/probe.rs | the_bodies_seen_so_far_carry_nothing_unrecognised | a | kept |
| codex/probe.rs | a_key_it_does_not_know_is_reported_wherever_it_sits | a | kept |
| codex/probe.rs | only_a_403_that_looks_like_the_edge_is_a_challenge | new | added |
| codex/probe.rs | a_window_past_its_limit_reads_over_one_and_an_answer_without_windows_offers_no_room | new | added |
| codex/refresh.rs | failures_classify_by_code_then_status | a | kept |
| codex/refresh.rs | a_profile_without_a_refresh_token_fails_locally | a | kept |
| codex/refresh.rs | a_fresh_profile_is_left_alone_unless_forced | a | kept |
| codex/refresh.rs | a_refresh_lands_the_rotated_bundle_and_keeps_what_the_answer_left_out | new | added |
| codex/resets.rs | the_list_decodes_with_timestamps_and_unknown_statuses_kept | a | kept |
| codex/resets.rs | consume_codes_use_the_backend_spellings | c | kept: serde names of a wire enum against the backend's spellings; nothing else pins them |
| codex/resets.rs | an_unlimited_account_or_one_without_applicable_credits_is_refused | b | strengthened |
| codex/resets.rs | a_claim_sends_one_id_and_clears_it_on_an_answer | a | kept |
| codex/resets.rs | an_unanswered_claim_is_retried_with_the_same_id | a | kept |
| codex/resets.rs | the_first_expiry_is_put_beside_the_count_only_where_credits_are_held | a | kept |
| codex/resets.rs | an_unreadable_answer_keeps_the_id | a | kept |
| codex/resets.rs | a_claim_left_pending_by_an_earlier_build_is_honoured | a | kept |
| codex/resets.rs | the_list_reports_keys_statuses_and_reset_types_it_does_not_know | a | kept |
| codex/seats.rs | seats_are_kept_per_home_and_trouble_sticks_to_the_token_it_was_about | b | strengthened |
| providers/mod.rs | the_key_comes_inline_or_trimmed_from_its_file | a | kept |
| providers/mod.rs | read_all_keeps_order_and_answers_for_every_account | b | unchanged (see Not tested) |
| providers/mod.rs | debug_output_names_which_secrets_are_held_and_shows_none | new | added |
| providers/zai.rs | credit_windows_read_used_over_limit_with_their_resets | a | kept |
| providers/zai.rs | an_idle_window_has_no_reset_pending | a | kept |
| providers/zai.rs | a_reset_beyond_the_window_length_is_dropped | a | kept |
| providers/zai.rs | legacy_token_and_mcp_limits_are_kept_apart | a | kept |
| providers/zai.rs | a_failure_reported_with_http_200_is_an_error | b | kept; gap closed by a new test |
| providers/zai.rs | read_sends_the_raw_key_and_reports_the_body_error | a | kept |
| providers/zai.rs | a_missing_key_file_fails_without_a_request | a | kept |
| providers/zai.rs | keys_units_and_levels_it_does_not_know_are_reported | a | kept |
| providers/zai.rs | success_needs_both_the_flag_and_code_200 | new | added |
| providers/zai.rs | used_comes_from_remaining_when_the_count_is_missing_and_may_pass_the_limit | new | added |
| providers/zai.rs | an_http_failure_is_reported_with_its_status | new | added |
| providers/kimi.rs | the_usages_map_becomes_meters_and_the_legacy_fields_are_ignored | a | kept |
| providers/kimi.rs | new_plan_monthly_pools_and_the_camel_case_wallet_are_read | a | kept |
| providers/kimi.rs | a_wallet_that_is_not_a_booster_or_is_empty_is_no_balance | a | kept |
| providers/kimi.rs | the_plan_is_the_level_name_only | a | kept |
| providers/kimi.rs | read_sends_bearer_and_survives_a_failed_me | b | strengthened |
| providers/kimi.rs | read_takes_the_plan_from_me | a | kept |
| providers/kimi.rs | a_window_a_wallet_field_or_a_profile_key_it_does_not_know_is_reported_by_name | a | kept |
| providers/antigravity/clients.rs | an_agy_command_line_is_an_acp_server_a_print_run_or_interactive | a | kept |
| providers/antigravity/clients.rs | the_census_finds_live_hubs_and_every_agy_with_its_directory_and_ports | a | kept |
| providers/antigravity/clients.rs | an_agy_on_a_per_account_home_is_a_session_not_a_client_of_the_slot | a | kept |
| providers/antigravity/clients.rs | the_resume_hint_is_the_last_conversation_recorded_for_the_directory | a | kept |
| providers/antigravity/clients.rs | a_census_that_cannot_be_taken_is_an_error_and_a_hung_one_is_bounded | a | kept |
| providers/antigravity/clients.rs | a_hub_needs_its_token_and_an_acp_server_answers_on_any_of_its_ports | a | kept |
| providers/antigravity/grant.rs | the_app_oauth_token_decodes_from_its_nested_protobuf | a | kept |
| providers/antigravity/grant.rs | the_app_user_status_names_the_account | b | strengthened |
| providers/antigravity/grant.rs | truncated_protobuf_is_rejected_not_misread | a | kept |
| providers/antigravity/grant.rs | the_agy_login_parses_with_and_without_the_go_keyring_prefix | b | strengthened |
| providers/antigravity/mod.rs | the_live_summary_becomes_one_meter_per_bucket | a | kept |
| providers/antigravity/mod.rs | an_untouched_window_has_no_reset_but_an_anchored_full_one_keeps_its_own | a | kept |
| providers/antigravity/mod.rs | a_summary_without_the_gemini_group_still_reads | a | kept |
| providers/antigravity/mod.rs | missing_fractions_disabled_buckets_and_wrappers | a | kept |
| providers/antigravity/mod.rs | the_plan_is_the_paid_tier_not_the_current_one | a | kept |
| providers/antigravity/mod.rs | the_user_agent_is_a_hub_client_new_enough_for_the_gemini_group | b | strengthened (was machine-dependent) |
| providers/antigravity/mod.rs | a_poll_refreshes_caches_and_reads_the_summary | b | strengthened |
| providers/antigravity/mod.rs | an_unexpired_token_is_used_and_a_rejected_one_is_refreshed_once | a | kept |
| providers/antigravity/mod.rs | a_revoked_grant_is_an_error_usage_that_names_the_fix | a | kept |
| providers/antigravity/mod.rs | an_account_without_a_refresh_token_fails_before_any_request | a | kept |
| providers/antigravity/mod.rs | a_token_rejected_even_after_a_refresh_is_an_error_after_one_refresh | new | added |
| providers/antigravity/mod.rs | quota_is_read_without_the_plan_when_load_code_assist_is_down | new | added |
| providers/antigravity/mod.rs | live | - | ignored by design (real accounts) |
| providers/antigravity/mod.rs | the_live_answers_carry_nothing_unrecognised_and_a_new_key_or_window_is_reported | a | kept |
| providers/antigravity/run.rs | an_account_gets_a_private_home_with_its_own_token_and_the_users_settings | a | kept |
| providers/antigravity/run.rs | a_second_run_keeps_the_file_agy_refreshed_and_follows_the_real_home | b | strengthened |
| providers/antigravity/run.rs | settings_are_seeded_once_and_an_earlier_link_to_the_real_file_becomes_a_copy | a | kept |
| providers/antigravity/run.rs | a_file_on_another_grant_is_replaced_and_a_dead_grant_is_refused_first | a | kept |
| providers/antigravity/run.rs | the_account_agy_already_holds_runs_plain_agy | a | kept |
| providers/antigravity/run.rs | an_ssh_tty_already_set_is_kept_and_other_services_are_refused | a | kept |
| providers/antigravity/run.rs | the_started_agy_gets_exactly_the_home_and_tty_and_the_arguments | a | kept |
| providers/antigravity/run.rs | sessions_name_the_account_whose_home_they_run_on | a | kept |
| providers/antigravity/seat.rs | seating_writes_agys_own_shape_and_backs_up_what_it_replaced | a | kept |
| providers/antigravity/seat.rs | seating_the_account_agy_already_holds_writes_nothing | a | kept |
| providers/antigravity/seat.rs | the_previous_login_comes_back_byte_for_byte_and_alternates | a | kept |
| providers/antigravity/seat.rs | restoring_without_a_backup_says_so | a | kept |
| providers/antigravity/seat.rs | a_dead_grant_or_another_service_is_refused_before_any_keychain_call | b | strengthened |
| providers/antigravity/seat.rs | a_write_the_keychain_does_not_keep_puts_the_old_login_back | b | strengthened |
| providers/antigravity/seat.rs | an_existing_fallback_file_follows_the_slot_and_a_missing_one_stays_missing | a | kept |
| providers/antigravity/seat.rs | the_slot_matches_an_account_by_grant_or_by_email | a | extended (Debug shows no token) |
| providers/antigravity/seat.rs | a_value_that_could_break_out_of_the_security_command_is_refused | a | kept |
| providers/antigravity/seat.rs | a_security_that_hangs_is_killed_at_the_bound | a | kept |
| providers/antigravity/seat.rs | the_write_goes_to_security_on_stdin_and_never_opens_the_item_to_every_app | a | kept |
| providers/antigravity/seat.rs | an_interactive_agy_refuses_the_switch_and_says_how_to_resume_each | a | kept |
| providers/antigravity/seat.rs | the_hub_is_told_and_confirmed_and_an_acp_server_on_the_old_account_is_reported | a | kept |
| providers/antigravity/seat.rs | a_hub_that_cannot_be_told_is_a_warning_and_the_seating_stands | a | kept |
| providers/antigravity/seat.rs | a_slot_that_reads_empty_right_after_the_write_is_written_once_more | a | kept |
| providers/antigravity/seat.rs | an_untakeable_census_stops_the_switch_unless_forced | a | kept |
| providers/antigravity/seat.rs | the_previous_login_goes_through_the_same_checks | a | kept |

### Mutations

Round 1: 137 mutations over every file, each run against the tests of its module (mutations 28, 33, 33b, 34
against all of `codex::`). 103 killed, 32 survived, 2 did not compile. The (a)-class sample is every KILLED row:
103 mutations, whose killers are 78 distinct tests of the 116.

| # | file | mutation (old → new) | tests run | outcome | follow-up |
|---|---|---|---|---|---|
| 1 | codex/probe.rs | `self.limit_reached != Some(true) && self.allowed.unwrap_or(true)` → `self.allowed.unwrap_or(true)` | codex::probe::viability_follows | KILLED by viability_follows_the_limit_block |  |
| 2 | codex/probe.rs | `let used = self.used_percent? / 100.0;` → `let used = self.used_percent? / 1000.0;` | codex::probe::todays_weekly_only | KILLED by todays_weekly_only_body_names_its_one_window_weekly |  |
| 3 | codex/probe.rs | `.map(\|after\| at.timestamp() + after)` → `.map(\|after\| after)` | codex::probe::a_full_body | KILLED by a_full_body_decodes_to_a_reading |  |
| 4 | codex/probe.rs | `Span::from_seconds(self.limit_window_seconds)` → `Span::from_seconds(None)` | codex::probe::todays_weekly_only | KILLED by todays_weekly_only_body_names_its_one_window_weekly |  |
| 5 | codex/probe.rs | `let status = (!block.allows()).then_some` → `let status = (block.allows()).then_some` | codex::probe::a_full_body | KILLED by a_full_body_decodes_to_a_reading |  |
| 7 | codex/probe.rs | `if is_edge_challenge(&response) && attempt < CHALLENGE_RETRIES` → `if false && attempt < CHALLENGE_RETRIES` | codex::probe::a_challenge_is_retried | KILLED by a_challenge_is_retried_with_the_cookie_and_a_401_is_unauthorized |  |
| 8 | codex/probe.rs | `return Err(if challenged {` → `return Err(if false {` | codex::probe::a_standing_challenge | KILLED by a_standing_challenge_is_not_an_auth_failure |  |
| 9 | codex/probe.rs | `request = request.header("chatgpt-account-id", account_id);` → `request = request.header("x-none", account_id);` | codex::probe::a_probe_sends | KILLED by a_probe_sends_codex_identity_and_reads_a_reading |  |
| 10 | codex/probe.rs | `&& codex.email.is_none()` → `&& false` | codex::probe::a_challenge_is_retried | KILLED by a_challenge_is_retried_with_the_cookie_and_a_401_is_unauthorized |  |
| 12 | codex/probe.rs | `"promo",` → `(removed)` | codex::probe::the_bodies_seen | KILLED by the_bodies_seen_so_far_carry_nothing_unrecognised |  |
| 13 | codex/probe.rs | `if reading.accepts_work() {` → `if !reading.accepts_work() {` | codex::probe::viability_follows | KILLED by viability_follows_the_limit_block |  |
| 14 | codex/mod.rs | `response.headers().contains_key("cf-mitigated")` → `false` | codex::probe | SURVIVED | fixed: killed in round 2 by only_a_403_that_looks_like_the_edge_is_a_challenge |
| 14b | codex/mod.rs | `.is_some_and(\|value\| value.starts_with("text/html"))` → `.is_some_and(\|_\| false)` | codex::probe | SURVIVED | fixed: killed in round 2 by only_a_403_that_looks_like_the_edge_is_a_challenge |
| 14c | codex/mod.rs | `response.status() == reqwest::StatusCode::FORBIDDEN &&` → `true &&` | codex::probe | SURVIVED | fixed: killed in round 2 by only_a_403_that_looks_like_the_edge_is_a_challenge |
| 6 | codex/probe.rs | `None \| Some(Value::Null) => None, Some(raw) => Some(` → `None \| Some(Value::Null) => None, Some(raw) if false => Some(` | codex::probe::a_cosmetic | NOCOMPILE | did not compile; replaced by p2 |
| 15 | codex/resets.rs | `if plan.probe.accepts_work() {` → `if false {` | codex::resets::an_unlimited | KILLED by an_unlimited_account_or_one_without_applicable_credits_is_refused |  |
| 16 | codex/resets.rs | `if applicable.is_none_or(\|applicable\| applicable <= 0) {` → `if false {` | codex::resets::an_unlimited | KILLED by an_unlimited_account_or_one_without_applicable_credits_is_refused |  |
| 17 | codex/resets.rs | `if credit.is_some() && credit != pending.item.as_deref() {` → `if false {` | codex::resets | KILLED by a_claim_left_pending_by_an_earlier_build_is_honoured |  |
| 18 | codex/resets.rs | `journal.finish(Service::Codex, &account.name, &pending.request_id)?;` → `(removed)` | codex::resets::a_claim_sends | KILLED by a_claim_sends_one_id_and_clears_it_on_an_answer |  |
| 20 | codex/resets.rs | `.min()` → `.max()` | codex::resets::the_first_expiry | KILLED by the_first_expiry_is_put_beside_the_count_only_where_credits_are_held |  |
| 20b | codex/resets.rs | `.filter(\|credit\| credit.status == CreditStatus::Available) .filter_m…` → `.filter_map(\|credit\| credit.expires_at)` | codex::resets::the_first_expiry | KILLED by the_first_expiry_is_put_beside_the_count_only_where_credits_are_held |  |
| 21 | codex/resets.rs | `.is_some_and(\|count\| count.available > 0)` → `.is_some_and(\|count\| count.available >= 0)` | codex::resets::the_first_expiry | KILLED by the_first_expiry_is_put_beside_the_count_only_where_credits_are_held |  |
| 21b | codex/resets.rs | `if let Some(error) = &plan.probe.error {` → `if let (Some(error), false) = (&plan.probe.error, true) {` | codex::resets | SURVIVED | fixed: killed in round 2 by an_unlimited_account_or_one_without_applicable_credits_is_refused |
| 21c | codex/resets.rs | `Some(credit) if credit.status != CreditStatus::Available => {` → `Some(credit) if false && credit.status != CreditStatus::Available => {` | codex::resets::an_unlimited | KILLED by an_unlimited_account_or_one_without_applicable_credits_is_refused |  |
| 25 | codex/auth.rs | `self.token_str("refresh_token") .filter(\|token\| !token.is_empty())` → `self.token_str("refresh_token")` | codex::auth | KILLED by an_empty_refresh_token_is_absent_and_blocks_seating |  |
| 26 | codex/auth.rs | `0o600, ) } } /// The parsed contents` → `0o644, ) } } /// The parsed contents` | codex::auth | KILLED by writing_tokens_keeps_unmodelled_keys_and_lands_owner_only |  |
| 27 | codex/auth.rs | `object.entry("OPENAI_API_KEY").or_insert(Value::Null);` → `object.insert("OPENAI_API_KEY".into(), Value::Null);` | codex::auth | KILLED by writing_tokens_keeps_unmodelled_keys_and_lands_owner_only |  |
| 28 | codex/auth.rs | `Some(id) => Some(id.to_owned()),` → `Some(_) => self.claims()?.chatgpt_account_id,` | codex:: | SURVIVED | fixed: killed in round 2 by the_account_id_falls_back_to_the_id_token_claim |
| 29 | codex/auth.rs | `if bundle.access_token.is_empty() \|\| bundle.refresh_token.is_empty()…` → `if bundle.access_token.is_empty() {` | codex::auth | KILLED by an_empty_refresh_token_is_absent_and_blocks_seating |  |
| 29b | codex/auth.rs | `let auth = value.get("https://api.openai.com/auth");` → `let auth = Some(&value);` | codex::auth | KILLED by id_claims_come_from_the_namespaced_payload |  |
| 32 | codex/refresh.rs | `_ if status == reqwest::StatusCode::UNAUTHORIZED => Failure::Invalidat…` → `_ if status == reqwest::StatusCode::UNAUTHORIZED => Failure::Transient,` | codex::refresh | KILLED by failures_classify_by_code_then_status |  |
| 33 | codex/refresh.rs | `.unwrap_or_else(\|\| refresh_token.to_owned()),` → `.unwrap_or_default(),` | codex:: | SURVIVED | fixed: killed in round 2 by a_refresh_lands_the_rotated_bundle_and_keeps_what_the_answer_left_out |
| 33b | codex/refresh.rs | `.ok_or_else(\|\| RefreshError::transient("refresh response carried no …` → `.unwrap_or_default();` | codex:: | SURVIVED | fixed: killed in round 2 by a_refresh_lands_the_rotated_bundle_and_keeps_what_the_answer_left_out |
| 34 | codex/refresh.rs | `bundle.account_id = account_id.or_else(\|\| {` → `bundle.account_id = None.or_else(\|\| {` | codex:: | SURVIVED | fixed: killed in round 2 by a_refresh_lands_the_rotated_bundle_and_keeps_what_the_answer_left_out |
| 35 | codex/refresh.rs | `if !auth.is_stale(max_age_days, now) {` → `if false {` | codex::refresh | KILLED by a_fresh_profile_is_left_alone_unless_forced |  |
| 36 | codex/maintenance.rs | `if account.access_only {` → `if false {` | codex::maintenance | SURVIVED | fixed: killed in round 2 by maintenance_refreshes_rejected_accounts_and_reports_login_needs_once |
| 37 | codex/maintenance.rs | `let force = rejected.contains(account.name.as_str());` → `let force = false;` | codex::maintenance | KILLED by maintenance_reports_transient_failures_every_sweep_and_a_reused_token_once |  |
| 38 | codex/maintenance.rs | `.filter(\|probe\| probe.error.as_ref().is_some_and(ReadError::is_unaut…` → `.filter(\|probe\| probe.error.is_some())` | codex::maintenance | KILLED by a_challenged_probe_does_not_force_a_refresh |  |
| 39 | codex/maintenance.rs | `if needs_login.insert(account.name.clone()) {` → `if needs_login.insert(account.name.clone()) \|\| true {` | codex::maintenance | KILLED by maintenance_refreshes_rejected_accounts_and_reports_login_needs_once |  |
| 39b | codex/maintenance.rs | `if permanent.is_none() && needs_login.remove(&account.name) {` → `if false && needs_login.remove(&account.name) {` | codex::maintenance | KILLED by maintenance_refreshes_rejected_accounts_and_reports_login_needs_once |  |
| 40 | codex/keep.rs | `expires_at.is_some_and(\|at\| at - now <= RENEW_WITHIN_SECS)` → `expires_at.is_some_and(\|at\| at - now < RENEW_WITHIN_SECS)` | codex::keep | KILLED by a_token_is_due_a_day_before_it_expires |  |
| 41 | codex/keep.rs | `if moved_by_id \|\| moved_by_email {` → `if false {` | codex::keep | KILLED by no_socket_or_another_account_ends_the_seat_and_sends_nothing |  |
| 41b | codex/keep.rs | `if moved_by_id \|\| moved_by_email {` → `if moved_by_email {` | codex::keep | SURVIVED | fixed: killed in round 2 by no_socket_or_another_account_ends_the_seat_and_sends_nothing |
| 41c | codex/keep.rs | `if moved_by_id \|\| moved_by_email {` → `if moved_by_id {` | codex::keep | SURVIVED | fixed: killed in round 2 by no_socket_or_another_account_ends_the_seat_and_sends_nothing |
| 42 | codex/keep.rs | `let dead = limits.is_err() && !due` → `let dead = limits.is_ok() && !due` | codex::keep | KILLED by a_token_with_time_left_is_left_alone_and_a_newer_one_on_disk_is_pushed |  |
| 42b | codex/keep.rs | `.as_ref() .is_some_and(ReadError::is_unauthorized);` → `.as_ref() .is_none_or(ReadError::is_unauthorized);` | codex::keep | KILLED by a_token_the_backend_rejects_early_is_renewed_at_the_next_look |  |
| 43 | codex/keep.rs | `Err(error) if nobody_listens(&error) => return Ok(Step::Ended),` → `(removed)` | codex::keep | KILLED by no_socket_or_another_account_ends_the_seat_and_sends_nothing |  |
| 46 | codex/keep.rs | `force \|\| expiring,` → `force,` | codex::keep | KILLED by a_token_near_expiry_is_renewed_through_the_profile_and_pushed |  |
| 47 | codex/keep.rs | `let _ = seats.forget(&seat.home);` → `(removed)` | codex::keep | KILLED by no_socket_or_another_account_ends_the_seat_and_sends_nothing |  |
| 47b | codex/keep.rs | `if due(seat.expires_at, now) \|\| dead {` → `if dead {` | codex::keep | KILLED by a_token_near_expiry_is_renewed_through_the_profile_and_pushed |  |
| 47c | codex/keep.rs | `let _ = seats.trouble(seat, Some(trouble.clone()));` → `(removed)` | codex::keep | KILLED by a_seat_whose_profile_left_the_fleet_is_trouble |  |
| 47d | codex/keep.rs | `true => Err((` → `true if false => Err((` | codex::keep | NOCOMPILE | did not compile; not evidence |
| 48 | codex/seats.rs | `.find(\|seat\| seat.home == of.home && seat.token == of.token)` → `.find(\|seat\| seat.home == of.home)` | codex::seats | KILLED by seats_are_kept_per_home_and_trouble_sticks_to_the_token_it_was_about |  |
| 49 | codex/seats.rs | `seats.retain(\|known\| known.home != seat.home); seats.push(seat);` → `seats.push(seat);` | codex::seats | KILLED by seats_are_kept_per_home_and_trouble_sticks_to_the_token_it_was_about |  |
| 50 | codex/seats.rs | `write_atomic(&self.path, &bytes, 0o600)` → `write_atomic(&self.path, &bytes, 0o644)` | codex::seats | SURVIVED | fixed: killed in round 2 by seats_are_kept_per_home_and_trouble_sticks_to_the_token_it_was_about |
| 51 | codex/mover.rs | `.filter(\|account\| !seat.is_on(account))` → `.filter(\|_\| true)` | codex::mover | KILLED by a_seat_below_its_floor_moves_to_the_best_profile_with_landing_room |  |
| 52 | codex/mover.rs | `.filter(\|account\| !account.parked)` → `.filter(\|_\| true)` | codex::mover | KILLED by a_low_seat_drains_when_nowhere_has_landing_room_and_a_spent_one_takes_scraps |  |
| 53 | codex/mover.rs | `if policy::above(&seat.reading, floors.leave, None) {` → `if false {` | codex::mover | KILLED by a_healthy_seat_is_left_alone_and_nothing_is_probed |  |
| 54 | codex/mover.rs | `if dry_run { look.outcome` → `if false { look.outcome` | codex::mover | KILLED by a_seat_below_its_floor_moves_to_the_best_profile_with_landing_room |  |
| 55 | codex/mover.rs | `kept.len().saturating_sub(KEPT_MOVES)` → `kept.len().saturating_sub(0)` | codex::mover | KILLED by moves_are_appended_read_back_and_compacted |  |
| 57 | codex/mover.rs | `session: self.land.session.max(min_remaining),` → `session: self.land.session.min(min_remaining),` | codex::mover | KILLED by one_leaving_floor_raises_the_landing_floor_to_it |  |
| 59 | codex/mover.rs | `allowed: Some(!limits.refused),` → `allowed: Some(true),` | codex::mover | SURVIVED | fixed: killed in round 2 by a_refused_seat_moves_with_room_in_its_windows_and_one_without_windows_is_left |
| 59b | codex/mover.rs | `if seat.reading.limits.is_empty() && seat.reading.allows() {` → `if false {` | codex::mover | SURVIVED | fixed: killed in round 2 by a_refused_seat_moves_with_room_in_its_windows_and_one_without_windows_is_left |
| 59c | codex/mover.rs | `(Some(seated), Some(held)) => *seated == held, _ => {` → `(Some(seated), Some(held)) if false => *seated == held, _ => {` | codex::mover | SURVIVED | fixed: killed in round 2 by a_seat_is_matched_by_account_id_before_email |
| 59d | codex/mover.rs | `let record_error = seats .put(` → `let record_error = Seats::at("/nonexistent-dir-x/y/seats.json") .put(` | codex::mover | KILLED by a_seat_below_its_floor_moves_to_the_best_profile_with_landing_room |  |
| 60 | codex/appserver.rs | `refused: reached \|\| disallowed,` → `refused: disallowed,` | codex::appserver | KILLED by the_account_wide_windows_are_read_and_named_by_length |  |
| 60b | codex/appserver.rs | `refused: reached \|\| disallowed,` → `refused: reached,` | codex::appserver | KILLED by the_account_wide_windows_are_read_and_named_by_length |  |
| 60c | codex/appserver.rs | `minutes.map(\|minutes\| minutes * 60)` → `minutes.map(\|minutes\| minutes)` | codex::appserver | KILLED by the_account_wide_windows_are_read_and_named_by_length |  |
| 60d | codex/appserver.rs | `let snapshot = value.get("rateLimits").unwrap_or(value);` → `let snapshot = value;` | codex::appserver | KILLED by the_account_wide_windows_are_read_and_named_by_length |  |
| 60e | codex/appserver.rs | `.join(CONTROL_SOCKET_DIR) .join(CONTROL_SOCKET_FILE)` → `.join("ipc") .join("ipc.sock")` | codex::appserver, codex::mover::a_healthy | KILLED by the_control_socket_is_not_the_ipc_socket |  |
| 60f | codex/appserver.rs | `"capabilities": { "experimentalApi": true },` → `"capabilities": { "experimentalApi": false },` | codex::appserver | SURVIVED | fixed: killed in round 2 by the_client_reads_limits_and_the_account_and_a_login_moves_the_server |
| 61 | providers/zai.rs | `if !envelope.success \|\| envelope.code != Some(200) {` → `if !envelope.success {` | providers::zai | SURVIVED | fixed: killed in round 2 by success_needs_both_the_flag_and_code_200 |
| 61b | providers/zai.rs | `if !envelope.success \|\| envelope.code != Some(200) {` → `if envelope.code != Some(200) {` | providers::zai | SURVIVED | fixed: killed in round 2 by success_needs_both_the_flag_and_code_200 |
| 62 | providers/zai.rs | `.or_else(\|\| self.remaining.map(\|remaining\| limit - remaining))` → `.or_else(\|\| self.remaining.map(\|remaining\| remaining))` | providers::zai | SURVIVED | fixed: killed in round 2 by used_comes_from_remaining_when_the_count_is_missing_and_may_pass_the_limit |
| 63 | providers/zai.rs | `_ => self.percentage? / 100.0,` → `_ => self.percentage? / 1.0,` | providers::zai | KILLED by legacy_token_and_mcp_limits_are_kept_apart |  |
| 64 | providers/zai.rs | `at <= now + span + MINUTE` → `at <= now + span * 100` | providers::zai | KILLED by a_reset_beyond_the_window_length_is_dropped |  |
| 64b | providers/zai.rs | `(Some(used), Some(limit)) => used.max(0.0) / limit,` → `(Some(_), Some(_)) => self.percentage? / 100.0,` | providers::zai | KILLED by credit_windows_read_used_over_limit_with_their_resets |  |
| 67 | providers/zai.rs | `meter.group().is_some(), meter.span.seconds().unwrap_or(i64::MAX),` → `false, 0,` | providers::zai | KILLED by legacy_token_and_mcp_limits_are_kept_apart |  |
| 67b | providers/zai.rs | `if meters.is_empty() { return Err(decode("the answer lists no limits t…` → `if false { return Err(decode("the answer lists no limits tokeman knows…` | providers::zai | SURVIVED | fixed: killed in round 2 by success_needs_both_the_flag_and_code_200 |
| 67c | providers/zai.rs | `if !status.is_success() {` → `if false {` | providers::zai | SURVIVED | fixed: killed in round 2 by an_http_failure_is_reported_with_its_status |
| 71 | providers/kimi.rs | `if balance.kind.as_deref() != Some("BOOSTER") {` → `if false {` | providers::kimi | KILLED by a_wallet_that_is_not_a_booster_or_is_empty_is_no_balance |  |
| 72 | providers/kimi.rs | `number(balance.amount.as_ref()).filter(\|&amount\| amount > 0.0)?` → `number(balance.amount.as_ref())?` | providers::kimi | KILLED by a_wallet_that_is_not_a_booster_or_is_empty_is_no_balance |  |
| 73 | providers/kimi.rs | `if cents > 0.0 && cents < 1.0 {` → `if false {` | providers::kimi | KILLED by new_plan_monthly_pools_and_the_camel_case_wallet_are_read |  |
| 74 | providers/kimi.rs | `#[serde(default, alias = "boosterWallet")]` → `#[serde(default)]` | providers::kimi | KILLED by new_plan_monthly_pools_and_the_camel_case_wallet_are_read |  |
| 74b | providers/kimi.rs | `if meters.is_empty() { return Err(decode("the answer has no usage wind…` → `if false { return Err(decode("the answer has no usage windows"));` | providers::kimi | KILLED by a_wallet_that_is_not_a_booster_or_is_empty_is_no_balance |  |
| 74c | providers/kimi.rs | `401 => format!("Kimi refused the key: {message}"),` → `401 => message.clone(),` | providers::kimi | SURVIVED | fixed: killed in round 2 by read_sends_bearer_and_survives_a_failed_me |
| 74d | providers/kimi.rs | `Value::String(text) => text.trim().parse().ok(), _ => None, } .filter` → `Value::String(_) => None, _ => None, } .filter` | providers::kimi | KILLED by new_plan_monthly_pools_and_the_camel_case_wallet_are_read |  |
| 80 | providers/mod.rs | `let key = contents.trim();` → `let key = contents.as_str();` | providers | KILLED by the_key_comes_inline_or_trimmed_from_its_file |  |
| 80b | providers/mod.rs | `.map(str::trim) .filter(\|key\| !key.is_empty())` → `.filter(\|key\| !key.is_empty())` | providers | KILLED by the_key_comes_inline_or_trimmed_from_its_file |  |
| 82 | providers/antigravity/mod.rs | `(tier("paidTier").or_else(\|\| tier("currentTier")), project)` → `(tier("currentTier").or_else(\|\| tier("paidTier")), project)` | providers::antigravity | KILLED by the_plan_is_the_paid_tier_not_the_current_one |  |
| 83 | providers/antigravity/mod.rs | `if bucket.disabled { continue;` → `if false { continue;` | providers::antigravity | KILLED by missing_fractions_disabled_buckets_and_wrappers |  |
| 84 | providers/antigravity/mod.rs | `let idle = remaining >= 1.0 && floating;` → `let idle = remaining >= 1.0;` | providers::antigravity | KILLED by an_untouched_window_has_no_reset_but_an_anchored_full_one_keeps_its_own |  |
| 84b | providers/antigravity/mod.rs | `let idle = remaining >= 1.0 && floating;` → `let idle = floating;` | providers::antigravity | SURVIVED | not addressed: a spent window whose reset equals now+span cannot be told from an idle one; see Not tested |
| 85 | providers/antigravity/mod.rs | `(None, None) if reset.is_some() && !floating => 0.0,` → `(None, None) if reset.is_some() => 0.0,` | providers::antigravity | KILLED by missing_fractions_disabled_buckets_and_wrappers |  |
| 86 | providers/antigravity/mod.rs | `if status == StatusCode::UNAUTHORIZED && !self.fresh {` → `if status == StatusCode::UNAUTHORIZED && !self.fresh && false {` | providers::antigravity | KILLED by an_unexpired_token_is_used_and_a_rejected_one_is_refreshed_once |  |
| 87 | providers/antigravity/mod.rs | `Err(_) => (None, None), }; let project` → `Err(error) => return Err(error), }; let project` | providers::antigravity | SURVIVED | fixed: killed in round 2 by quota_is_read_without_the_plan_when_load_code_assist_is_down |
| 88 | providers/antigravity/mod.rs | `if cached.refresh_fingerprint != Fingerprint::of(refresh_token) {` → `if false {` | providers::antigravity | KILLED by a_poll_refreshes_caches_and_reads_the_summary |  |
| 89 | providers/antigravity/mod.rs | `(!token.is_empty() && expires_at > self.now_ms + EXPIRY_MARGIN_MS)` → `(!token.is_empty() && expires_at > 0)` | providers::antigravity | SURVIVED | fixed: killed in round 2 by a_poll_refreshes_caches_and_reads_the_summary |
| 89b | providers/antigravity/mod.rs | `let project = project.unwrap_or_else(\|\| CONSUMER_PROJECT.into());` → `let project = project.unwrap_or_else(\|\| "x".into());` | providers::antigravity | SURVIVED | fixed: killed in round 2 by quota_is_read_without_the_plan_when_load_code_assist_is_down |
| 89c | providers/antigravity/mod.rs | `(None, Some(amount)) if amount_is_zero(amount) => 0.0,` → `(None, Some(amount)) if !amount_is_zero(amount) => 0.0,` | providers::antigravity | KILLED by missing_fractions_disabled_buckets_and_wrappers |  |
| 89d | providers/antigravity/mod.rs | `let body = value.get("response").unwrap_or(value); let summary` → `let body = value; let summary` | providers::antigravity | KILLED by missing_fractions_disabled_buckets_and_wrappers |  |
| 93 | providers/antigravity/grant.rs | `if value.get("error").and_then(Value::as_str) == Some("invalid_grant")…` → `if false {` | providers::antigravity, providers::antigravity::seat::a_dead_grant | KILLED by a_revoked_grant_is_an_error_usage_that_names_the_fix |  |
| 95 | providers/antigravity/grant.rs | `if number == 0 { return None; }` → `(removed)` | providers::antigravity::grant | SURVIVED | not addressed: equivalent on every reachable input (a field numbered 0 cannot follow a valid key) |
| 95b | providers/antigravity/grant.rs | `.filter(\|email\| email.contains('@'))` → `.filter(\|_\| true)` | providers::antigravity::grant | SURVIVED | fixed: killed in round 2 by the_app_user_status_names_the_account |
| 95c | providers/antigravity/grant.rs | `.context("agy's login names no account (id_token_email)")?;` → `.unwrap_or("");` | providers::antigravity::grant | SURVIVED | fixed: killed in round 2 by the_agy_login_parses_with_and_without_the_go_keyring_prefix |
| 96 | providers/antigravity/seat.rs | `b"+/=".contains(&b)` → `true` | providers::antigravity::seat::a_value_that | KILLED by a_value_that_could_break_out_of_the_security_command_is_refused |  |
| 97 | providers/antigravity/seat.rs | `let blocked = !already && !options.force` → `let blocked = false && !options.force` | providers::antigravity::seat | KILLED by an_interactive_agy_refuses_the_switch_and_says_how_to_resume_each |  |
| 97b | providers/antigravity/seat.rs | `let blocked = !already && !options.force` → `let blocked = !already && true` | providers::antigravity::seat | KILLED by an_interactive_agy_refuses_the_switch_and_says_how_to_resume_each |  |
| 98 | providers/antigravity/seat.rs | `Err(error) if options.force \|\| options.dry_run =>` → `Err(error) if true =>` | providers::antigravity::seat | KILLED by an_untakeable_census_stops_the_switch_unless_forced |  |
| 99 | providers/antigravity/seat.rs | `.save(&place.backup)?;` → `;` | providers::antigravity::seat | KILLED by seating_writes_agys_own_shape_and_backs_up_what_it_replaced |  |
| 100 | providers/antigravity/seat.rs | `if previous.as_ref() == Some(target) {` → `if false {` | providers::antigravity::seat | KILLED by seating_the_account_agy_already_holds_writes_nothing |  |
| 102 | providers/antigravity/seat.rs | `let _ = write_raw(&place.security, raw);` → `(removed)` | providers::antigravity::seat | SURVIVED | fixed: killed in round 2 by a_write_the_keychain_does_not_keep_puts_the_old_login_back |
| 103 | providers/antigravity/seat.rs | `if place.token_file.exists() {` → `if true {` | providers::antigravity::seat | KILLED by an_existing_fallback_file_follows_the_slot_and_a_missing_one_stays_missing |  |
| 105 | providers/antigravity/seat.rs | `\|\| account.name == self.email)` → `)` | providers::antigravity::seat | KILLED by the_slot_matches_an_account_by_grant_or_by_email |  |
| 107 | providers/antigravity/seat.rs | `if blocked \|\| options.dry_run {` → `if blocked {` | providers::antigravity::seat | KILLED by the_hub_is_told_and_confirmed_and_an_acp_server_on_the_old_account_is_reported |  |
| 107b | providers/antigravity/seat.rs | `ClientResult::WouldTell => match clients::reread(client).await {` → `ClientResult::WouldTell => match Ok::<(), anyhow::Error>(()) {` | providers::antigravity::seat | KILLED by a_hub_that_cannot_be_told_is_a_warning_and_the_seating_stands |  |
| 107d | providers/antigravity/seat.rs | `if kept.as_deref() != Some(value) {` → `if false {` | providers::antigravity::seat | KILLED by a_write_the_keychain_does_not_keep_puts_the_old_login_back |  |
| 107e | providers/antigravity/seat.rs | `.or_else(\|\| account.name.contains('@').then(\|\| account.name.clone(…` → `.or_else(\|\| Some(account.name.clone())) .with_context(\|\| format!("…` | providers::antigravity::seat | SURVIVED | fixed: killed in round 2 by a_dead_grant_or_another_service_is_refused_before_any_keychain_call |
| 110 | providers/antigravity/clients.rs | `alive.contains(&file.pid).then(\|\| Running {` → `true.then(\|\| Running {` | providers::antigravity::clients | KILLED by the_census_finds_live_hubs_and_every_agy_with_its_directory_and_ports |  |
| 111 | providers/antigravity/clients.rs | `&& id.len() <= 128 && id .chars() .all(\|c\| c.is_ascii_alphanumeric()…` → `&& id.len() <= 128` | providers::antigravity::clients | KILLED by the_resume_hint_is_the_last_conversation_recorded_for_the_directory |  |
| 111b | providers/antigravity/clients.rs | `command == "agy" \|\| command.ends_with("/agy")` → `command.ends_with("agy")` | providers::antigravity::clients | KILLED by an_agy_command_line_is_an_acp_server_a_print_run_or_interactive |  |
| 111c | providers/antigravity/clients.rs | `if let Some(home) = home { sessions.push(` → `if let (Some(home), false) = (home, true) { sessions.push(` | providers::antigravity::clients | KILLED by an_agy_on_a_per_account_home_is_a_session_not_a_client_of_the_slot |  |
| 111d | providers/antigravity/clients.rs | `f.write_str("Csrf(..)")` → `write!(f, "Csrf({})", self.0)` | providers::antigravity::clients | KILLED by the_census_finds_live_hubs_and_every_agy_with_its_directory_and_ports |  |
| 111e | providers/antigravity/clients.rs | `.rev() .filter_map(\|line\| serde_json::from_str::<HistoryLine>` → `.filter_map(\|line\| serde_json::from_str::<HistoryLine>` | providers::antigravity::clients | KILLED by the_resume_hint_is_the_last_conversation_recorded_for_the_directory |  |
| 114 | providers/antigravity/run.rs | `const NOT_LINKED: &[&str] = &[".gemini", "Library"];` → `const NOT_LINKED: &[&str] = &[".gemini"];` | providers::antigravity::run | KILLED by an_account_gets_a_private_home_with_its_own_token_and_the_users_settings |  |
| 115 | providers/antigravity/run.rs | `env: vec![("HOME", home.into_os_string()), ("SSH_TTY", ssh_tty)],` → `env: vec![("HOME", home.into_os_string())],` | providers::antigravity::run | KILLED by the_started_agy_gets_exactly_the_home_and_tty_and_the_arguments |  |
| 116 | providers/antigravity/run.rs | `if seated { return Ok(Launch {` → `if false { return Ok(Launch {` | providers::antigravity::run | KILLED by the_account_agy_already_holds_runs_plain_agy |  |
| 117 | providers/antigravity/run.rs | `let token_text = if current {` → `let token_text = if false {` | providers::antigravity::run | KILLED by a_second_run_keeps_the_file_agy_refreshed_and_follows_the_real_home |  |
| 118 | providers/antigravity/run.rs | `if present(&copy)? { continue;` → `if false { continue;` | providers::antigravity::run | KILLED by a_second_run_keeps_the_file_agy_refreshed_and_follows_the_real_home |  |
| 119 | providers/antigravity/run.rs | `write_atomic(&home.join(TOKEN_FILE), text.as_bytes(), 0o600)?;` → `write_atomic(&home.join(TOKEN_FILE), text.as_bytes(), 0o644)?;` | providers::antigravity::run | KILLED by an_account_gets_a_private_home_with_its_own_token_and_the_users_settings |  |
| 120 | providers/antigravity/run.rs | `set_mode(&dir, 0o700)?; } link_real_home` → `set_mode(&dir, 0o755)?; } link_real_home` | providers::antigravity::run | KILLED by an_account_gets_a_private_home_with_its_own_token_and_the_users_settings |  |
| 121 | providers/antigravity/run.rs | `if ours && (unwanted \|\| !present(&target)?) {` → `if unwanted \|\| !present(&target)? {` | providers::antigravity::run | SURVIVED | fixed: killed in round 2 by a_second_run_keeps_the_file_agy_refreshed_and_follows_the_real_home |
| 121b | providers/antigravity/run.rs | `if std::fs::read_link(&copy).is_ok_and(\|target\| target == source) {` → `if false {` | providers::antigravity::run | KILLED by settings_are_seeded_once_and_an_earlier_link_to_the_real_file_becomes_a_copy |  |
| 121c | providers/antigravity/run.rs | `.unwrap_or_else(\|\| OsString::from("/dev/tty"));` → `.unwrap_or_else(\|\| OsString::from("/dev/null"));` | providers::antigravity::run | KILLED by the_started_agy_gets_exactly_the_home_and_tty_and_the_arguments |  |
| 121d | providers/antigravity/run.rs | `let ssh_tty = place .ssh_tty .clone()` → `let ssh_tty = None::<OsString>` | providers::antigravity::run | KILLED by an_ssh_tty_already_set_is_kept_and_other_services_are_refused |  |
| 121e | providers/antigravity/run.rs | `write_atomic(&copy, bytes.as_bytes(), 0o600)?;` → `write_atomic(&copy, bytes.as_bytes(), 0o644)?;` | providers::antigravity::run | KILLED by an_account_gets_a_private_home_with_its_own_token_and_the_users_settings |  |
| 121f | providers/antigravity/run.rs | `.trim_start_matches('.')` → `(removed)` | providers::antigravity::run | KILLED by sessions_name_the_account_whose_home_they_run_on |  |

Round 2 and 3: every new or strengthened test run against the mutation it exists for (all KILLED but h1, a probe of whether anything pins `~/.codex` as the default home: nothing does; see Not tested). Rows whose # repeats a round-1 number are the round-1 survivors again.

| # | file | mutation (old → new) | outcome |
|---|---|---|---|
| 14 | codex/mod.rs | `response.headers().contains_key("cf-mitigated")` → `false` | KILLED by only_a_403_that_looks_like_the_edge_is_a_challenge |
| 14b | codex/mod.rs | `.is_some_and(\|value\| value.starts_with("text/html"))` → `.is_some_and(\|_\| false)` | KILLED by only_a_403_that_looks_like_the_edge_is_a_challenge |
| 14c | codex/mod.rs | `response.status() == reqwest::StatusCode::FORBIDDEN &&` → `true &&` | KILLED by only_a_403_that_looks_like_the_edge_is_a_challenge |
| p1 | codex/probe.rs | `Window::new(used, resets_at), ))` → `Window::new(used.min(1.0), resets_at), ))` | KILLED by a_window_past_its_limit_reads_over_one_and_an_answer_without_windows_offers_no_room |
| p2 | codex/probe.rs | `None \| Some(Value::Null) => None,` → `None => None, Some(Value::Null) => Some(RawRateLimit::default()),` | KILLED by a_window_past_its_limit_reads_over_one_and_an_answer_without_windows_offers_no_room |
| p3 | codex/probe.rs | `seen.keys("usage", body, KNOWN_KEYS);` → `(removed)` | KILLED by a_window_past_its_limit_reads_over_one_and_an_answer_without_windows_offers_no_room |
| p4 | codex/probe.rs | `let used = self.used_percent? / 100.0;` → `let used = self.used_percent.unwrap_or(0.0) / 100.0;` | KILLED by a_window_past_its_limit_reads_over_one_and_an_answer_without_windows_offers_no_room |
| 21b | codex/resets.rs | `if let Some(error) = &plan.probe.error {` → `if let (Some(error), false) = (&plan.probe.error, true) {` | KILLED by an_unlimited_account_or_one_without_applicable_credits_is_refused |
| 28 | codex/auth.rs | `Some(id) => Some(id.to_owned()),` → `Some(_) => self.claims()?.chatgpt_account_id,` | KILLED by the_account_id_falls_back_to_the_id_token_claim |
| 33 | codex/refresh.rs | `.unwrap_or_else(\|\| refresh_token.to_owned()),` → `.unwrap_or_default(),` | KILLED by a_refresh_lands_the_rotated_bundle_and_keeps_what_the_answer_left_out |
| 33b | codex/refresh.rs | `.ok_or_else(\|\| RefreshError::transient("refresh response carried no …` → `.unwrap_or_default();` | KILLED by a_refresh_lands_the_rotated_bundle_and_keeps_what_the_answer_left_out |
| 34 | codex/refresh.rs | `bundle.account_id = account_id.or_else(\|\| {` → `bundle.account_id = None.or_else(\|\| {` | KILLED by a_refresh_lands_the_rotated_bundle_and_keeps_what_the_answer_left_out |
| 36 | codex/maintenance.rs | `if account.access_only {` → `if false {` | KILLED by maintenance_refreshes_rejected_accounts_and_reports_login_needs_once |
| 41b | codex/keep.rs | `if moved_by_id \|\| moved_by_email {` → `if moved_by_email {` | KILLED by no_socket_or_another_account_ends_the_seat_and_sends_nothing |
| 41c | codex/keep.rs | `if moved_by_id \|\| moved_by_email {` → `if moved_by_id {` | KILLED by no_socket_or_another_account_ends_the_seat_and_sends_nothing |
| 50 | codex/seats.rs | `write_atomic(&self.path, &bytes, 0o600)` → `write_atomic(&self.path, &bytes, 0o644)` | KILLED by seats_are_kept_per_home_and_trouble_sticks_to_the_token_it_was_about |
| 59 | codex/mover.rs | `allowed: Some(!limits.refused),` → `allowed: Some(true),` | KILLED by a_refused_seat_moves_with_room_in_its_windows_and_one_without_windows_is_left |
| 59b | codex/mover.rs | `if seat.reading.limits.is_empty() && seat.reading.allows() {` → `if false {` | KILLED by a_refused_seat_moves_with_room_in_its_windows_and_one_without_windows_is_left |
| 59c | codex/mover.rs | `(Some(seated), Some(held)) => *seated == held, _ => {` → `(Some(seated), Some(held)) if false => *seated == held, _ => {` | KILLED by a_seat_is_matched_by_account_id_before_email |
| 59e | codex/mover.rs | `if !homes.contains(&home) {` → `if true {` | KILLED by the_default_home_comes_first_and_each_home_once |
| 60f | codex/appserver.rs | `"capabilities": { "experimentalApi": true },` → `"capabilities": { "experimentalApi": false },` | KILLED by the_client_reads_limits_and_the_account_and_a_login_moves_the_server |
| 61 | providers/zai.rs | `if !envelope.success \|\| envelope.code != Some(200) {` → `if !envelope.success {` | KILLED by success_needs_both_the_flag_and_code_200 |
| 61b | providers/zai.rs | `if !envelope.success \|\| envelope.code != Some(200) {` → `if envelope.code != Some(200) {` | KILLED by success_needs_both_the_flag_and_code_200 |
| 62 | providers/zai.rs | `.or_else(\|\| self.remaining.map(\|remaining\| limit - remaining))` → `.or_else(\|\| self.remaining.map(\|remaining\| remaining))` | KILLED by used_comes_from_remaining_when_the_count_is_missing_and_may_pass_the_limit |
| 62b | providers/zai.rs | `(Some(used), Some(limit)) => used.max(0.0) / limit,` → `(Some(used), Some(limit)) => used.max(0.0).min(limit) / limit,` | KILLED by used_comes_from_remaining_when_the_count_is_missing_and_may_pass_the_limit |
| 67b | providers/zai.rs | `if meters.is_empty() { return Err(decode("the answer lists no limits t…` → `if false { return Err(decode("the answer lists no limits tokeman knows…` | KILLED by success_needs_both_the_flag_and_code_200 |
| 67c | providers/zai.rs | `if !status.is_success() {` → `if false {` | KILLED by an_http_failure_is_reported_with_its_status |
| 74c | providers/kimi.rs | `401 => format!("Kimi refused the key: {message}"),` → `401 => message.clone(),` | KILLED by read_sends_bearer_and_survives_a_failed_me |
| 86 | providers/antigravity/mod.rs | `if status == StatusCode::UNAUTHORIZED && !self.fresh {` → `if status == StatusCode::UNAUTHORIZED {` | KILLED by a_token_rejected_even_after_a_refresh_is_an_error_after_one_refresh |
| 87 | providers/antigravity/mod.rs | `Err(_) => (None, None), }; let project` → `Err(error) => return Err(error), }; let project` | KILLED by quota_is_read_without_the_plan_when_load_code_assist_is_down |
| 89 | providers/antigravity/mod.rs | `(!token.is_empty() && expires_at > self.now_ms + EXPIRY_MARGIN_MS)` → `(!token.is_empty() && expires_at > 0)` | KILLED by a_poll_refreshes_caches_and_reads_the_summary |
| 89a | providers/antigravity/mod.rs | `(!token.is_empty() && expires_at > self.now_ms + EXPIRY_MARGIN_MS)` → `(!token.is_empty() && expires_at > self.now_ms)` | KILLED by a_poll_refreshes_caches_and_reads_the_summary |
| 89b | providers/antigravity/mod.rs | `let project = project.unwrap_or_else(\|\| CONSUMER_PROJECT.into());` → `let project = project.unwrap_or_else(\|\| "x".into());` | KILLED by quota_is_read_without_the_plan_when_load_code_assist_is_down |
| ua1 | providers/antigravity/mod.rs | `v >= MIN_GEMINI_HUB_VERSION && Some(v) > parse_version(PINNED_HUB_VERS…` → `v >= MIN_GEMINI_HUB_VERSION` | KILLED by the_user_agent_is_a_hub_client_new_enough_for_the_gemini_group |
| ua2 | providers/antigravity/mod.rs | `.unwrap_or_else(\|\| PINNED_HUB_VERSION.into()); let os` → `.unwrap_or_else(\|\| "2.0.1".into()); let os` | KILLED by the_user_agent_is_a_hub_client_new_enough_for_the_gemini_group |
| 95b | providers/antigravity/grant.rs | `.filter(\|email\| email.contains('@'))` → `.filter(\|_\| true)` | KILLED by the_app_user_status_names_the_account |
| 95c | providers/antigravity/grant.rs | `.context("agy's login names no account (id_token_email)")?;` → `.unwrap_or("");` | KILLED by the_agy_login_parses_with_and_without_the_go_keyring_prefix |
| 102 | providers/antigravity/seat.rs | `let _ = write_raw(&place.security, raw);` → `(removed)` | KILLED by a_write_the_keychain_does_not_keep_puts_the_old_login_back |
| 107e | providers/antigravity/seat.rs | `.or_else(\|\| account.name.contains('@').then(\|\| account.name.clone(…` → `.or_else(\|\| Some(account.name.clone())) .with_context(\|\| format!("…` | KILLED by a_dead_grant_or_another_service_is_refused_before_any_keychain_call |
| 121 | providers/antigravity/run.rs | `if ours && (unwanted \|\| !present(&target)?) {` → `if unwanted \|\| !present(&target)? {` | KILLED by a_second_run_keeps_the_file_agy_refreshed_and_follows_the_real_home |
| c1 | codex/mover.rs | `Relocation::Drain => {` → `Relocation::Drain => { if let Some(account) = others.first() { let jou…` | KILLED by a_codex_with_nowhere_to_go_drains_and_no_reset_credit_is_spent |
| d1 | providers/mod.rs | `.field("refresh_token", &held(&self.refresh_token))` → `.field("refresh_token", &self.refresh_token)` | KILLED by debug_output_names_which_secrets_are_held_and_shows_none |
| d2 | providers/mod.rs | `.field("key", &held(&self.key))` → `.field("key", &self.key)` | KILLED by debug_output_names_which_secrets_are_held_and_shows_none |
| d3 | codex/auth.rs | `f.debug_struct("TokenBundle") .field("account_id", &self.account_id)` → `f.debug_struct("TokenBundle") .field("refresh_token", &self.refresh_to…` | KILLED by debug_output_of_a_bundle_or_an_auth_file_shows_no_token |
| d4 | codex/auth.rs | `f.debug_struct("AuthFile")` → `f.debug_struct("AuthFile") .field("raw", &self.raw)` | KILLED by debug_output_of_a_bundle_or_an_auth_file_shows_no_token |
| d5 | providers/antigravity/seat.rs | `f.debug_struct("Slot") .field("email", &self.email)` → `f.debug_struct("Slot") .field("refresh_token", &self.refresh_token) .f…` | KILLED by the_slot_matches_an_account_by_grant_or_by_email |
| h1 | codex/auth.rs | `if !home.is_absolute() { bail!("could not determine home directory"); …` → `Ok(Self::new(home.join(".codex-other")))` | SURVIVED |

### Deleted

None. No test in this group restated its fixture, passed vacuously as a whole, or duplicated another.
Two class-c tests were kept deliberately:

| test | why kept |
|---|---|
| codex::appserver::the_control_socket_is_not_the_ipc_socket | It asserts a path constant, which is the pattern ember objects to. The constant is Codex's own socket location, every other test builds the socket through the same function (so they agree with any value), and mutation 60e shows this is the one test that fails when the path changes. |
| codex::resets::consume_codes_use_the_backend_spellings | A serde-name round trip, but of the four words the backend answers a claim with; a misspelt one turns a definite answer into an "uncertain" claim that is retried forever. Nothing else pins them. |

### Strengthened

| test | what was weak | mutation it now catches |
|---|---|---|
| codex::maintenance::maintenance_refreshes_rejected_accounts_and_reports_login_needs_once | The access-only profile was fresh, so it was skipped by age, not by `access_only`: the skip could be deleted. | 36 |
| codex::resets::an_unlimited_account_or_one_without_applicable_credits_is_refused | No case for an account whose usage cannot be read; it fell through to "no applicable credit". | 21b |
| codex::auth::the_account_id_falls_back_to_the_id_token_claim | Stored id and claim were always equal, so which one wins was unpinned. | 28 |
| codex::seats::seats_are_kept_per_home_and_trouble_sticks_to_the_token_it_was_about | File mode of codex-seats.json not asserted. | 50 |
| codex::keep::no_socket_or_another_account_ends_the_seat_and_sends_nothing | The moved Codex differed in both id and email; either check could be deleted. | 41b, 41c |
| codex::mover::the_default_home_comes_first_and_each_home_once | Asked `dirs::home_dir()`; no case without a default home. | 59e |
| codex::appserver::the_client_reads_limits_and_the_account_and_a_login_moves_the_server | The fake accepted `chatgptAuthTokens` from any connection; the experimental capability could be dropped. The fake now refuses without it, which every mover/keep/daemon test inherits. | 60f |
| providers::kimi::read_sends_bearer_and_survives_a_failed_me | The 401 wording that names the cause was unasserted. | 74c |
| providers::antigravity::a_poll_refreshes_caches_and_reads_the_summary | A cached token was never read near its expiry: the margin and the expiry check itself could go. | 89, 89a |
| providers::antigravity::the_user_agent_is_a_hub_client_new_enough_for_the_gemini_group | Asserted only ">= 2.8" on a value read from /Applications on the machine running the test. Now the choice is tested directly: pinned unless a newer app is installed. | ua1, ua2 |
| providers::antigravity::seat::a_write_the_keychain_does_not_keep_puts_the_old_login_back | The fake `security` dropped the write, leaving the old value in place by itself: the rollback was never needed. It now stores another value. | 102 |
| providers::antigravity::seat::a_dead_grant_or_another_service_is_refused_before_any_keychain_call | No case for a grant that names no account under a name that is not an email. | 107e |
| providers::antigravity::seat::the_slot_matches_an_account_by_grant_or_by_email | (extended) `Slot`'s Debug shows no refresh token. | d5 |
| providers::antigravity::grant::the_app_user_status_names_the_account | No value in field 7 that is not an address. | 95b |
| providers::antigravity::grant::the_agy_login_parses_with_and_without_the_go_keyring_prefix | A login with a refresh token and no account name was untested (it must be an error, not an account named ""). | 95c |
| providers::antigravity::run::a_second_run_keeps_the_file_agy_refreshed_and_follows_the_real_home | "Only links tokeman made are removed" had no link that tokeman did not make. | 121 |

### Added

| test | rule it pins | mutation seen to fail it |
|---|---|---|
| codex::probe::only_a_403_that_looks_like_the_edge_is_a_challenge | A backend 403 (JSON) and a gateway's HTML 502 are answers, not challenges: one request, `Http`. `cf-mitigated` alone or an HTML 403 alone is a challenge. | 14, 14b, 14c |
| codex::probe::a_window_past_its_limit_reads_over_one_and_an_answer_without_windows_offers_no_room | Percent over 100 is kept and offers no room; `rate_limit: null`, a window without `used_percent` and an HTTP-200 error envelope yield no window, are no landing, claim no `allowed`, and the envelope's key is reported unrecognised. | p1, p2, p3, p4 |
| codex::refresh::a_refresh_lands_the_rotated_bundle_and_keeps_what_the_answer_left_out | The single-use refresh token: the new bundle is on disk; an answer omitting the refresh token keeps the old one (as Codex does); an answer without an access token writes nothing; the account id survives. | 33, 33b, 34 |
| codex::mover::a_refused_seat_moves_with_room_in_its_windows_and_one_without_windows_is_left | Refusal moves a seat whatever its windows say; a seat that reports no window is not judged and nothing is probed. | 59, 59b |
| codex::mover::a_seat_is_matched_by_account_id_before_email | One login in two workspaces is two accounts; email decides only without an id. | 59c |
| codex::mover::a_codex_with_nowhere_to_go_drains_and_no_reset_credit_is_spent | Nothing spends a reset by itself, at the mover's drain decision and in the half-hour sweep: a landing that is limited with an applicable credit draws no `/credits/consume`. Behavioural, not structural. | c1 (a claim inserted at `Relocation::Drain`) |
| codex::auth::debug_output_of_a_bundle_or_an_auth_file_shows_no_token | No credential in any printed form. | d3, d4 |
| providers::debug_output_names_which_secrets_are_held_and_shows_none | Same, for a service account's key and Google tokens. | d1, d2 |
| providers::zai::success_needs_both_the_flag_and_code_200 | z.ai's HTTP-200 envelope: `success: true` with another code, and code 200 with `success: false`, are refusals; an answer of only unknown limit types is a decode error, not an empty account. | 61, 61b, 67b |
| providers::zai::used_comes_from_remaining_when_the_count_is_missing_and_may_pass_the_limit | `used = usage - remaining` when `currentValue` is absent; over the limit reads over 1.0 and has no room. | 62, 62b |
| providers::zai::an_http_failure_is_reported_with_its_status | A non-2xx answer is `Http` with its status, one line. | 67c |
| providers::antigravity::a_token_rejected_even_after_a_refresh_is_an_error_after_one_refresh | A 401 on a freshly minted token ends the read (bounded): one refresh, `Http 401`. Under the mutation the read loops; the test fails at a 20 s timeout. | 86 |
| providers::antigravity::quota_is_read_without_the_plan_when_load_code_assist_is_down | The plan is decoration: quota is read on the consumer project when `loadCodeAssist` answers 500. | 87, 89b |

### Bugs

1. **HAZARD, found through this group's code, in g4's tests (reported to the root at once; closed by d5b0bbb + fdb38d7 + g4's change).**
   `daemon::codex::tests` (three tests) ran `mover::homes(&fleet)`, whose first entry was `dirs::home_dir()/.codex`,
   then `mover::sweep(.., dry_run = false)`. With a Codex running on the real ~/.codex with a control socket,
   `cargo test` read its limits and, had it been below 20% or refused, would have sent `account/login/start` with a
   test profile's token (`access-b`) to it. Evidence: a `#[cfg(test)] panic!` in `CodexHome::default_home`,
   `codex::Client::new` and `Seats::default` failed exactly those three tests and none under `codex::` or
   `providers::`. Fixed here in `fdb38d7`: `default_home` asks `paths::home()` (contained in tests), and
   `mover::homes_after(default, fleet)` is the seam for g4.
2. **Latent credential leak: derived `Debug` on `ServiceAccount`, `TokenBundle`, `AuthFile`, agy `Slot`,
   `Backup`, `Prepared`, `CachedToken`.** Nothing printed them (grep of every `{:?}` outside tests), but one
   `{account:?}` or `{fleet:?}` would have printed an API key or a refresh token, against "no credential is printed
   or logged, in any form". Fixed in `2e66ea2` (redacting `Debug`, or none), with tests.
3. **Minor, production: `agy use` built a new HTTP client for every loopback RPC**, loading the system trust store
   each time (measured through the tests: 3.1 s → 1.3 s for six calls). Fixed in `536643a` (one client, no idle pool).
4. **Latent, not fixed: `seat::ClientReport` serializes two `account` keys** for `Told` and `HoldsOld` (the struct's
   own field and the flattened variant's). `the_hub_is_told_...` asserts it with a `json!` literal that itself has
   the key twice, so the literal keeps only the last and the assertion says less than it looks. `UseReport` is
   printed as lines only (no `--json` for `agy use`), so no user sees it today. Left alone: renaming the field is
   an output-shape decision.
5. **Design gap against the research note, not fixed:** research/zai-kimi.md says to fall back to Kimi's legacy
   `usage`/`limits[]` when `usages` is absent; `kimi.rs` reads only `usages` and a test pins legacy-only as an
   error. Live answers carry both today. If Kimi drops `usages` for some plan the account reads as an error
   (visible, and `doctor` would say so), not as wrong numbers.
6. **Display only, not fixed:** a Codex usage answer with no `rate_limit` (including an HTTP-200 error envelope)
   has `State::Ok` and is shown as "ok" with no windows. It is never a landing (`has_quota` is false; pinned now)
   and its keys land in `unrecognized`, but the word is wrong. The fix belongs with `State::of`/`badge`.

### Not tested and matters

| what | why it matters | why not done |
|---|---|---|
| `providers::read_all`'s timeout branch (a read that hangs past 2 x READ_TIMEOUT becomes a `Network` error) | "every wait bounded" for the services poll | The bound is a 30 s constant; needs a parameter. `read_all_keeps_order...` also relies on missing key files to keep three reads off the real api.z.ai / api.kimi.com: if the key check regressed, the test would go to the network. Left as class b. |
| `codex::probe::probe_all` sending the first probe alone before the rest | The cookie-jar rule in DESIGN ("one jar", challenge settles first) | No clean single mutation; the challenge test covers the jar being shared, not the ordering. |
| A Codex refresh whose answer is lost after the server rotated the token | The single-use token is then gone; nothing can recover it | Not testable as behaviour: there is nothing to assert but the loss. The disk-before-return property is pinned by the new refresh test. |
| No lock around `codex::refresh::refresh_home` (two tokeman processes, or tokeman and Codex, refreshing one profile at once) | One of them gets `refresh_token_reused`, which is terminal until `codex login`. The module doc accepts the race ("another refresher is racing us") and the sweep reports it once | It is a design decision (Codex itself takes no lock tokeman could share), not a missing test. Stated here because the Claude side has a locked read-modify-write and this side does not. |
| `CodexHome::default_home()` being `~/.codex` (mutation h1 survives) | Codex's own fallback home | Would be a constant assertion. |
| Antigravity: a partly spent window whose reset happens to equal now + span (mutation 84b) | Would be shown as idle with no reset | Cannot be told apart from an idle window in the server's answer; the research note says the same. |
| `agy run` / `agy use` / `discover` on the live path (`Place::live`, `Lookout::live`, the real Keychain) | These are the functions that touch ember's login | By design no unit test reaches them (verified with the panic patch). Covered only by the lane's manual run recorded in research/antigravity.md. |
| `TOKEMAN_CODEX_DEBUG` prints every response header of a failed Codex request, `set-cookie` (the edge's `__cf_bm`) included, through `notice` | A cookie in the terminal; not a credential, opt-in | Not changed; noted. |
| Kimi 403/404 wording, `providers::read` for a Claude/Codex account, z.ai team headers | Low | Low. |

### Deferred (audit-only files)

None left: mover.rs and appserver.rs were released by the root after the Codex follow-up merged, and are covered above together with the new seats.rs and keep.rs.

### Cross-group

- g4: `daemon/codex.rs` must give its mover task its homes (`mover::homes_after`, pub(crate) since `fdb38d7`) and must not build `codex::Client::new()` / `Terms::of` (= `Seats::default()`) on a path a test overrides. See Bugs 1.
- g4/g1: `State::of` for a reading with no windows (Bugs 6).
- g2: `paths.rs` now owns the home directory; `grep -rn "dirs::" src/codex src/providers` is empty.

### Hygiene notes

- **Real locations.** Method: `#[cfg(test)] panic!("REALPATH")` in `CodexHome::default_home`, `codex::Client::new`, `Seats::default`, antigravity `Endpoints::live`, `installed_app_version`, `grant::app_state_db`, `run::Place::live`, `seat::Place::live`, `Lookout::live`, `security_bin`, `token_file`, `require_macos`, `providers::client`; ran `codex::` and `providers::`; reverted. Every test under `codex::` and `providers::` passed; the only failures were g4's three (Bugs 1).
- Fixed: `providers::antigravity::the_user_agent_...` and every loopback poll test read `/Applications/Antigravity.app/Contents/Info.plist` through `user_agent()`. The agent string is now a field of `Endpoints`, built by `agent_for(installed)`; tests pass `agent_for(None)`.
- Fixed: `codex::mover::the_default_home_comes_first_...` asked `dirs::home_dir()`; it uses `homes_after` with a fixed path.
- The three tests the root listed (`mover::a_seat_below_its_floor_...`, two `seat` tests) only formatted a path with `paths::tilde`; the root's test home in paths.rs makes that deterministic. No per-test change.
- **Sleeps.** `codex::probe::a_standing_challenge_is_not_an_auth_failure` 2.7 s → 0.23 s and `codex::maintenance::a_challenged_probe_does_not_force_a_refresh` 2.6 s → 0.28 s: the challenge backoff is `Client::with_challenge_backoff` (400 ms in production, 1 ms in the fake backend). `seat::the_hub_is_told_...` 3.1 s → 1.3 s, `seat::a_hub_that_cannot_be_told_...` 2.5 s → 1.0 s, `clients::a_hub_needs_its_token_...` 1.8 s → 0.3 s: one loopback RPC client instead of one per call.
- Left: `seat::a_security_that_hangs_is_killed_at_the_bound` and `clients::a_census_that_cannot_be_taken_...` wait 2.0 s each. The bound is the thing under test and is already a `cfg!(test)` constant (2 s instead of 10 s); every other test in those files runs a shell script as the fake `security`/`ps` under the same bound, so a shorter one risks flakes on a loaded box. The remaining ~1 s in the hub tests is the fake `security`/`ps`/`lsof` shell scripts and one `reqwest::Client::new()` per test.
- Wall clock: `auth::bundle()` and the maintenance `age()` helper use `Utc::now()` with margins of days; `seat::seating_writes_...` checks an expiry within a 100 s band of now. No time-zone or ordering dependence found. Fake app-server homes are `tempfile` directories under `/tmp` (Unix socket path length), uniquely named and removed on drop.
- Flake check: `codex::` + `providers::` three times in a row, and `providers::antigravity` five times after the shared RPC client: all green.
- I created and removed `<scratchpad>/g3m` (a copy of my clone used only for mutations). `<scratchpad>/g3-tools/` holds the mutation scripts and their raw output (`batch1.out`, `batch2.out`, `batch3.out`).

---

## g4: commands/, daemon/, ui/ (overview, text, chart, tui), cli.rs, main.rs, drift.rs

Branch `lane/tests-g4` in `<scratchpad>/g4`. Blunt summary: the tests of this group were mostly real (85 of 102
pinned a rule, and 100 of 119 first-pass mutations were killed), but four things that matter had NO test and a
mutation of each survived: the viewer decision (a surface reads nothing while the daemon reads), "a standing item is
notified once" (a ledger bug would have re-notified every six hours), "nothing spends a reset by itself", and the
status of an account that cannot take work. Two real bugs were found and fixed. The rotation cycle itself
(`daemon::cycle::run`) still has no test of its control flow; see "Not tested".

### Counts

Before = at 0248920 plus the two tests the Codex follow-up added to `daemon/codex.rs` (102). `ui/tray.rs` (3 tests,
feature-gated) was deleted by lane `compact` and is not counted.

| file | before | a | b | c | d | deleted | strengthened | added | after |
|---|---|---|---|---|---|---|---|---|---|
| cli.rs | 4 | 3 | 1 | 0 | 0 | 0 | 1 | 1 | 5 |
| drift.rs | 6 | 5 | 1 | 0 | 0 | 0 | 1 | 0 | 6 |
| daemon/cadence.rs | 8 | 8 | 0 | 0 | 0 | 0 | 1 | 0 | 8 |
| daemon/codex.rs | 5 | 5 | 0 | 0 | 0 | 0 | 2 | 1 | 6 |
| daemon/cycle.rs | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 2 | 3 |
| daemon/lock.rs | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| daemon/mod.rs | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 |
| daemon/notify.rs | 7 | 7 | 0 | 0 | 0 | 0 | 1 | 0 | 7 |
| daemon/refresh.rs | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| daemon/service.rs | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| daemon/services.rs | 2 | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 2 |
| commands/claude.rs | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| commands/codex.rs | 7 | 6 | 0 | 1 | 0 | 1 | 1 | 0 | 6 |
| commands/doctor.rs | 12 | 11 | 1 | 0 | 0 | 0 | 1 | 0 | 12 |
| commands/launch.rs | 2 | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 2 |
| commands/view.rs | 3 | 3 | 0 | 0 | 0 | 0 | 1 | 1 | 4 |
| commands/rotate.rs, services.rs, mod.rs | 0 | | | | | | | | 0 |
| ui/chart.rs | 8 | 4 | 4 | 0 | 0 | 0 | 0 | 1 | 9 |
| ui/overview.rs | 7 | 7 | 0 | 0 | 0 | 0 | 0 | 2 | 9 |
| ui/text.rs | 20 | 14 | 3 | 0 | 3 | 2 | 4 | 2 | 20 |
| ui/tui.rs | 6 | 3 | 3 | 0 | 0 | 0 | 0 | 0 | 6 |
| **total** | **102** | **85** | **13** | **1** | **3** | **3** | **13** | **11** | **110** |

After: a 96, b 14 (the 13 kept as they are plus `truncation_counts_characters_not_bytes`), c 0, d 0.

### Inventory

Classes: a = pins behaviour that matters; b = meaningful but weak; c = hollow; d = redundant. "Action" is what this
lane did. Tests added by the Codex follow-up merge (7f69918) are marked (new).

| file | test | claim | class | action |
|---|---|---|---|---|
| cli.rs | the_changelog_table_is_the_removed_table | CHANGELOG rows equal `REMOVED`, both directions | a | kept |
| cli.rs | the_service_first_tree_parses | 70 command lines parse | b (is_ok only; no field checked) | kept as the tree smoke; reset flags pinned by a new test |
| cli.rs | what_an_installed_0_3_service_and_hook_invoke_still_parses | the launchd and hook invocations of 0.3 parse to the same actions | a | kept |
| cli.rs | a_removed_spelling_does_not_parse_and_names_its_replacement | old spellings are refused by clap and name the replacement | a | strengthened: now walks every `REMOVED` row |
| drift.rs | keys_values_and_rows_outside_the_known_sets_are_collected_as_paths | unknown keys/words become paths, content is not kept | a | kept |
| drift.rs | an_item_is_one_bounded_token_whatever_the_server_put_in_it | an item is one token of at most 64 chars | a | kept |
| drift.rs | merging_keeps_a_list_sorted_and_unique | merge sorts and dedups | b (small helper) | kept |
| drift.rs | the_ledger_reports_an_item_as_new_once_and_keeps_where_it_was_first_seen | new once, per service, first sighting kept | a | strengthened: file mode 0600 |
| drift.rs | each_new_item_of_a_read_is_one_log_line_and_a_repeat_is_silent | one log line per new item | a | kept |
| drift.rs | what_another_build_recorded_is_not_read_and_is_dropped_at_the_next_write | another version's ledger reads empty | a | kept |
| daemon/cadence.rs | backoff_schedule_climbs_then_caps_at_thirty_minutes | 5m,10m,20m,30m cap | a (the schedule is the rule) | kept |
| daemon/cadence.rs | refusals_park_and_successes_clear | a refusal parks, restates, clears on success | a | kept |
| daemon/cadence.rs | a_changed_credential_ends_the_backoff_early | a new credential is asked at once | a | kept |
| daemon/cadence.rs | transient_failures_never_park | a timeout is not a refusal | a | kept |
| daemon/cadence.rs | state_forgets_accounts_that_left_the_fleet | removed accounts leave the state | a | kept |
| daemon/cadence.rs | samples_are_recorded_then_carried_until_reset | Fable sample carried until its reset; unanswered said once | a, one hole | strengthened: a limit the probe read is not replaced by a carried one |
| daemon/cadence.rs | the_state_file_keeps_its_spelling | on-disk key names | a | kept |
| daemon/cadence.rs | todays_state_file_loads_and_an_entry_it_cannot_read_is_dropped_alone | the real 2026-11-11 file loads; a bad entry is dropped alone | a | kept |
| daemon/codex.rs | the_daemon_looks_only_with_the_setting_on_unpaused_and_somewhere_to_move | Standing from setting, pause, profiles | a, one hole | strengthened: paused wins over off |
| daemon/codex.rs | a_low_codex_is_moved_in_the_background_recorded_and_logged_once | the daemon moves a low Codex, records, logs once | a, one vacuous assert | strengthened: vacuous assert replaced; no credit is listed or consumed |
| daemon/codex.rs | a_pushed_seat_is_renewed_with_moving_off_and_left_alone_while_paused (new) | tending runs with moving off, not while paused | a | kept |
| daemon/codex.rs | a_refused_renewal_reaches_the_fleet_view_as_an_attention_item_with_its_fix (new) | a refused renewal becomes one attention item with its fix | a | kept |
| daemon/codex.rs | only_changes_are_logged_and_quiet_states_only_after_trouble | log lines only on change | a | kept |
| daemon/cycle.rs | what_a_sample_did_not_recognise_joins_its_accounts_read | sample drift merges into the account's read | a | kept |
| daemon/lock.rs | the_daemon_lock_is_held_for_its_lifetime_and_admits_one_daemon | lock held while alive, check creates nothing | a | kept |
| daemon/notify.rs | a_key_is_told_once_and_again_at_its_next_threshold | once per key, again at the next threshold | a, one hole | strengthened: an item standing for days is told once |
| daemon/notify.rs | an_item_that_went_away_is_told_again_only_after_the_rearm_time | re-arm after 6 h absent | a | kept |
| daemon/notify.rs | the_ledger_survives_a_restart_and_drops_only_entries_it_cannot_read | ledger round trip, 0600, lenient per entry | a | kept |
| daemon/notify.rs | a_few_items_are_told_one_by_one_and_many_as_one | up to 3 separately, more as one | a | kept |
| daemon/notify.rs | with_notifications_off_an_item_is_logged_once_and_remembered | off: logged once; unwritable ledger sends nothing | a | kept |
| daemon/notify.rs | a_notifier_is_bounded_and_its_failure_is_reported | notifier bounded by timeout | a | kept |
| daemon/notify.rs | the_daemon_never_waits_on_a_round | tick does not await a running round | a | kept |
| daemon/refresh.rs | transient_refresh_failures_back_off_a_minute_then_double_to_ten | refresh retry schedule | a | kept |
| daemon/service.rs | the_plist_escapes_paths_and_runs_at_standard_priority | plist escaping, Standard priority | a | kept |
| daemon/services.rs | a_poll_runs_in_the_background_and_is_recorded_once_finished | poll in a task, recorded, next waits | a | kept |
| daemon/services.rs | only_changes_in_an_accounts_error_are_logged | error transitions logged once | a | hygiene: no default path |
| commands/claude.rs | login_walks_the_named_account_or_those_without_a_login | which accounts `login` walks | a | kept |
| commands/codex.rs | survey_marks_registered_roots_and_names_the_default_profile | discovery marks registered homes | a | kept |
| commands/codex.rs | import_lines_are_parsed_strictly_and_seed_access_only_profiles | import parsing, access-only seeding | a | hygiene: example.com |
| commands/codex.rs | the_detailed_list_reads_credits_only_where_the_usage_body_counts_some | credit list read only where credits exist | a | kept |
| commands/codex.rs | watch_once_moves_a_low_codex_and_a_dry_run_only_says_so | watch moves a low Codex; dry run sends nothing | a | strengthened: no credit listed or consumed |
| commands/codex.rs | watch_with_one_profile_says_there_is_nowhere_to_move | one unparked profile: nowhere to move | a | kept |
| commands/codex.rs | use_announces_the_named_account_to_the_running_codex | `codex use` pushes the named account | a | kept |
| commands/codex.rs | report_rows_and_warnings_read_plainly | two Display strings | c (format strings of two values, no rule) | deleted |
| commands/doctor.rs | nothing_wrong_is_healthy_and_states_no_problem | baseline: no problem | b (control for the others) | kept |
| commands/doctor.rs | a_read_error_says_whether_the_service_answered_and_what_became_of_the_credential | error -> reach/credential table | a | kept |
| commands/doctor.rs | each_failing_account_is_a_problem_with_the_command_for_its_service | per-service fix command | a | kept |
| commands/doctor.rs | a_refused_login_is_a_problem_even_when_the_setup_key_reads | refused login is a problem | a | kept |
| commands/doctor.rs | unrecognised_items_are_one_problem_per_service_with_what_the_daemon_saw | drift: one problem per service, daemon items merged | a | kept |
| commands/doctor.rs | a_tool_is_compared_with_its_verified_version_and_never_counts_as_a_problem | version compare; not a problem | a | kept |
| commands/doctor.rs | the_hook_is_found_by_its_command_and_a_missing_binary_is_dangling | hook states | a | kept |
| commands/doctor.rs | a_missing_hook_is_a_problem_only_where_the_service_is_installed | missing hook only with a service | a, one hole | strengthened: every state of an installed service |
| commands/doctor.rs | a_stopped_service_is_a_problem_and_a_paused_one_is_not | stopped is a problem, paused is not | a | kept |
| commands/doctor.rs | a_file_others_can_read_is_a_problem_with_its_chmod | mode check and chmod fix | a | kept |
| commands/doctor.rs | an_unreadable_agy_login_and_an_unanswered_claim_are_problems | agy slot, pending claims, unreadable journal | a | kept |
| commands/doctor.rs | the_report_serializes_with_fixed_words_for_every_state | JSON words of the report | a (the Swift app and scripts read it) | kept |
| commands/launch.rs | codex_starts_on_the_winning_profiles_home | CODEX_HOME of the winner, no secret | a | kept |
| commands/launch.rs | a_vendor_launch_carries_the_endpoint_key_and_models_and_drops_anthropic_credentials | vendor env: key secret, competing vars unset | a | kept |
| commands/view.rs | best_skips_parked_and_unviable_and_ranks_buckets_separately | codex best: parked, refused, spent, limit scope | a, one hole | strengthened: a refused account is no last resort |
| commands/view.rs | a_weekly_only_account_is_ranked_on_its_weekly_window | weekly-only real shape ranks on weekly | a | kept |
| commands/view.rs | every_service_ranks_through_the_one_function | group ranking | a | kept |
| ui/chart.rs | ellipsize_respects_character_count | ellipsize helper | b | kept |
| ui/chart.rs | coordinate_mapping_clamps | map_x/map_y clamp | b | kept |
| ui/chart.rs | a_metric_names_a_window_by_length_or_a_limit_by_what_it_covers | --metric resolver | a (its default case did not see bug 1) | kept; new test beside it |
| ui/chart.rs | only_the_rotation_windows_have_floors | floors only on Claude's account-wide windows | a | kept |
| ui/chart.rs | a_chart_of_another_service_draws | a PNG is produced; missing series errors | b (PNG magic only) | kept |
| ui/chart.rs | a_held_value_is_a_step_and_a_silence_is_a_gap | step drawing under the hold rule | a | kept |
| ui/chart.rs | segments_break_on_gaps_resets_and_refills | segment breaks | a | kept |
| ui/chart.rs | durations_read_as_days_hours_or_minutes | duration words | b | kept |
| ui/overview.rs | a_live_daemon_keeps_views_fresh_however_it_was_started | Monitor from alive/paused/installed; keeps_fresh | a | kept |
| ui/overview.rs | every_account_of_every_service_is_one_kind_of_entry | the fleet view and its JSON | a | kept |
| ui/overview.rs | a_login_near_its_end_warns_and_an_unread_account_has_no_reading | login view; unread account | a | kept |
| ui/overview.rs | fresh_readings_replace_the_stored_ones_and_the_rest_stay | with_fresh | a | kept |
| ui/overview.rs | reset_offers_and_rejections_on_record_ride_along | offers, observed rejections, attention | a | kept |
| ui/overview.rs | every_limit_carries_the_name_and_note_the_surfaces_show | LimitView name/note | a (one loop restates text::limit_name; explicit values follow) | kept |
| ui/overview.rs | a_service_with_a_measured_rate_has_a_forecast_and_the_rest_have_none | forecast wiring, refills apart | a | kept |
| ui/text.rs | a_launch_line_names_a_credential_and_never_shows_it | launch line never prints a secret | a | kept |
| ui/text.rs | an_error_with_a_json_body_is_shown_by_its_message | brief_error | d (with an_api_error_body_...) | kept, now holding both tests' cases |
| ui/text.rs | text_helpers_count_characters_not_bytes | one_line / truncate_chars | d (one_line is util's, tested there) | deleted; truncate_chars case moved |
| ui/text.rs | a_usage_reads_as_one_line_of_room_left | summary line; over-100% reads 0% | a | kept |
| ui/text.rs | the_login_note_is_the_refusal_or_the_end_date_alone | login note | a | kept |
| ui/text.rs | resets_in_the_past_or_absent_read_as_words | reset 0 = "--", past = "now" | b | kept |
| ui/text.rs | signals_are_described_but_quiet_when_absent | signal lines | b | kept |
| ui/text.rs | a_shared_limit_says_how_much_of_the_weekly_it_still_allows | share note uses Limit::room | a | kept |
| ui/text.rs | the_login_line_uses_the_reported_refresh_expiry_before_the_age | login line: reported expiry, then age | a | kept |
| ui/text.rs | a_limit_is_named_by_what_it_covers_and_how_long_it_is | limit_name | a | kept |
| ui/text.rs | every_service_is_drawn_by_the_one_card | the text card for every service | a | kept |
| ui/text.rs | a_codex_reading_on_one_line_names_its_windows_credits_and_trouble | codex summary | a | kept |
| ui/text.rs | an_api_error_body_is_shown_as_its_message | brief_error | d | deleted; its cases moved into the test above it |
| ui/text.rs | what_needs_the_user_is_a_sentence_and_the_command_that_deals_with_it | attention text and fix | a | strengthened: the paused pushed seat |
| ui/text.rs | a_forecast_says_its_outlook_its_basis_and_what_resets_would_add | forecast sentence | a | kept |
| ui/text.rs | the_reports_open_with_what_needs_attention_and_the_forecasts | headlines | a | strengthened: an idle service has no line |
| ui/text.rs | a_failed_credit_list_is_said_under_the_account | credit list failure line | b | kept |
| ui/text.rs | other_windows_unavailable_models_and_spend_caps_are_said | codex notes | a | kept |
| ui/text.rs | the_caption_says_where_the_view_comes_from_and_how_old_it_is | stored/fresh/stale caption | a | strengthened: the ten-minute boundary |
| ui/text.rs | the_doctor_report_lists_what_was_checked_then_each_problem_with_its_fix | doctor text | a | kept |
| ui/tui.rs | a_card_has_a_gauge_per_limit_whatever_the_service | TUI card | a | kept |
| ui/tui.rs | a_card_without_limits_says_why | error / unread / empty card | a | kept |
| ui/tui.rs | the_header_says_stored_or_read_and_only_a_reader_counts_down | viewer vs reader in the TUI | a | kept |
| ui/tui.rs | tab_steps_through_the_limits_the_history_holds | series cycling | b | kept |
| ui/tui.rs | the_selected_card_stays_visible | scroll window | b | kept |
| ui/tui.rs | chart_segments_count_hours_back_from_now | hours back from now | b | kept |

### Mutations

One mutation at a time, the named tests run filtered, the file restored with `git checkout` after each. 138 mutations: 119 killed, 19 survived. Ids ending in `r` re-run a survivor against the strengthened test; `N`, `K`, `T`, `R`, `C`, `H` ids are the mutations each added or strengthened test was seen to fail under.

| id | file | changed | to | tests run | outcome | follow-up |
|---|---|---|---|---|---|---|
| M01 | cli.rs | `"tokeman claude login [<account>] [--more]"` | `"tokeman claude login [<account>]"` | cli::the_changelog_table_is_the_removed_table | killed |  |
| M02 | cli.rs | `.skip_while(\|arg\| arg.starts_with('-'))` | `.skip_while(\|_\| false)` | cli::a_removed_spelling | killed |  |
| M03 | cli.rs | `words.len() >= old.len() && words[..old.len()] == **old` | `words.first() == old.first()` | cli::a_removed_spelling | killed |  |
| M03b | cli.rs | `(&["resets"], "tokeman claude reset [<account>]"),` | `(removed)` | cli:: | killed |  |
| M05 | drift.rs | `c.is_ascii_graphic() && c != '='` | `c.is_ascii_graphic()` | drift::an_item_is_one_bounded | killed |  |
| M06 | drift.rs | `if !known.contains(&key.as_str()) {` | `if known.contains(&key.as_str()) {` | drift::keys_values | killed |  |
| M07 | drift.rs | `.filter(\|file\| file.build == Self::BUILD)` | `(removed)` | drift::what_another_build | killed |  |
| M08 | drift.rs | `.filter(\|item\| !known.contains_key(*item))` | `(removed)` | drift::the_ledger_reports | killed |  |
| M09 | drift.rs | `write_atomic(&self.path, &bytes, 0o600)?;` | `write_atomic(&self.path, &bytes, 0o644)?;` | drift:: | **SURVIVED** | strengthened the ledger test with the mode; re-run M09r killed |
| M09b | drift.rs | `.take(MAX_WORD)` | `.take(MAX_WORD * 100)` | drift::an_item_is_one_bounded | killed |  |
| M10 | daemon/cadence.rs | `&& entry.credential == account.fingerprint((now * 1000.0) as i64)` | `(removed)` | daemon::cadence::a_changed_credential | killed |  |
| M11 | daemon/cadence.rs | `if reading.is_refused() {` | `if reading.error.is_some() {` | daemon::cadence::transient | killed |  |
| M12 | daemon/cadence.rs | `.filter(\|window\| window.resets_at == 0 \|\| window.resets_at as f64 …` | `;` | daemon::cadence::samples_are | killed |  |
| M13 | daemon/cadence.rs | `&& account.profile_token((now * 1000.0) as i64).is_none()` | `(removed)` | daemon::cadence::samples_are | killed |  |
| M14 | daemon/cadence.rs | `status: 401, body: format!("authentication previously` | `status: 500, body: format!("authentication previously` | daemon::cadence::refusals_park | killed |  |
| M15 | daemon/cadence.rs | `.is_none_or(\|previous\| previous.answered)` | `.is_none_or(\|_\| true)` | daemon::cadence::samples_are | killed |  |
| M15b | daemon/cadence.rs | `entry.next_attempt_epoch = now + probe_backoff_secs(entry.failures);` | `entry.next_attempt_epoch = now;` | daemon::cadence:: | killed |  |
| M15c | daemon/cadence.rs | `if reading.model_limit(&fable).is_some() { continue; }` | `(removed)` | daemon::cadence:: | **SURVIVED** | strengthened samples_are_recorded...; re-run M15cr killed |
| M16 | daemon/cycle.rs | `.find(\|reading\| reading.account == sample.account)` | `.find(\|reading\| reading.account != sample.account)` | daemon::cycle:: | killed |  |
| M17 | daemon/lock.rs | `for attempt in 0..Self::ATTEMPTS {` | `for attempt in 0..0 {` | daemon::lock:: | killed |  |
| M18 | daemon/notify.rs | `Some(told) => told.seen_at = now,` | `Some(told) => { told.seen_at = now; due.push(item); }` | daemon::notify::a_key_is_told_once | killed |  |
| M19 | daemon/notify.rs | `self.told.retain(\|_, told\| now - told.seen_at < REARM_SECS);` | `self.told.retain(\|_, told\| now - told.notified_at < REARM_SECS);` | daemon::notify::a_key daemon::notify::an_item | **SURVIVED** | an item standing for days was re-notified every 6 h under this mutation and no test saw it; strengthened a_key_is_told_once...; re-run M19r killed |
| M20 | daemon/notify.rs | `if told != before && let Err(error) = told.save(ledger)` | `if false && let Err(error) = told.save(ledger)` | daemon::notify::with_notifications_off | killed |  |
| M21 | daemon/notify.rs | `const AT_ONCE: usize = 3;` | `const AT_ONCE: usize = 1;` | daemon::notify::a_few_items | killed |  |
| M21b | daemon/notify.rs | `Some(fix) => format!("{}. Run: {fix}", item.text),` | `Some(_) => item.text.clone(),` | daemon::notify:: | killed |  |
| M25 | daemon/refresh.rs | `.min(600.0);` | `.min(60.0);` | daemon::refresh:: | killed |  |
| M25b | daemon/refresh.rs | `entry.0.min(4) as i32 - 1` | `entry.0.min(9) as i32 - 1` | daemon::refresh:: | killed |  |
| M26 | daemon/service.rs | `<string>Standard</string>` | `<string>Background</string>` | daemon::service:: | killed |  |
| M27 | daemon/service.rs | `.replace('<', "&lt;")` | `(removed)` | daemon::service:: | killed |  |
| M28 | daemon/services.rs | `if self.failing.get(&key) != Some(error) {` | `if true {` | daemon::services::only_changes | killed |  |
| M29 | daemon/services.rs | `self.next = Some(Instant::now() + fleet.launch.services_interval());` | `self.next = Some(Instant::now());` | daemon::services::a_poll_runs | killed |  |
| M30 | daemon/codex.rs | `if paused { Self::Paused } else if !fleet.policy.codex_auto_move { Sel…` | `if !fleet.policy.codex_auto_move { Self::Off } else if paused { Self::…` | daemon::codex:: | **SURVIVED** | strengthened the_daemon_looks_only...; re-run M30r killed |
| M31 | daemon/codex.rs | `let tends = standing != Standing::Paused && !self.seats.load().is_empt…` | `let tends = !self.seats.load().is_empty();` | daemon::codex::a_pushed_seat | killed |  |
| M32 | daemon/codex.rs | `Outcome::Draining \| Outcome::Unreachable { .. } => before != Some(kin…` | `Outcome::Draining \| Outcome::Unreachable { .. } => true,` | daemon::codex::only_changes | killed |  |
| M33 | daemon/codex.rs | `look.moved(By::Daemon, chrono::Utc::now().timestamp())` | `look.moved(By::Watch, chrono::Utc::now().timestamp())` | daemon::codex::a_low_codex | killed |  |
| M34 | daemon/codex.rs | `Tended::Troubled(_) => before != Some(kind),` | `Tended::Troubled(_) => true,` | daemon::codex::a_refused_renewal | killed |  |
| M34b | daemon/codex.rs | `let moves = standing == Standing::On;` | `let moves = standing != Standing::Paused;` | daemon::codex:: | killed |  |
| M35 | commands/claude.rs | `let needs_login = \|account: &&Account\| !account.is_refreshable();` | `let needs_login = \|_: &&Account\| true;` | commands::claude:: | killed |  |
| M36 | commands/codex.rs | `Some(min) if (0.0..1.0).contains(&min) => Ok(floors.leaving_at(min)),` | `Some(min) if (0.0..=2.0).contains(&min) => Ok(floors.leaving_at(min)),` | commands::codex::watch_with_one | killed |  |
| M37 | commands/codex.rs | `if unparked < 2 { on_event(WatchEvent::NowhereToMove { unparked });` | `if unparked < 1 { on_event(WatchEvent::NowhereToMove { unparked });` | commands::codex::watch_with_one | killed |  |
| M39 | commands/codex.rs | `.is_some_and(\|count\| count.available > 0);` | `.is_some_and(\|_\| true);` | commands::codex::the_detailed_list | killed |  |
| M40 | commands/codex.rs | `bail!("line {}: expected `<name> <access-token>`", index + 1);` | `continue;` | commands::codex::import_lines | killed |  |
| M41 | commands/codex.rs | `registered: fleet.codex.iter().any(\|account\| account.home() == *root…` | `registered: !fleet.codex.is_empty(),` | commands::codex::survey | killed |  |
| M41b | commands/codex.rs | `access_only: true,` | `access_only: false,` | commands::codex::import_lines | **SURVIVED** | NOT fixed: registration goes through Fleet::update on the live account file; see Not tested |
| M42 | commands/doctor.rs | `status: 401 \| 403, .. }) => (Reach::Answered, CredentialState::Refuse…` | `status: 401, .. }) => (Reach::Answered, CredentialState::Refused),` | commands::doctor::a_read_error | killed |  |
| M43 | commands/doctor.rs | `Some(mode) if mode & !expected != 0 => ModeState::TooOpen,` | `Some(mode) if mode != expected => ModeState::TooOpen,` | commands::doctor::a_file_others | killed |  |
| M44 | commands/doctor.rs | `Monitor::Running \| Monitor::Paused \| Monitor::Stopped` | `Monitor::Running \| Monitor::Paused` | commands::doctor:: | **SURVIVED** | strengthened a_missing_hook...; re-run M44r killed |
| M44b | commands/doctor.rs | `Monitor::Running \| Monitor::Paused \| Monitor::Stopped` | `Monitor::Paused \| Monitor::Stopped` | commands::doctor:: | **SURVIVED** | strengthened a_missing_hook...; re-run M44br killed |
| M45 | commands/doctor.rs | `Some(binary) if binary.contains('/') && !exists(Path::new(&binary)) =>…` | `Some(binary) if !exists(Path::new(&binary)) => {` | commands::doctor::the_hook_is_found | killed |  |
| M46 | commands/doctor.rs | `.filter(\|entry\| !fresh.contains(&(entry.service, entry.item.as_str()…` | `(removed)` | commands::doctor::unrecognised | killed |  |
| M47 | commands/doctor.rs | `format!("tokeman claude reset {} --grant {item}", pending.account)` | `format!("tokeman claude reset {} --credit {item}", pending.account)` | commands::doctor::an_unreadable | killed |  |
| M48 | commands/doctor.rs | `if let Some(error) = &account.login_error {` | `if let Some(error) = account.login_error.as_ref().filter(\|_\| account…` | commands::doctor:: | killed |  |
| M49 | commands/doctor.rs | `if findings.daemon == Monitor::Stopped {` | `if findings.daemon != Monitor::Running {` | commands::doctor:: | killed |  |
| M49b | commands/doctor.rs | `(None, _) => format!("tokeman {word} reset {}", pending.account),` | `(None, _) => format!("tokeman {word} reset {} --clear", pending.accoun…` | commands::doctor:: | killed |  |
| M49c | commands/doctor.rs | `healthy: problems.is_empty(),` | `healthy: true,` | commands::doctor:: | killed |  |
| M49d | commands/doctor.rs | `None \| Some(ReadError::Decode { .. }) => (Reach::Answered, Credential…` | `None => (Reach::Answered, CredentialState::Accepted), Some(ReadError::…` | commands::doctor:: | killed |  |
| M50 | commands/launch.rs | `for name in COMPETING { launch = launch.unset(name); }` | `(removed)` | commands::launch:: | killed |  |
| M51 | commands/launch.rs | `.filter(\|account\| account.service == best.service)` | `(removed)` | commands::launch:: | killed |  |
| M52 | commands/launch.rs | `.ranked .first()` | `.ranked .last()` | commands::launch:: | killed |  |
| M52b | commands/launch.rs | `.set_secret(endpoint.key_var, key);` | `.set(endpoint.key_var, key);` | commands::launch:: | killed |  |
| M53 | commands/view.rs | `.is_none_or(\|account\| !account.parked)` | `.is_none_or(\|_\| true)` | commands::view::best_skips | killed |  |
| M54 | commands/view.rs | `if limit.is_none() { order.extend(` | `if true { order.extend(` | commands::view::best_skips | killed |  |
| M55 | commands/view.rs | `RankOn::Limits(needle) => reading.scoped_to(needle),` | `RankOn::Limits(_) => Some(reading.clone()),` | commands::view::every_service | killed |  |
| M55b | commands/view.rs | `.filter(\|reading\| reading.accepts_work() && reading.limits.is_empty(…` | `.filter(\|reading\| reading.limits.is_empty())` | commands::view:: | **SURVIVED** | strengthened best_skips...; re-run M55br killed |
| M56 | commands/view.rs | `if !force && Monitor::now().keeps_fresh() {` | `if !force && !Monitor::now().keeps_fresh() {` | commands::view:: ui::overview:: | **SURVIVED** | the viewer decision had no test at all; added a_surface_reads_no_account_while_the_service_reads (N1a-d killed) |
| M57 | ui/overview.rs | `(true, true, _) => Self::Paused,` | `(true, true, _) => Self::Running,` | ui::overview::a_live_daemon | killed |  |
| M58 | ui/overview.rs | `self == Self::Running` | `self != Self::NotInstalled` | ui::overview::a_live_daemon | killed |  |
| M59 | ui/overview.rs | `view.held = facts.default_account.as_deref() == Some(account.name.as_s…` | `view.held = false;` | ui::overview::every_account | killed |  |
| M60 | ui/overview.rs | `self.origin = Origin::Fresh;` | `(removed)` | ui::overview::fresh_readings | killed |  |
| M61 | ui/overview.rs | `.filter(\|reading\| reading.error.is_none() && reading.verdict() != Ve…` | `(removed)` | ui::overview:: | **SURVIVED** | added an_account_that_cannot_take_work_is_not_room (N5a killed) |
| M62 | ui/overview.rs | `.into_iter() .filter(\|_\| wants(Service::Claude)) .collect(), default…` | `.into_iter() .collect(), default_account` | ui::overview::reset_offers | killed |  |
| M63 | ui/overview.rs | `&& reading .shared() .all(\|limit\| !limit.is_rejected() && limit.wind…` | `(removed)` | ui::overview:: | **SURVIVED** | same new test (N5b killed) |
| M65 | ui/overview.rs | `.filter(\|account\| account.service == service && !account.parked)` | `.filter(\|account\| account.service == service)` | ui::overview:: | **SURVIVED** | added a_parked_profile_is_not_counted_in_a_forecast (N5d killed) |
| M65b | ui/overview.rs | `Service::Claude => (model.as_ref(), fleet.policy.sip_min_7d_remaining)…` | `Service::Claude => (model.as_ref(), 0.0),` | ui::overview::a_service_with | killed |  |
| M65c | ui/overview.rs | `view.viable = reading.map(Reading::accepts_work);` | `view.viable = reading.map(\|_\| true);` | ui::overview:: | **SURVIVED** | same new test as M61 (N5c killed) |
| M65d | ui/overview.rs | `.filter_map(\|account\| account.reading.as_ref()) .map(\|reading\| rea…` | `.filter_map(\|account\| account.reading.as_ref()) .map(\|reading\| rea…` | ui::overview:: | killed |  |
| M65e | ui/overview.rs | `Some(stored) => *stored = reading,` | `Some(_) => {}` | ui::overview::fresh_readings | killed |  |
| H1 | codex/auth.rs | `let home = crate::paths::home(); (default_home)` | `a fixed home outside TEST_HOME` | daemon::codex::a_test_run_looks_at_no_codex_home_outside_its… | killed |  |
| M66 | ui/text.rs | `Monitor::Running if overview.at - read_at > STALE_SECS => {` | `Monitor::Running if false => {` | ui::text::the_caption | killed |  |
| M66b | ui/text.rs | `pub const STALE_SECS: i64 = 600;` | `pub const STALE_SECS: i64 = 6000;` | ui::text::the_caption ui::tui:: | **SURVIVED** | strengthened the_caption...; re-run M66br (off-by-one at the boundary) killed |
| M67 | ui/text.rs | `true => format!("{} set", var.name),` | `true => format!("{}={}", var.name, var.value.to_string_lossy()),` | ui::text::a_launch_line | killed |  |
| M68 | ui/text.rs | `Verdict::Rejected => ("REJECTED", Some(Level::Critical)),` | `Verdict::Rejected => ("allowed", Some(Level::Ok)),` | ui::text:: ui::overview:: ui::tui:: | **SURVIVED** | added a_claude_badge_is_the_verdict... (N6a killed) |
| M69 | ui/text.rs | `Scope::All => 0, Scope::Model(_) => 1, Scope::Group(_) => 2,` | `Scope::All => 0, Scope::Model(_) => 0, Scope::Group(_) => 0,` | ui::text::every_service ui::overview::every_limit | killed |  |
| M70 | ui/text.rs | `limit.room(weekly).max(0.0) * 100.0` | `limit.window.remaining().max(0.0) * 100.0` | ui::text::a_shared_limit | killed |  |
| M71 | ui/text.rs | `if login.error.is_some() { return Some(format!( "login refused: run` | `if false { return Some(format!( "login refused: run` | ui::text::the_login_note | killed |  |
| M72 | ui/text.rs | `Level::Critical => RED, _ => YELLOW,` | `Level::Critical => YELLOW, _ => YELLOW,` | ui::text::the_reports_open | **SURVIVED** | left: a colour code; tests compare text without colours |
| M73 | ui/text.rs | `Outlook::Idle => continue,` | `Outlook::Idle => DIM,` | ui::text::the_reports_open ui::text::a_forecast | **SURVIVED** | strengthened the_reports_open...; re-run M73r killed |
| M74 | ui/text.rs | `Attention::DaemonStopped => Some("tokeman rotate install".to_owned()),` | `Attention::DaemonStopped => None,` | ui::text::what_needs | killed |  |
| M75 | ui/text.rs | `(Unkept::Paused, _) => "tokeman rotate resume".to_owned(),` | `(Unkept::Paused, _) => "tokeman rotate install".to_owned(),` | ui::text:: daemon::codex:: | **SURVIVED** | strengthened what_needs_the_user...; re-run M75r killed |
| M76 | ui/chart.rs | `cur.at - prev.at > gap_secs \|\| reset_changed \|\| cur.remaining > pr…` | `cur.at - prev.at > gap_secs \|\| reset_changed` | ui::chart::segments_break | killed |  |
| M77 | ui/chart.rs | `if point.at - prev.at > gap_secs && point.remaining != prev.remaining …` | `if false {` | ui::chart::a_held_value | killed |  |
| M78 | ui/chart.rs | `Span::FiveHour => Some((normal.session, sip.session)), Span::Weekly =>…` | `Span::FiveHour => Some((normal.weekly, sip.weekly)), Span::Weekly => S…` | ui::chart::only_the_rotation | killed |  |
| M79 | ui/chart.rs | `position.clamp(0.0, 1.0) * (right - left) as f64).round() as u32 } fn …` | `position * (right - left) as f64).round() as u32 } fn map_y` | ui::chart::coordinate | killed |  |
| M80 | ui/chart.rs | `let held = gap_secs.max(HELD_SECS).max(usual * 5 / 2);` | `let held = gap_secs.max(HELD_SECS);` | ui::chart:: | killed |  |
| M81 | ui/chart.rs | `if service != Service::Claude \|\| series.scope != Scope::All {` | `if series.scope != Scope::All {` | ui::chart::only_the_rotation | killed |  |
| M82 | ui/tui.rs | `!self.always_fresh && self` | `self` | ui::tui::the_header | killed |  |
| M83 | ui/tui.rs | `.is_none_or(\|overview\| overview.monitor.keeps_fresh())` | `.is_none_or(\|_\| false)` | ui::tui::the_header | killed |  |
| M84 | ui/tui.rs | `while used_through_selected > available && start < selected {` | `while false {` | ui::tui::the_selected | killed |  |
| M85 | ui/tui.rs | `.unwrap_or(known.len() - 1);` | `.unwrap_or(0);` | ui::tui::tab_steps | **SURVIVED** | left: which limit Tab selects first when the current one left the history; cosmetic |
| M86 | ui/tui.rs | `.map(\|p\| ((p.at - now) as f64 / 3600.0, p.remaining * 100.0))` | `.map(\|p\| ((now - p.at) as f64 / 3600.0, p.remaining * 100.0))` | ui::tui::chart_segments | killed |  |
| M87 | ui/tui.rs | `(Some(error), _) => (format!("error: {}", truncate_chars(error, 100)),…` | `(Some(error), _) => (format!("error: {}", truncate_chars(error, 100)),…` | ui::tui::a_card_without | killed |  |
| M09r | drift.rs | `write_atomic(&self.path, &bytes, 0o600)?;` | `write_atomic(&self.path, &bytes, 0o644)?;` | drift::the_ledger_reports | killed |  |
| M15cr | daemon/cadence.rs | `if reading.model_limit(&fable).is_some() { continue; }` | `(removed)` | daemon::cadence::samples_are | killed |  |
| M19r | daemon/notify.rs | `self.told.retain(\|_, told\| now - told.seen_at < REARM_SECS);` | `self.told.retain(\|_, told\| now - told.notified_at < REARM_SECS);` | daemon::notify::a_key | killed |  |
| M30r | daemon/codex.rs | `if paused { Self::Paused } else if !fleet.policy.codex_auto_move { Sel…` | `if !fleet.policy.codex_auto_move { Self::Off } else if paused { Self::…` | daemon::codex::the_daemon_looks | killed |  |
| M44r | commands/doctor.rs | `Monitor::Running \| Monitor::Paused \| Monitor::Stopped` | `Monitor::Running \| Monitor::Paused` | commands::doctor::a_missing_hook | killed |  |
| M44br | commands/doctor.rs | `Monitor::Running \| Monitor::Paused \| Monitor::Stopped` | `Monitor::Paused \| Monitor::Stopped` | commands::doctor::a_missing_hook | killed |  |
| M55br | commands/view.rs | `.filter(\|reading\| reading.accepts_work() && reading.limits.is_empty(…` | `.filter(\|reading\| reading.limits.is_empty())` | commands::view::best_skips | killed |  |
| M66br | ui/text.rs | `Monitor::Running if overview.at - read_at > STALE_SECS => {` | `Monitor::Running if overview.at - read_at >= STALE_SECS => {` | ui::text::the_caption | killed |  |
| M73r | ui/text.rs | `Outlook::Idle => continue,` | `Outlook::Idle => DIM,` | ui::text::the_reports_open | killed |  |
| M75r | ui/text.rs | `(Unkept::Paused, _) => "tokeman rotate resume".to_owned(),` | `(Unkept::Paused, _) => "tokeman rotate install".to_owned(),` | ui::text::what_needs | killed |  |
| R1 | codex/mover.rs | `) -> Look { let floors = terms.floors;` | `) -> Look { let _ = client.http().post(client.consume_url()).send().aw…` | commands::codex::watch_once daemon::codex::a_low_codex | killed |  |
| R2 | codex/mover.rs | `) -> Look { let floors = terms.floors;` | `) -> Look { let _ = client.http().get(client.credits_url()).send().awa…` | commands::codex::watch_once | killed |  |
| C1 | cli.rs | `(&["add"], "tokeman claude add <name> <key>"),` | `(&["add"], "tokeman claude add <name> <key>"), (&["stats"], "tokeman s…` | cli::a_removed_spelling | killed |  |
| N1a | commands/view.rs | `if !force && monitor.keeps_fresh() {` | `if !force && !monitor.keeps_fresh() {` | commands::view::a_surface_reads | killed |  |
| N1b | commands/view.rs | `if !force && monitor.keeps_fresh() {` | `if monitor.keeps_fresh() {` | commands::view::a_surface_reads | killed |  |
| N1c | commands/view.rs | `if !force && monitor.keeps_fresh() {` | `if false {` | commands::view::a_surface_reads | killed |  |
| N1d | ui/overview.rs | `self == Self::Running` | `matches!(self, Self::Running \| Self::Paused)` | commands::view::a_surface_reads | killed |  |
| N2a | daemon/services.rs | `fn record(&self, usages: &[Reading]) {` | `#[allow(dead_code)] async fn spend(plan: &crate::codex::resets::ResetP…` | daemon::nothing_but | killed |  |
| N2b | codex/mover.rs | `) -> Look { let floors = terms.floors;` | `) -> Look { let _ = client.http().post(client.consume_url()).send().aw…` | daemon::nothing_but | killed |  |
| N2c | main.rs | `if !yes && !prompt::confirm("use it now?")? { text::print_kept(); retu…` | `if !yes { text::print_kept(); }` | daemon::nothing_but | killed |  |
| N2d | main.rs | `if !yes && !prompt::confirm(&format!("spend {which} on {account} now?"…` | `if yes && !prompt::confirm(&format!("spend {which} on {account} now?")…` | daemon::nothing_but | killed |  |
| N2e | launch.rs | `pub async fn run(` | `#[allow(dead_code)] async fn spend(client: &crate::anthropic::Client) …` | daemon::nothing_but | killed |  |
| N3a | cli.rs | `#[arg(long, requires = "name", conflicts_with_all = ["grant", "session…` | `#[arg(long, requires = "name")]` | cli::a_reset_is_confirmed | killed |  |
| N3b | cli.rs | `/// Do not ask #[arg(long, requires = "name")] yes: bool,` | `/// Do not ask #[arg(long)] yes: bool,` | cli::a_reset_is_confirmed | killed |  |
| N3c | cli.rs | `#[arg(long, conflicts_with_all = ["credit", "yes"])]` | `#[arg(long)]` | cli::a_reset_is_confirmed | killed |  |
| N3d | cli.rs | `/// Do not ask #[arg(long, requires = "name")] yes: bool,` | `/// Do not ask #[arg(long, requires = "name", default_value_t = true)]…` | cli::a_reset_is_confirmed | killed |  |
| N4a | daemon/cycle.rs | `limit.room(weekly.as_ref()) * 100.0` | `limit.window.remaining() * 100.0` | daemon::cycle::a_reading_is_described | killed |  |
| N4b | daemon/cycle.rs | `(None, crate::quota::SessionLimit::Absent) => "none (no 5h limit)".to_…` | `(None, crate::quota::SessionLimit::Present) => "none (no 5h limit)".to…` | daemon::cycle::a_reading_is_described | killed |  |
| N4c | daemon/cycle.rs | `for reading in readings.iter().filter(\|reading\| !reading.has_quota()…` | `for reading in readings.iter().filter(\|reading\| reading.error.is_som…` | daemon::cycle::coverage | killed |  |
| N5a | ui/overview.rs | `.filter(\|reading\| reading.error.is_none() && reading.verdict() != Ve…` | `(removed)` | ui::overview::an_account_that | killed |  |
| N5b | ui/overview.rs | `&& reading .shared() .all(\|limit\| !limit.is_rejected() && limit.wind…` | `(removed)` | ui::overview::an_account_that | killed |  |
| N5c | ui/overview.rs | `view.viable = reading.map(Reading::accepts_work);` | `view.viable = reading.map(\|_\| true);` | ui::overview::an_account_that | killed |  |
| N5d | ui/overview.rs | `.filter(\|account\| account.service == service && !account.parked)` | `.filter(\|account\| account.service == service)` | ui::overview::a_parked | killed |  |
| N6a | ui/text.rs | `Verdict::Rejected => ("REJECTED", Some(Level::Critical)),` | `Verdict::Rejected => ("allowed", Some(Level::Ok)),` | ui::text::a_claude_badge | killed |  |
| K1 | ui/chart.rs | `.filter(\|(series, _)\| series.scope == Scope::All) .min_by_key(\|(ser…` | `.find(\|(series, _)\| series.scope == Scope::All)` | ui::chart::the_default_metric | killed |  |
| K2 | ui/chart.rs | `.min_by_key(\|(series, _)\| series.span.seconds().unwrap_or(i64::MAX))` | `.max_by_key(\|(series, _)\| series.span.seconds().unwrap_or(i64::MAX))` | ui::chart::the_default_metric | killed |  |
| T1 | cli.rs | `&["tray"],` | `&["trays"],` | cli::a_removed_spelling | killed |  |

### Deleted

| test | reason |
|---|---|
| commands::codex `report_rows_and_warnings_read_plainly` | c: asserted the `Display` text of one `RefreshReport` and one `Warning`; no rule, no branch |
| ui::text `an_api_error_body_is_shown_as_its_message` | d: the same `brief_error` cases as `an_error_with_a_json_body_is_shown_by_its_message`; its two extra cases (a `request_id` beside the error, a body that is not JSON) moved into that test |
| ui::text `text_helpers_count_characters_not_bytes` | d: two of three assertions re-tested `util::one_line`, which `util::tests` covers; the `truncate_chars` case stays as `truncation_counts_characters_not_bytes` |

### Strengthened

| test | what was weak | mutation it now catches |
|---|---|---|
| drift `the_ledger_reports_an_item_as_new_once...` | the ledger's mode was unpinned | M09r: written 0644 |
| daemon::cadence `samples_are_recorded_then_carried_until_reset` | nothing said a carried sample must not replace a limit the probe read | M15cr: the guard removed |
| daemon::notify `a_key_is_told_once_and_again_at_its_next_threshold` | "once" was checked at +2 min and +1 day only | M19r: forgetting by `notified_at` instead of `seen_at` (re-notifies a standing item every 6 h) |
| daemon::codex `the_daemon_looks_only_with_the_setting_on...` | off and paused were never both set | M30r: off judged before paused (stops tending pushed seats under the wrong word) |
| daemon::codex `a_low_codex_is_moved_in_the_background...` | `assert!(logins().is_empty() \|\| running.is_some())` could not fail; nothing said a move spends no reset | R1: a POST to `/credits/consume` in `mover::consider` |
| commands::codex `watch_once_moves_a_low_codex...` | nothing said a move lists or spends no credit, though the landing has two on offer | R1, R2: a consume POST, a credit-list GET |
| commands::doctor `a_missing_hook_is_a_problem_only_where_the_service_is_installed` | only `Paused` was tried | M44r, M44br: `Stopped` / `Running` dropped from "service expected" |
| commands::view `best_skips_parked_and_unviable...` | a refused account with no window could have been the "last resort" | M55br: `accepts_work()` dropped |
| ui::text `the_caption_says_where_the_view_comes_from...` | stale at 2 h only | M66br: `>=` at the ten-minute boundary; the constant itself |
| ui::text `what_needs_the_user_is_a_sentence...` | the fix of a paused pushed seat unpinned | M75r |
| ui::text `the_reports_open_with_what_needs_attention...` | an idle forecast's line unpinned | M73r |
| ui::text `an_error_with_a_json_body_is_shown_by_its_message` | took the two cases of the test folded into it | (unchanged coverage) |
| cli `a_removed_spelling_does_not_parse_and_names_its_replacement` | walked a hand-written list, not the table | C1: a `REMOVED` row for a spelling that still parses; T1: the `tray` row |

### Added

| test | rule it pins | mutation seen to fail it |
|---|---|---|
| commands::view `a_surface_reads_no_account_while_the_service_reads` | viewer mode: under `Monitor::Running` the view is the stored one and no account is read; every other state and `--fresh` read | N1a (condition inverted), N1b (`force` ignored), N1c (never stored), N1d (`keeps_fresh` true while paused) |
| daemon `nothing_but_the_reset_commands_can_reach_a_claim` | nothing spends a reset by itself. STRUCTURAL, over the source: the six steps of a claim (`resets::claim(`, `reset_claim(`, `.begin(`, `consume_url(`, `reset_rate_limits`, `oauth_post(`) are named only in the files that own them, and both calls in `main.rs` follow `if !yes && !prompt::confirm(` with a return. Structural because `cycle::run` and `launch` use `Client::shared()` at the live API and the global paths; a behavioural test would send real requests. The Codex mover's side IS behavioural (the two strengthened move tests). | N2a (a call to `commands::codex::reset_claim` in `daemon/services.rs`), N2b (a consume POST in `codex/mover.rs`), N2e (a `reset_rate_limits` POST in `launch.rs`), N2c (the prompt removed in `main.rs`), N2d (`!yes` flipped) |
| cli `a_reset_is_confirmed_only_by_yes_and_clear_goes_with_no_claim` | `reset <account>` parses with `yes = false`; a reset needs its account; `--clear` cannot ride with `--yes`, `--grant`, `--session`, `--credit` | N3a, N3c (`conflicts_with_all` removed), N3b (`requires = "name"` removed), N3d (`yes` defaulting to true) |
| daemon::cycle `a_reading_is_described_by_what_is_left_on_each_limit_that_counts` | the cycle's log line: a model limit counts only for the session's model and by `Limit::room` (share of the weekly); an account without a 5-hour limit says so | N4a (own window instead of `room`), N4b (Absent/Present swapped) |
| daemon::cycle `coverage_counts_the_readings_with_quota_and_groups_the_rest_by_cause` | coverage counts readings with quota; a read with no headers is unavailable too | N4c (only errored reads counted as unavailable) |
| ui::overview `an_account_that_cannot_take_work_is_not_room` | the status dot ignores a rejected account; a Codex account the backend refuses and a service account whose week is spent are not viable | N5a, N5b, N5c |
| ui::overview `a_parked_profile_is_not_counted_in_a_forecast` | a parked Codex profile adds no room to the forecast | N5d |
| ui::text `a_claude_badge_is_the_verdict_and_an_unread_account_is_an_error_or_no_quota` | REJECTED is said and is critical | N6a |
| ui::chart `the_default_metric_does_not_depend_on_which_account_was_read_last` | bug below | K1 (the old code), K2 (longest instead of shortest) |
| daemon::codex `a_test_run_looks_at_no_codex_home_outside_its_own` | containment: under test, Codex's fallback home and every home a look names are under `paths::TEST_HOME` | H1 (`default_home` built from a path outside it) |
| ui::text `truncation_counts_characters_not_bytes` | what is left of the deleted helper test | (not mutated; b) |

### Bugs

1. **The default chart limit flipped with the account read last.** `ui::chart::resolve(None, ..)` took the first
   account-wide limit of the newest reading (`Series::known` lists the newest reading's limits first). In a fleet
   with one account that has a 5-hour limit and one that has the weekly alone (today's weekly-only Claude accounts),
   `tokeman chart` and the chart of `--watch` drew 5h or weekly depending on which account the last read came from,
   so the `--watch` chart could change limit between two refreshes. Evidence: the new test failed on the second case
   (`left: Weekly, right: FiveHour`). Fixed in faa3cf3: the shortest account-wide window the history holds. NOT
   fixed: a history with scoped limits only (Antigravity) still charts the first limit its newest reading reports;
   one account's group order is the server's, so it flips only between accounts with different groups.
2. **`tokeman tray` was removed without a row in `cli::REMOVED`.** After lane `compact` deleted the egui tray,
   `tokeman tray` answered with clap's unknown-subcommand error; DESIGN's rule is that every removed spelling is
   refused naming its replacement and has its CHANGELOG row. Evidence: the test line `("tray", ...)` panicked with
   "tray is not in the table". Fixed in 23038aa (`REMOVED` row, CHANGELOG row; the pin test covers both).

Not a bug, but a decision ember should know is now pinned by a test: **a paused daemon holds `daemon.lock` and the
surfaces still read every account themselves** (`Monitor::Paused.keeps_fresh()` is false). DESIGN says "stored
while `Monitor::keeps_fresh()`", so the code follows the design, but the task's phrase "no request while the daemon
lock is held" is not what happens while paused: the paused daemon keeps polling the services and sweeping Codex, and
a `tokeman` run beside it reads Codex and the services again.

### Not tested and matters

| what | why it matters | why not done |
|---|---|---|
| `daemon::cycle::run` control flow: paused returns before anything; a scheduled tick that is not due sends nothing; **dry run writes nothing** (cadence, settings, history, store); only accounts asked this cycle change the backoff; a switch installs and records | this is the rotation; "dry run means no write anywhere" is a DESIGN rule with no test | `run` takes no seam: `Client::shared()` (live API), `paths::` globals for the lock, cadence, settings and store, and `keychain::suppress` (the real `security` binary) on an unscheduled run. A test would either need those four injected (a refactor of the cycle's signature, not a test repair) or would send live requests when it regresses. The policy decision inside it is tested in `policy.rs` (g1). |
| `daemon::refresh::refresh_where`: the grant is re-read under the fleet lock right before redeeming, a permanent refusal is recorded once, the default is re-offered | single-use refresh tokens | same reason: `Fleet::load_locked()` and `Client::shared()` have no seam here. g2 covers `oauth::refresh` and the locked RMW of `account.rs`; the sweep that composes them is untested. |
| `commands::doctor::gather` | what `doctor` actually reads | live reads, real binaries (`claude --version` etc.); `Report::assemble` (pure) is fully tested |
| `commands::codex::import_into(.., register = true)` marks profiles `access_only` (M41b survived) | an access-only profile must never be refreshed | registration is `Fleet::update` on the account file with no path seam |
| `commands::rotate` (status, quarantine, observe_session), `commands::services` (add, remove, agy_use, agy_run), `commands::claude` (add, remove, finish_login, reset_plan) | the command layer | thin wrappers over tested domain code plus `Fleet::load/update`; `reset_plan`'s choice of grant (named, pending, next) is logic worth a test and has none because it reads through `Client::shared()` |
| `main.rs` dispatch | the confirm gate before a claim, the removed-spelling refusal | a binary crate; `cargo test --lib` does not build its tests. The structural test reads `main.rs` for the confirm gate; `cli::removed` is tested, its call in `main` is not |
| `text::print_*`, `print_uncompacted`, `print_compaction`, `prompt.rs` | what the user reads | printing; the functions that build the text (`*_lines`, `summary`, `view_caption`, `headlines`) are tested |
| `daemon::notify::deliver` and `tell(.., enabled = true)` | the real notifier | would post a desktop notification from `cargo test`; `run` (bounded, failure reported) and the ledger-before-send order are tested |
| `daemon::service::install/uninstall`, `daemon::install/pause/resume/activate` | launchd, settings | `launchctl` and the live settings file |
| colour of an attention line (M72), which limit Tab lands on when the charted one left the history (M85) | cosmetic | left |

### Deferred (audit-only files)

None left: `src/daemon/codex.rs` was released to this lane after the Codex follow-up merged and is audited, mutated
(M30-M34b, R1) and edited above. `src/ui/tray.rs` was deleted by lane `compact`.

### Cross-group

- g2 (`src/launch.rs`): `EnvVar` and `Launch` derive `Debug` with the secret's value in it; `text::launch_line` is
  the only printer and is tested, but a `{launch:?}` anywhere would print the key. Worth a manual `Debug` or a test.
- g1 (`src/store.rs`): `Series::known` orders by the newest reading; `ui::chart::resolve` no longer depends on that
  for account-wide limits, the TUI's Tab order still does.
- root: RULES.md and BRIEF-COMMON.txt still list the `--features tray` gates; they no longer exist.

### Hygiene notes

- **Real paths.** With the root's panic patch, 13 tests of this group resolved a real location. None read or wrote
  a real file: nine formatted a path with `paths::tilde`, `Poller::default()` and `Terms::of -> Seats::default()`
  built a default path and never opened it, and doctor's fix text named `paths::claims_journal()`. Fixed here:
  `daemon::services` test builds its `Poller` on a fixed path; the daemon's look builds its `Terms` from the injected
  seat file; `daemon/service.rs` and `commands/codex.rs` ask `paths::home()` (no `dirs::` call is left in this
  group's files). The rest is contained by the root's `paths` seam (7da6dac), which this lane proposed.
- **The real `~/.codex`.** `daemon::codex` tests ran `mover::homes(&fleet)`, whose first entry was the real
  `~/.codex`, with `dry_run = false`. On this machine `~/.codex/app-server-control/app-server-control.sock` is a
  dangling symlink (its target directory does not exist), so every run answered `NoSocket` and nothing was read or
  pushed; with a Codex app-server running there the test could have moved it onto a test token. Now closed twice:
  `CodexHome::default_home` goes through `paths::home()` (g3), and the daemon's `Mover` holds its fallback home as
  a field that tests leave empty. `a_test_run_looks_at_no_codex_home_outside_its_own` pins it. This lane ran those
  tests about ten times before the hazard was known.
- **Real domain.** The import test used an address on the real domain x.com; it is an example.com address now. The `a@x.y` / `a@b.c` addresses are
  not real domains and were left.
- **Wall clock.** `daemon::codex` (two tests from the follow-up) and `daemon::services` build their fixtures around
  `chrono::Utc::now()` because the code under test reads the clock itself; margins are an hour or more. `daemon::notify`
  and `daemon::services` compare `Instant`s with a 1 s / 5 s bound. No test of this group depends on the time zone:
  the `Local`-formatted dates in `ui::text` are asserted by prefix only.
- **Sleeps.** `daemon::codex::tests::finished` polls with a 5 ms tokio sleep; `a_notifier_is_bounded` waits 200 ms
  for its timeout (it spawns `/bin/sh -c "sleep 30"`, killed on drop). `DaemonLock::take_at` retries for 5 s when the
  lock is held; the lock test avoids that path by using `FileLock::try_acquire` for the second taker.
- **Real binaries.** `/bin/sh` in the notifier test. No test of this group reaches `osascript`, `security`, `ps`,
  `launchctl`, `claude`, `codex` or `agy`: `notify::deliver`, `doctor::gather`, `service::install` are not called
  by any test. Two mutations were NOT run for that reason (making the first notifier round immediate, sending before
  saving): under them the suite would post a real notification.
- **Env reads.** `commands::launch::codex` reads `TOKEMAN_CODEX_BIN` in its test; the test does not assert on the
  program, so a set variable cannot fail it.
- **Source-reading tests.** `cli::the_changelog_table...` (`include_str!`) and the new structural test (reads
  `src/**/*.rs` under `CARGO_MANIFEST_DIR` at run time) depend on the checkout, not on the machine.

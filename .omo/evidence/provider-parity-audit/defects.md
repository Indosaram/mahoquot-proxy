# Provider proxy parity defect register

Scope: `mahoquot-proxy` (O) audited against `opencodex` (A, TypeScript) and
`CLIProxyAPI` (B, Go). Six read-only lanes produced id-stable findings; this file
is the consolidated register plus the required coverage matrix.

Lane reports (detail, citations, fix sketches, test seams):

| lane | file | rows | FIX | BETTER | DEVIATION |
|---|---|---|---|---|---|
| 1 Claude/Anthropic | `lanes/l1-claude.md` | 14 | 11 | 0 | 3 |
| 2 Gemini/Antigravity/Vertex | `lanes/l2-gemini.md` | 12 | 10 | 1 | 1 |
| 3 Codex/OpenAI/Responses | `lanes/l3-openai.md` | 11 | 9 | 1 | 1 |
| 4 Auth/quota/routing | `lanes/l4-auth.md` | 10 | 8 | 0 | 2 |
| 5 Streaming/SSE/failover | `lanes/l5-streaming.md` | 7 | 5 | 1 | 1 |
| 6 Provider adapters | `lanes/l6-adapters.md` | 12 | 7 | 0 | 5 |
| **total** | | **66** | **50** | **3** | **13** |

Counts are taken from the verdict cell of every row in the six lane tables, not
carried forward by hand. In this file the same set appears as 50 FIX rows, 3
BETTER rows and the L6-03 row, plus a `## DEVIATION` prose list naming all
thirteen deviations (the twelve that have no row here, and `L6-03` again for
traceability); the lane tables give all thirteen their own row. Either
way the thirteen IDs match `provider-parity-deviations.json` one-for-one.

`L1-10` and `L1-11` were found by the final gates' relay-path checks, not
by the original lane pass, and are recorded in `lanes/l1-claude.md`.

Every FIX cell is a row that survived review and is proved red/green. The one
row that was classified FIX when found and did not survive is no longer counted
there:

- `L6-03` (cursor exec) is **DEVIATION L6-03** in both the register and the lane
  table — local execution is the documented default, not a defect. Its hardening
  test is still proved, as `L6-EXEC`.

`L1-09` was first recorded as falsified and then **reinstated as a real defect**
by the final review gate: `apply_prompt_cache_breakpoints` returned early
whenever any `cache_control` was present, so a client sending more than four
breakpoints reached Anthropic unchanged (HTTP 400). Both references enforce the
cap (A `src/adapters/anthropic.ts:189-221`, B
`claude_executor_execute.go:173-176`); ours now does too.

Register totals, recounted by verdict cell from the six lane tables:
**66 lane-table rows = 50 FIX + 3 BETTER (`L2-S1`, `L3-S1`, `L5-06`) + 13
DEVIATION**. This file carries the same set as 50 FIX rows, 3 BETTER rows, one
`L6-03` row and a `## DEVIATION` list naming all thirteen IDs. The thirteen deviation IDs
match `provider-parity-deviations.json` one-for-one.
`proofs/red-green.md` holds **51 results: 50 prove FIX rows and `L6-EXEC`
proves audit hardening** (a contained `..` escape) that is not a defect against
either reference. `L6-03` is the one row that moved from FIX to DEVIATION and
claims no ledger row; it stays in the register for traceability.

Three entries were added after the original six-lane pass, all by review gates:
`L1-09` (breakpoint cap), `L1-10` (that cap only guarded the subscription path,
so Zcode native and relay-compatible destinations still forwarded five markers), and `L1-11`
(the Generic `anthropic` adapter returned before that cap). All three are
proven red/green.

Verdict vocabulary: **FIX** = our behavior is wrong relative to at least one
reference and not covered by a documented deviation. **BETTER** = our behavior
differs and is strictly better; keep. **DEVIATION** = difference is documented
policy (code comment or `docs/reference/provider-parity-deviations.json`).
**NOT-CERTIFIED** = neither reference contains the path or evidence needed to
compare this concern, so no parity claim is made. Absence of a reference is not
evidence of parity, and these cells are excluded from every equal count.
**EQUAL (shared)** = a gateway-wide component (request/response translation, SSE
parser, retry classification, usage defaults) matched the named reference
function, but the provider-specific dispatch around it is not certified. It is
not a claim of provider-level parity.

## FIX register

Severity: C = critical, H = high, M = medium.

| id | sev | subsystem | wrong behavior (one line) | fix state |
|---|---|---|---|---|
| L1-01 | H | claude usage | OpenAI prompt/total omit Anthropic cache read+write | fixed + `compat::claude::tests::anthropic_cache_usage_is_inclusive_json_and_stream`|
| L1-02 | H | claude usage | Anthropic render emits inclusive prompt, drops cache fields | fixed + `compat::claude::tests::anthropic_render_usage_excludes_known_caches`|
| L1-03 | H | claude stop | `stop`/`stop_sequences` never cross-mapped | fixed + `compat::claude::tests::claude_stop_controls_cross_map`|
| L1-04 | H | claude limits | `max_completion_tokens` ignored, 32768 default used | fixed + `compat::claude::tests::claude_honors_max_completion_tokens`|
| L1-05 | H | claude images | url image sources silently dropped | fixed + `compat::claude::tests::anthropic_url_images_survive_chat_translation`|
| L1-06 | H | claude tools | mixed `[tool_result, text]` reordered past the result | fixed + `compat::claude::tests::anthropic_tool_results_precede_user_followup`|
| L1-07 | H | claude thinking | native thinking disabled/adaptive budget discarded | fixed + `compat::claude::tests::anthropic_thinking_controls_survive_codex_plan`|
| L1-08 | H | claude replay | signed thinking not rebuilt before tool calls | fixed + `compat::claude::tests::claude_replays_signed_assistant_reasoning_before_tools`|
| L1-09 | M | claude cache | >4 cache breakpoints pass through unnormalized | fixed + `compat::claude::openai_to_anthropic_tests::excess_client_breakpoints_are_capped_at_the_provider_limit` (reinstated by the final gate; first recorded as falsified in error) |
| L1-10 | M | claude cache | the cap only guarded the subscription path, so Zcode native and relay-compatible destinations still received five markers upstream | fixed + `zcode_native_client_breakpoints_are_capped_before_upstream` (cap moved to the single `/v1/messages` serialization point) |
| L1-11 | M | claude cache | the Generic `anthropic` adapter returns before the shared `/v1/messages` serialization point, so it forwarded all five client markers and Anthropic would answer HTTP 400 | fixed + `generic_anthropic_adapter_caps_client_breakpoints` (the cap now runs inside `openai_to_anthropic`, so every translation inherits it) |
| L2-01 | H | vertex url | regional location keeps global host | fixed + `url::tests::vertex_location_selects_regional_host`|
| L2-02 | H | gemini finish | safety/recitation/malformed become success | fixed + `compat::gemini::tests::gemini_finish_reason_safety_is_not_success`|
| L2-03 | M | gemini usage | completion excludes thoughts; buffered cache dropped | fixed + `test_t9_l2_03_gemini_stream_and_buffered_usage_match`, `test_t9_l2_03_gemini_native_usage_roundtrips`; promptTokenCount no longer adds cached (subset), completion includes thoughts |
| L2-04 | M | gemini stop | `stop` never becomes `stopSequences` | fixed + `compat::gemini::tests::gemini_stop_sequences_are_forwarded`|
| L2-05 | H | gemini schema | `$ref` left dangling after `$defs` strip | fixed + `compat::gemini::tests::gemini_local_ref_schema_is_resolved`|
| L2-06 | M | gemini tools | invalid tool identifiers passed to Gemini | fixed + `compat::gemini::tests::gemini_tool_name_codec_roundtrips`|
| L2-07 | H | gemini thinking | explicit budget/auto loses to default HIGH | fixed + `compat::gemini::tests::gemini_explicit_budget_beats_model_default`|
| L2-08 | M | gemini format | `response_format` only applied on Vertex | fixed + `compat::gemini::tests::gemini_response_format_on_generic_and_antigravity`|
| L2-09 | H | antigravity auth | onboarding persists empty project_id | fixed + test `antigravity_login_waits_for_project_before_persist` |
| L2-10 | M | gemini audio | valid `input_audio` silently discarded | fixed + `compat::gemini::tests::gemini_input_audio_is_rejected`|
| L3-01 | H | responses native | native `/v1/responses` bypasses Codex shaping | fixed + `relay::native_responses_tests::native_codex_responses_drops_envelope_fields_the_backend_rejects` and 5 sibling tests in that module |
| L3-02 | H | responses tools | done-only function arguments lost | fixed + `compat::render::tests::codex_done_only_arguments_survive_chat_rendering`|
| L3-03 | M | responses finish | `response.incomplete` becomes upstream error | fixed + `compat::render::tests::codex_incomplete_preserves_chat_reason_and_usage`|
| L3-04 | H | codex tools | tool ids >64 chars relayed | fixed + `compat::request::tests::codex_long_call_ids_keep_injective_pairing`|
| L3-05 | M | responses options | effort + `parallel_tool_calls:false` dropped | fixed + `compat::responses::tests::responses_claude_controls_survive_translation`|
| L3-06 | H | responses tools | `function_call_output` arrays stringified | fixed + `compat::responses::tests::responses_tool_output_array_retains_image_order` |
| L3-07 | H | responses compact | compact runs ordinary inference | fixed + `compat::responses::tests::compact_requests_summary_and_rejects_tool_only_reply`|
| L3-08 | M | chat reasoning | `reasoning.summary` dropped when effort present | fixed + `compat::request::tests::codex_explicit_reasoning_summary_is_retained`|
| L3-09 | H | generic chat | Responses client on a chat upstream decoded with the Responses parser, yielding an empty answer | fixed + `compat::chat::tests::chat_stream_deltas_reach_the_responses_renderer_as_text`, `tests/generic_chat_responses.rs` (stream and buffered) |
| L4-01 | H | auth refresh | proactive dead grant removes usable access token | fixed + `pre_expiry_refresh::tests::a_rejected_grant_keeps_the_working_access_token_routable`|
| L4-02 | H | auth refresh | 400-403 blanket classified terminal | fixed + `pre_expiry_refresh::tests::only_a_rejected_grant_benches_the_account`|
| L4-03 | H | auth logs | refresh error Display leaks response bodies | fixed + `refresh_exec::tests::a_status_error_never_renders_the_upstream_body`|
| L4-04 | H | import dedupe | email outranks account_id identity | fixed + `management::creds::tests::codex_import_keys_on_account_id`|
| L4-05 | H | reauth | reauth clears operator `disabled` | fixed + `management::oauth::tests::reauth_preserves_operator_disabled`|
| L4-06 | H | oauth persist | local writes bypass composition lock | fixed + `management::oauth::tests::oauth_write_takes_composition_lock` |
| L4-07 | H | refresh replay | no durable grant claim before dispatch | fixed + `codex_takeover_persists_the_rotated_grant`, `codex_takeover_recreates_the_pending_receipt`, `codex_takeover_has_exactly_one_winner`; receipt-less claims take over after 6h (`codex_abandoned_claim_reaches_an_actionable_state`); complete HTTP responses release, transport/parse failures retain (`codex_http_error_response_releases_claim`, `codex_transport_failure_after_dispatch_retains_claim`, `codex_grant_rejection_releases_claim`); acquire/complete/release all run in `spawn_blocking` |
| L4-08 | M | routing | WRR accepted but is a no-op | fixed + test `weighted_round_robin_is_refused_because_no_selector_implements_it` |
| L5-01 | H | raw sse | generic/native SSE has no preflight or terminal check | fixed + frame-aware terminal scan (`raw_sse_terminal_frames_control_eof_and_preserve_bytes`, `terminal_scan_requires_complete_bounded_frames`, `double_observation_corrupts_a_split_terminal_frame`, `raw_stream_observes_the_preflight_chunk_exactly_once`): parses SSE event/JSON, ignores terminal words in text content, accepts whitespace chat finish_reason and the `[DONE]` sentinel, observes each forwarded chunk exactly once, forwards bytes unchanged |
| L5-02 | H | stalls | translated stream never expires upstream stall | fixed + `compat::bounded_stream_tests::translate_stall_budget_accounts_only_upstream_wait` |
| L5-03 | M | heartbeats | responses/gemini translated streams stay silent | fixed + `compat::bounded_stream_tests::responses_and_gemini_idle_tick_is_complete_comment`|
| L5-04 | H | anthropic preflight | empty text block counts as served | fixed + `compat::bounded_stream_tests::anthropic_empty_text_preamble_remains_retryable` |
| L5-05 | M | cooldown | HTTP-date `Retry-After` ignored | fixed + `relay::usage_capture_tests::cooldown_retry_after_http_date_precedes_reset`|
| L6-01 | H | cursor media | images dropped; image-only turn rejected | fixed + `compat::cursor::tests::cursor_image_only_encodes_selected_image`|
| L6-02 | H | cursor blobs | SetBlob acknowledged without storage | fixed + `compat::cursor::tests::cursor_kv_set_then_get_preserves_bytes`|
| L6-03 | C | cursor exec | local exec default-on without operator opt-in | RECLASSIFIED as DEVIATION L6-03 (`behavioralDeviations[L6-03]`): local execution is the documented default when `MAHOQUOT_EXEC_BRIDGE` is unset (`compat/exec_bridge.rs:52-74`) and `MAHOQUOT_EXEC_ROOT` defaults to `/` (`compat/cursor.rs:1329-1334`). Audit hardening retained: `..` escapes are contained (`compat::cursor::tests::exec_write_rejects_dotdot_escape`, proven red/green as `L6-EXEC`) |
| L6-04 | H | kiro history | historical tool pairing/images lost | fixed + `compat::kiro::tests::kiro_prior_tool_result_survives_new_user`|
| L6-05 | H | kiro reasoning | `redactedContent` dropped | fixed + `kiro_redacted_reasoning_roundtrips`: blob carried on the Responses reasoning item and decoded back into Kiro replay for reasoning-only and tool-only turns |
| L6-06 | M | kiro usage | `tokenUsage` ignored, usage=None | fixed + `compat::kiro::tests::kiro_metadata_usage_survives_stop_and_eof`|
| L6-07 | H | command code | OAuth routed through generic chat adapter | fixed + 2 tests in tests/command_code_wire.rs |
| L6-08 | M | interactions | Devin Interactions unsupported (503) | fixed + 3 tests in interactions.rs |

## Coverage matrix (C001)

Ten provider kinds x the eight shared concerns named in C001, in twelve
blocks: the ten `ProviderKind` variants at
`crates/gateway/src/account.rs:130-142`, plus the `command-code` and `meta`
generic adapters that dispatch through `ProviderKind::Generic`
(`relay.rs:1464-1480`, `:1548-1568`). Every cell carries our site, the
governing reference site (A = opencodex, B = CLIProxyAPI), a verdict, the
observable behavior, and for FIX the proving test.

Verdict vocabulary: **FIX** = we were wrong and repaired it; **BETTER** = we differ
and are superior; **DEVIATION** = declared in
`docs/reference/provider-parity-deviations.json` or documented at the code site;
**EQUAL** = compared against a reference that implements the same provider, same
behavior, no change needed; **EQUAL (shared)** = a gateway-wide component matched
the named reference function while the provider-specific dispatch is not
certified; **NOT-CERTIFIED** = no reference implements or evidences this concern,
so no parity is claimed and the cell is excluded from every equal count.

Shared concerns and their single implementation site:

| # | concern | shared site |
|---|---|---|
| 1 | upstream headers | `relay.rs:5286-5330` (`strip_ingress_headers`, `header_value`), per-provider UA in `providers/src/*` |
| 2 | auth + refresh | `providers/src/refresh.rs:43-160`, `refresh_exec.rs`, `gateway/src/account.rs:2787-2980` |
| 3 | request translation | `compat/request.rs`, `compat/claude.rs`, `compat/gemini.rs`, `compat/kiro.rs`, `compat/cursor.rs`, `compat/devin.rs`, `compat/command_code.rs` |
| 4 | response translation | `compat/render.rs`, `compat/events.rs:236-430` |
| 5 | SSE streaming | `relay.rs:2809-3000` (from `RawSseWire` doc), `compat/mod.rs:184-680`, `compat/events.rs:56-60` |
| 6 | failover / retry | `relay.rs:21-40,2257-2420,2588` |
| 7 | catalog / model listing | `registry/src/lib.rs:1737`, `gateway/src/models_route.rs:472` |
| 8 | quota | `gateway/src/usage.rs:96`, `relay.rs:4230-4272` (`usage_header_prefix` + `capture_usage`) |

### codex

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `providers/src/account.rs:3-4,114` | A `src/adapters/openai-responses.ts:1335-1341` | EQUAL | pinned Codex UA, ingress routing headers stripped |
| 2 | auth + refresh | `account.rs:2787-2980`; `providers/src/account.rs` | A `src/oauth/store.ts:183-205`; B `internal/auth/codex/refresh_journal.go:16-26,72-103` | FIX L4-07 | receipt-less claims take over after 6h; complete responses release, transport failures retain; `codex_abandoned_claim_reaches_an_actionable_state` |
| 3 | request translation | `compat/request.rs:25-35,137-164,273-319,369-378` | A `openai-responses.ts:2180-2302`; B `codex_openai_request.go:69-72,176-179,326-355` | FIX L3-01/L3-04/L3-06/L3-08 + DEVIATION L3-S2 (`behavioralDeviations[L3-S2]`) | native `/v1/responses` shaping, injective long call-id pairing, tool-output arrays keep order, explicit reasoning summary; `native_codex_responses_drops_envelope_fields_the_backend_rejects`  token limits dropped for Codex destinations; |
| 4 | response translation | `compat/render.rs:1069`, `compat/events.rs:236-430` | B `codex_openai_response.go:275-362` | FIX L3-02/L3-03 | `[DONE]`-only arguments survive; `response.incomplete` becomes a length finish, not an error; `codex_incomplete_preserves_chat_reason_and_usage` |
| 5 | SSE streaming | `relay.rs:2809-3000` (from `RawSseWire` doc) | A `src/server/chat-native-sse.ts:294-305`; B `claude_executor_stream.go:476-491` | FIX L5-01 | frame-aware terminal scan, each chunk observed once, bytes unchanged; `raw_sse_terminal_frames_control_eof_and_preserve_bytes` |
| 6 | failover / retry | `relay.rs:21-40,2588` | A `src/combos/failover.ts:116-137` | FIX L5-05 | HTTP-date `Retry-After` honoured before the reset header; `relay::usage_capture_tests::cooldown_retry_after_http_date_precedes_reset` |
| 7 | catalog | `registry/src/lib.rs:1737`; `models_route.rs:472` | A registry snapshot | EQUAL | codex models from the embedded snapshot |
| 8 | quota | `relay.rs:4230-4272` (`usage_header_prefix` + `capture_usage`) | B `sdk/cliproxy/auth/conductor_cooldown.go:2369-2454` | EQUAL | `x-codex-`/`anthropic-ratelimit-` families captured per provider kind; `relay::usage_capture_tests::claude_responses_are_scanned_for_subscription_quota_headers` |

### antigravity

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `providers/src/antigravity.rs:23` (constant) + `:202` (account header builder); applied at `quota.rs:925,971,1028` and `management/oauth.rs:184,384` | A `src/adapters/google.ts:954-959` | EQUAL + DEVIATION L2-S2 (`behavioralDeviations[L2-S2]`) | pinned CLI UA on stream and load calls; aistudio represents WebSocket workers while API-key traffic uses adapter=google (declared deviation) |
| 2 | auth + refresh | `management/oauth.rs:169-235` | A `src/oauth/google-antigravity.ts:108-136,208-215`; B `internal/auth/antigravity/auth.go:312-405` | FIX L2-09 | login waits for a non-empty project before persisting; `antigravity_login_waits_for_project_before_persist` |
| 3 | request translation | `compat/gemini.rs:1254-1262` (stop sequences), `:1388-1436` (local `$ref` resolution, invoked `:1344`), `:1479-1539` (response entry points) | A `google.ts:806-811`; B `gemini_openai_request.go:89-90,364-380` | FIX L2-04/L2-05/L2-08 | stop sequences forwarded, local `$ref` resolved, `response_format` honoured; `gemini_response_format_on_generic_and_antigravity` |
| 4 | response translation | `compat/gemini.rs:1620-1660` | B `gemini_openai_response.go:482-496,655-669` | FIX L2-03 | prompt excludes the cached subset, completion includes thoughts; `gemini_buffered_usage_includes_thoughts_and_cache` |
| 5 | SSE streaming | `compat/gemini.rs:1766-1900` | B `gemini_openai_response.go:637-680` | FIX L2-02 | safety/recitation finish is not success; `gemini_finish_reason_safety_is_not_success` |
| 6 | failover / retry | `relay.rs:21-40,2588` | A `src/oauth/generic-account-failover.ts:205-215` | FIX L5-05 | shared Retry-After parsing; `relay::usage_capture_tests::cooldown_retry_after_http_date_precedes_reset` |
| 7 | catalog | `registry/src/lib.rs:1737` | A registry snapshot | EQUAL | antigravity models from the snapshot |
| 8 | quota | `relay.rs:4230-4272` (`usage_header_prefix` + `capture_usage`) | B `sdk/cliproxy/auth/conductor_cooldown.go:2369-2391` | EQUAL | provider-scoped families; `relay::usage_capture_tests::claude_responses_are_scanned_for_subscription_quota_headers` |

### claude

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `relay.rs:1897-1947,5271-5282` | A `src/adapters/anthropic.ts:674-677` | EQUAL + DEVIATION L1-D3 (`behavioralDeviations[L1-D3]`) | client `anthropic-beta` merged, not overwritten; session headers isolated; later system turns stay in history (declared deviation) |
| 2 | auth + refresh | `providers/src/claude.rs`, `refresh.rs:77` | A `src/claude/outbound.ts:89-103` | EQUAL | refresh-token grant, same shape |
| 3 | request translation | `compat/claude.rs:700-985` (incl. the cache cap at `:1144-1208`), `:262-330`, `:444-526` | A `src/chat/inbound.ts:304-312`, `src/claude/inbound.ts:134-143,319-356,559-575`; B `openai_claude_request.go:77-118,299-302` | FIX L1-03..L1-07 + DEVIATION L1-D2 (`behavioralDeviations[L1-D2]`) | stop/max_completion_tokens cross-map, URL images survive, tool results precede user text, thinking controls survive; `claude_stop_controls_cross_map`  JSON response formats rejected unless the catalog advertises the capability; |
| 4 | response translation | `compat/claude.rs:219-229,1276-1300,1537` | A `anthropic.ts:569-584`; B `claude_openai_response.go:87-94` | FIX L1-01/L1-02 | OpenAI totals inclusive, Anthropic render exclusive with cache fields; `anthropic_cache_usage_is_inclusive_json_and_stream` |
| 5 | SSE streaming | `compat/claude.rs:1550-1600`, `compat/mod.rs:184-254` | A `src/adapters/anthropic.ts:916`; B `claude_openai_request.go:130-143` | FIX L1-03/L5-04 | streamed stop mapping; empty text block no longer counts as served; `compat::bounded_stream_tests` |
| 6 | failover / retry | `relay.rs:21-40,2588` | A `src/combos/failover.ts:116-137` | FIX L5-05 | shared Retry-After parsing; `relay::usage_capture_tests::cooldown_retry_after_http_date_precedes_reset` |
| 7 | catalog | `registry/src/lib.rs:1737` | A registry snapshot | EQUAL | claude models from the snapshot |
| 8 | quota | `routes.rs:610-612` (calls `compat::estimate_input_tokens`, defined `compat/claude.rs:1498-1519`) | B `claude_executor_tokens.go:144-157` | DEVIATION L1-D1 (`docs/reference/provider-parity-deviations.json` `behavioralDeviations[L1-D1]`) | local count_tokens approximation, documented |

### cursor

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `providers/src/cursor.rs:17` | A `src/adapters/cursor/native-exec.ts` | EQUAL | bearer + pinned host |
| 2 | auth + refresh | `providers/src/refresh.rs:90` | A cursor login/poll flow | EQUAL | bearer refresh with empty JSON body; `cursor_refresh_uses_bearer_empty_json_and_camelcase_response` |
| 3 | request translation | `compat/cursor.rs:24-200`, `:1341-1390` | A `src/adapters/cursor/protobuf-request.ts:1398-1406`; `cursor/exec-policy.ts:9-44` | FIX L6-01 + DEVIATION L6-03 | image-only turns encode `selected_images`; local exec runs on the gateway host when `MAHOQUOT_EXEC_BRIDGE` is unset (`compat/exec_bridge.rs:52-74`) with `MAHOQUOT_EXEC_ROOT` defaulting to `/` (`compat/cursor.rs:1329-1334`), contained by `..` rejection; `exec_write_rejects_dotdot_escape` |
| 4 | response translation | `compat/cursor.rs:290-420` | A `native-exec.ts` decode | EQUAL | Connect frames through the shared decoder |
| 5 | SSE streaming | `compat/cursor.rs:290-420` | A `native-exec.ts` | EQUAL | framed data with end-stream |
| 6 | failover / retry | `relay.rs:21-40` | A `failover.ts:116-137` | EQUAL | shared retry policy |
| 7 | catalog | `registry/src/lib.rs:1737` | A registry snapshot | EQUAL | `cursor-auto` and named models |
| 8 | quota | `relay.rs:4222-4252` | no reference surface | NOT-CERTIFIED | no rate-limit headers; usage from the shared decoder |

### kiro

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `providers/src/kiro_idc.rs:374-375` | A `src/adapters/kiro.ts` | EQUAL | pinned AWS SDK + KiroIDE UA |
| 2 | auth + refresh | `refresh.rs:102-140`, `kiro_idc.rs` | A kiro auth flow | EQUAL | social and IDC refresh shapes; `kiro_social_refresh_uses_refresh_token_json` |
| 3 | request translation | `compat/kiro.rs:5-135` | A `kiro.ts:656-673,722-731` | FIX L6-04 | historical tool results and images fold into the right turn, `a\|b` normalised; `kiro_prior_tool_result_survives_new_user` |
| 4 | response translation | `compat/kiro.rs:396-460` | A `adapters/kiro-events.ts:122-173` | FIX L6-05/L6-06 | `redactedContent` replayed; `tokenUsage` survives stop and EOF; `kiro_redacted_reasoning_roundtrips` |
| 5 | SSE streaming | `compat/kiro.rs:275-460` | A `adapters/kiro-events.ts:70-91` | EQUAL | AWS event frames decoded with tool/usage aggregation |
| 6 | failover / retry | `relay.rs:21-40` | A `failover.ts:116-137` | EQUAL | shared |
| 7 | catalog | `registry/src/lib.rs:1737` | A registry snapshot | EQUAL | runtime dot-notation ids (date-suffixed ids rejected upstream with `INVALID_MODEL_ID`) |
| 8 | quota | `relay.rs:4222-4252` | no reference surface | NOT-CERTIFIED | no quota headers; usage from the decoder |

### zcode

> Provider-level contract (endpoint path, upstream header set, model identity,
> refresh) is **not equal-certified**: neither reference contains a dedicated
> Zcode adapter (`lanes/l6-adapters.md:36`). Cells marked `EQUAL (shared)`
> compare a gateway-wide component against the named reference function and say
> nothing about Zcode dispatch.

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `providers/src/zcode.rs:32-38` | B `auth_files_plugin_oauth_test.go:426-447` covers plugin identifiers only, not the upstream header set | NOT-CERTIFIED | plan vs api host selected by family; no reference establishes the header set |
| 2 | auth + refresh | `providers/src/zcode.rs` | B `auth_files_plugin_oauth_test.go:426-447` covers plugin identifiers only | NOT-CERTIFIED | 64-hex token normalised; no reference establishes the refresh contract |
| 3 | request translation | `compat/zcode.rs:151-200` | B plan/api host split (no translator module) | DEVIATION L6-D2 (`behavioralDeviations[L6-D2]`) | plan identity applied per model family |
| 4 | response translation | shared `compat/mod.rs:400-470` | B openai-compatible decoder | EQUAL (shared) | OpenAI-compatible frames |
| 5 | SSE streaming | `compat/mod.rs:405-430` (shared parser) | B `internal/runtime/executor/openai_compat_executor.go:490,589` (scanner) | EQUAL (shared) | OpenAI frames |
| 6 | failover / retry | `relay.rs:21-40,2445` | B quota path | FIX L5-05 | plan quota body classified as limit exhaustion; `zcode_plan_quota_body_is_limit_exhaustion` |
| 7 | catalog | `registry/src/lib.rs:1737` | no reference catalogue for the zcode family | NOT-CERTIFIED | zcode family from our snapshot; nothing to compare against |
| 8 | quota | `relay.rs:7257` | no reference quota surface for zcode | NOT-CERTIFIED | limit-exhaustion classification, never zeroed; asserted by our own test, not compared |

### vertex

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `url.rs:5-9` | A `google.ts:954-959` | FIX L2-01 | regional host from `location`; `vertex_location_selects_regional_host` |
| 2 | auth + refresh | `providers/src/vertex.rs` | A google auth | EQUAL | service-account bearer refresh |
| 3 | request translation | `compat/gemini.rs:998-1100`, `:1488-1509` | B `gemini_openai_request.go:174-190` | FIX L2-10 | audio rejected explicitly instead of dropped; `gemini_input_audio_is_rejected` |
| 4 | response translation | `compat/gemini.rs:1620-1660` | B `gemini_openai_response.go:482-496` | FIX L2-03 | thoughts counted once; `compat::gemini::tests::gemini_buffered_usage_includes_thoughts_and_cache` |
| 5 | SSE streaming | `compat/gemini.rs:1766-1900` | B `gemini_openai_response.go:637-680` | FIX L2-02 | shared finish mapping; `compat::gemini::tests::gemini_finish_reason_safety_is_not_success` |
| 6 | failover / retry | `relay.rs:21-40` | A `failover.ts:116-137` | EQUAL | shared |
| 7 | catalog | `registry/src/lib.rs:1737` | A registry snapshot | EQUAL | vertex models from the snapshot |
| 8 | quota | `relay.rs:4222-4252` | no reference surface | NOT-CERTIFIED | no quota headers |

### devin

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `compat/devin.rs:51-110` | B `devin_executor.go:100-105` | EQUAL | `Bearer` normalisation |
| 2 | auth + refresh | `providers/src/devin.rs` | B devin auth | EQUAL | token passthrough |
| 3 | request translation | `compat/devin.rs:137-299`, `interactions.rs:285-320` | B `devin_executor.go:1788-1801` | FIX L6-08 | Interactions API supported instead of 503, image bytes verbatim; `devin_image_user_input_preserves_base64_bytes` |
| 4 | response translation | `compat/devin.rs:524-600` | B decode path | EQUAL | framed data + end-stream |
| 5 | SSE streaming | `interactions.rs:175-195` | B `internal/runtime/executor/devin_executor.go` (stream consume) | FIX L6-08 | previous interaction pairs pending calls; `devin_function_result_continuation_pairs_pending_call` |
| 6 | failover / retry | `relay.rs:21-40` | A `failover.ts:116-137` | EQUAL | shared |
| 7 | catalog | `registry/src/lib.rs:1737` | B model configs | EQUAL + DEVIATION L6-S1 (`behavioralDeviations[L6-S1]`) | devin models from the snapshot |
| 8 | quota | `relay.rs:4251` | no reference surface | NOT-CERTIFIED | default usage, never invented |

### opencode

> Provider-level contract (endpoint path, upstream header set, model identity,
> auth) is **not equal-certified**: neither reference contains a dedicated
> OpenCode inference adapter (`lanes/l6-adapters.md:37`). Cells marked
> `EQUAL (shared)` compare a gateway-wide component against the named reference
> function and say nothing about OpenCode dispatch.

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `providers/src/opencode.rs:5,35` | A registry snapshot lists models, not upstream headers | NOT-CERTIFIED | pinned `opencode/2.2.2` UA; no reference establishes the header set |
| 2 | auth + refresh | `providers/src/opencode.rs` | A `src/chat/inbound.ts:18-40` asserts inbound routing auth, not upstream provider auth | NOT-CERTIFIED | bearer only; no reference establishes the provider auth contract |
| 3 | request translation | shared OpenAI path (`compat/request.rs`) | A `src/chat/inbound.ts:245-300` (`chatCompletionsToResponsesBody`) | EQUAL (shared) | OpenAI-compatible body |
| 4 | response translation | shared `compat/render.rs` | A `src/chat/outbound.ts:162-300` (`responsesSseToChatCompletionsSse`) | EQUAL (shared) | same renderer |
| 5 | SSE streaming | `compat/mod.rs:405-430` (shared parser) | A `src/adapters/openai-chat.ts:1768-1772` (chat stream decode) | EQUAL (shared) | same parser |
| 6 | failover / retry | `relay.rs:21-40` | A `src/combos/failover.ts:116-137` (`parseRetryAfterMs`) | EQUAL (shared) | shared |
| 7 | catalog | `providers/src/opencode.rs:62-67` | A registry snapshot | DEVIATION L6-D3 (`behavioralDeviations[L6-D3]`) | model allowed only when the pinned snapshot lists it |
| 8 | quota | `relay.rs:4252` | A `src/chat/outbound.ts:34-54` (`chatCompletionsUsage`) | EQUAL (shared) | default usage |

### generic (xai / cline / mimo / kimi)

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `compat/xai.rs:227-340`, `compat/mimo.rs:19,75`, `compat/kimi.rs:22-60` | B generic executor | EQUAL | per-family normalisation, encrypted-content validation |
| 2 | auth + refresh | `providers/src/refresh.rs:43-150`, `refresh_exec.rs:50-120` | B `conductor_refresh.go:1578-1599` | FIX L4-01/L4-03 | grant rejection does not bench a working token; upstream body never rendered; `a_status_error_never_renders_the_upstream_body` |
| 3 | request translation | `compat/mimo.rs:5-30`, `compat/kimi.rs:39` | B generic translator | EQUAL | system marker + session affinity |
| 4 | response translation | shared `compat/render.rs` | B `internal/runtime/executor/openai_compat_executor.go:566-624` (stream translate) | EQUAL | same renderer |
| 5 | SSE streaming | `compat/mod.rs:405-430` (shared parser) | B `internal/runtime/executor/openai_compat_executor.go:490,589` | EQUAL | same parser |
| 6 | failover / retry | `relay.rs:2318-2410` | A `src/combos/failover.ts:116-137` | FIX L5-05 | HTTP-date, RFC 850 and asctime `Retry-After`; `cooldown_retry_after_http_date_precedes_reset` |
| 7 | catalog | `registry/src/lib.rs:1737` | A/B catalogs | EQUAL | per-family entries |
| 8 | quota | `relay.rs:2486-2600` | B `sdk/cliproxy/auth/conductor_cooldown.go:2369-2454` | FIX L4-02 | cline cap 429 records the bucket and prefers the named reset over the 24h default; `relay::cline_deadline_tests::precise_reading_still_reanchors` |

### command code (generic adapter `command-code`)

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `compat/command_code.rs` (`command_code_headers`) | A `src/adapters/command-code.ts:502-532` | FIX L6-07 | CLI headers and session id sent on the proprietary endpoint; `command_code_oauth_chat_uses_cli_wire_and_decodes_ndjson` |
| 2 | auth + refresh | `management/oauth.rs:2118-2127` | A `src/adapters/command-code.ts:502-532` | FIX L6-07 | OAuth credential persists the command-code adapter, not generic chat; `command_code_oauth_stream_uses_upstream_override_and_decodes_ndjson` |
| 3 | request translation | `compat/command_code.rs` (`openai_to_command_code`) | A `src/adapters/command-code.ts:502-532` | FIX L6-07 | params/config wrapper with system folded out of the message list; `command_code_oauth_chat_uses_cli_wire_and_decodes_ndjson` |
| 4 | response translation | `compat/command_code.rs` NDJSON decode | A `src/adapters/command-code.ts` | FIX L6-07 | NDJSON frames decoded to chat completions; `command_code_oauth_chat_uses_cli_wire_and_decodes_ndjson` |
| 5 | SSE streaming | `relay.rs:1548-1568` (`Protocol::CommandCode`) | A `src/adapters/command-code.ts` | FIX L6-07 | streaming NDJSON routed through its own protocol decoder; `command_code_oauth_stream_uses_upstream_override_and_decodes_ndjson` |
| 6 | failover / retry | `relay.rs:21-40,2588` | A `src/combos/failover.ts:116-137` | FIX L5-05 | shared Retry-After parsing; `relay::usage_capture_tests::cooldown_retry_after_http_date_precedes_reset` |
| 7 | catalog | `registry/src/lib.rs:1737` | A registry snapshot lists the same model ids | EQUAL | command-code models come from the snapshot |
| 8 | quota | `relay.rs:4230-4272` (`usage_header_prefix` + `capture_usage`) | B `sdk/cliproxy/auth/conductor_cooldown.go:2369-2454` covers shared cooldown capture only | EQUAL (shared) |

### meta / muse (generic adapter `meta`)

> No reference implements Muse. The provider contract (onboarding, wire, model
> identity) is **not equal-certified**; see deviation L6-D4.

| # | concern | our site | reference | verdict | behavior / test |
|---|---|---|---|---|---|
| 1 | upstream headers | `providers/src/meta.rs:13-16` | none: first-party Muse device/OIDC flow absent from A and B | DEVIATION L6-D4 (`behavioralDeviations[L6-D4]`) | pinned Muse build and auth user agents |
| 2 | auth + refresh | `providers/src/meta.rs`, `compat/meta.rs:267-410` | none: A and B have no Muse device-code onboarding | DEVIATION L6-D4 (`behavioralDeviations[L6-D4]`) | device authorization, DCA-to-API-key mint, member refresh |
| 3 | request translation | `compat/meta.rs:5-60` (`prepare_request`) | no reference implements Muse; only the shared Responses shaping is comparable | NOT-CERTIFIED | OpenAI input normalised to Responses, foreign reasoning envelopes stripped; `known_foreign_envelopes_are_removed_without_changing_muse_ciphertext` |
| 4 | response translation | shared `compat/render.rs`, `compat/events.rs` | shared renderers only; no reference Muse decoder | EQUAL (shared) | same Responses renderer; `terminal_output_recovers_indexed_reasoning_without_cross_request_state` |
| 5 | SSE streaming | `relay.rs:1464-1480` (`Protocol::Codex` over Muse SSE) | shared Responses stream only; no reference Muse stream | EQUAL (shared) | Muse streams Responses SSE for both modes; `failed_event_never_becomes_successful_completion` |
| 6 | failover / retry | `relay.rs:21-40,2588` | A `src/combos/failover.ts:116-137` | FIX L5-05 | shared Retry-After parsing; `relay::usage_capture_tests::cooldown_retry_after_http_date_precedes_reset` |
| 7 | catalog | `registry/src/lib.rs:1737` | no reference catalogue lists Muse models | NOT-CERTIFIED | Muse models come from our snapshot; nothing to compare against |
| 8 | quota | `relay.rs:4230-4272` (`usage_header_prefix` + `capture_usage`) | B `sdk/cliproxy/auth/conductor_cooldown.go:2369-2454` covers shared cooldown capture only | EQUAL (shared) |

### Cross-provider defects that belong to no single provider

| concern | our site | reference | verdict | test |
|---|---|---|---|---|
| SSE frame bounds | `compat/events.rs:56-60,209-263` | B `internal/runtime/executor/helps/relay_sse_buffer.go:14-57` | BETTER L5-06 | unterminated line/frame caps; B accumulates unbounded |
| upstream stall budget | `compat/mod.rs:595-601` (budget fn; const at `:96`) | A `src/stall-timeout.ts:1-19` | FIX L5-02 | translated stream expires a stalled upstream |
| idle heartbeats | `compat/mod.rs:743-749` (`STREAM_IDLE_TICK` tick) + `:661-667` (`StreamRenderer::tick`) + `compat/render.rs:337-345` (chat keepalive frame) | A `src/bridge.ts:458-460,1502-1518` | FIX L5-03 + DEVIATION L5-07 (`behavioralDeviations[L5-07]`) | heartbeat keeps the client alive without a fake frame |
| empty-preamble guard | `compat/mod.rs:184-254` | A `responses/empty-completion-guard.ts:119-135` | FIX L5-04 | empty text block no longer counts as served |
| import dedupe | `management/creds.rs` | B `internal/auth/codex/filename.go:9-26` | FIX L4-04 | same account file not duplicated |
| reauth preservation | `management/oauth.rs` | B `auth_files_fields.go:995-1022` | FIX L4-05 | re-login preserves unrelated fields |
| composition lock | `management/oauth.rs:1302-1326` | A `src/oauth/store.ts:196-205` | FIX L4-06 | local writes take the composition lock |
| weighted RR refusal | `management/scalar_table.rs:64-74` (`normalize_routing_strategy` refuses wrr) | B `selector.go:682-714` | FIX L4-08 | refused because no selector implements it; `weighted_round_robin_is_refused_because_no_selector_implements_it` |
| quota header projection | `relay.rs:4230-4272` (`usage_header_prefix` + `capture_usage`) | B `sdk/cliproxy/auth/conductor_cooldown.go:2369-2454` | EQUAL | provider-scoped families; unknown stays unknown rather than zero |
| cache telemetry | `compat/gemini.rs:1846-1859` (`cached_tokens_known`: zero is a measurement, absent is unknown) | B `gemini_openai_response.go:667-669` | BETTER L2-S1 | measured cache-zero kept distinct from absent |
| image detail | `compat/request.rs:422-424` (`detail` forwarded) | B `codex_openai_request.go:234-240` | BETTER L3-S1 | user `image_url.detail` preserved, B drops it |
| claude reset ordering | `claude_reset_order.rs:9,41-60,97-128` | A `src/oauth/account-quota-rank.ts:75-110` | DEVIATION L4-S2 (`docs/reference/provider-parity-deviations.json` `behavioralDeviations[L4-S2]`) | we rank Claude subscription slots by earliest weekly reset and put nearly-spent new-session accounts last; A ranks by measured headroom. Different declared policies, not a payload defect |
| router cursor | `router/src/lib.rs:1-13,154-194` | B `sdk/cliproxy/auth/selector.go:29-36` | DEVIATION L4-S1 (`docs/reference/provider-parity-deviations.json` `behavioralDeviations[L4-S1]`) | member-id last-service-sequence RR instead of previous-picked-identity cursor; both resist positional cursor churn |

## BETTER (keep, no change)

| id | area | ours | reference | why better |
|---|---|---|---|---|
| L2-S1 | gemini cache telemetry | `compat/gemini.rs:1629-1660` records a measured `cachedContentTokenCount: 0` as zero with a known-flag | B `gemini_openai_response.go:667-669` collapses zero and absent into the same value | A provider that reports 0% cache hit is a real signal (cache was offered and missed); printing "unknown" hides a misconfiguration and breaks cache-hit rate arithmetic. Falsification confirmed B loses the distinction. |
| L3-S1 | openai image detail | `compat/request.rs:410-435` (`user_parts`) forwards the user's `image_url.detail` verbatim | B `codex_openai_request.go:234-240` discards it | `detail: high` changes upstream token cost and fidelity; dropping a caller-supplied field silently changes both. Destinations that do not understand it ignore it, so forwarding is strictly safer. |
| L5-06 | sse bounds | `compat/events.rs:56-60,209,258-263` caps an unterminated line at 8 MiB and a frame at 32 MiB, releases the buffers, and errors the stream once | B `internal/runtime/executor/helps/relay_sse_buffer.go:14-57` accumulates an unterminated buffer without bound | An upstream that never emits a newline pins memory until OOM. Our cap converts a hostile or broken upstream into a bounded error; B has no ceiling. |

## DEVIATION (documented, no change)

L1-D1 local count_tokens approximation; L1-D2 unsupported structured output
rejected; L1-D3 later system turns stay in history for cache prefix;
L2-S2 AIStudio worker identity; L3-S2 destination-specific max-token policy;
L4-S1 member-sequence RR; L4-S2 Claude weekly-reset ordering; L5-07 chat
keepalive chunk shape; L6-S1 Devin experimental status; L6-03 cursor local
exec; L6-D2 zcode plan identity; L6-D3 OpenCode catalog binding; L6-D4 Meta
Muse onboarding and wire (no reference counterpart, recorded as uncompared).

All thirteen are declared machine-readably in
`docs/reference/provider-parity-deviations.json` under `behavioralDeviations`,
each with area, our site, reference site, observable behavior, and reason. The
pre-existing `deviations` array (2 registry entries, consumed by
`crates/gateway/tests/t20_reference_parity.rs`) is untouched and that test still
passes.

## Verification notes

- Full pass: `cargo test -p mahoquot-gateway -p mahoquot-providers -p mahoquot-router`
  ran **107 test binaries + 3 doc-test blocks = 110 top-level result blocks**,
  plus one nested child-process result, for **111 result blocks** and **1760
  passed, 0 failed**, ending `EXIT=0` (`/Volumes/T9-Mac/tmp/final10.log`, and
  reproduced on the final restored tree in `final11.log` with the same counts);
  `cargo check --workspace` finished clean in a separate invocation. Earlier
  runs: 1760 (final9, no sentinel), 1759 (final8) and 1756 (final7).
- `tests/t7_refresh.rs::test_t7_concurrent_reactive_401_single_flight_refresh`
  is pre-existing load-sensitive: it passed in four of five full runs and 3/3
  in isolation, and failed once under full-suite parallelism against its own
  5s budget. The file is unmodified by this work.
- Intentional test-expectation updates, each tied to a register row:
  `t10_anthropic_compat` cache-inclusive prompt tokens (L1-01),
  `t25_account_management` import identity precedence (L4-04),
  `review_auth_stream` EOF-without-terminal now errors (L5-01).

## Evidence ledger (C001 / C002 / C003)

C001 — the audit inventory. `defects.md` registers every classified finding
across the twelve provider blocks. Each row carries our site, the governing
reference site (whichever of opencodex / CLIProxyAPI actually implements or
evidences that concern -- a row has two reference cites only where both apply,
one where only one does, and none on NOT-CERTIFIED rows, which claim no parity),
the observed divergence, the chosen remedy, and the verdict vocabulary
(`FIX` / `BETTER` / `DEVIATION` / `EQUAL` / `EQUAL (shared)` /
`NOT-CERTIFIED`). The per-lane detail with file:line citations lives in
`lanes/l1-claude.md` … `lanes/l6-adapters.md`; the thirteen behavioural
deviations are declared machine-readably in
`docs/reference/provider-parity-deviations.json`.

C002 — the red/green proofs. `proofs/mutate-proof.mjs` applies each row's
remedy in reverse, requires the named test to fail, restores the file, and
requires the same test to pass. `proofs/results.json` is the ledger;
`proofs/red-green.md` is the readable summary. **51 of 51 rows are PROVEN**
(`provenTotal: 51, ran: 51, bad: [], incomplete: []`; full 51-row rebuild after the
Gate 11 binding repair, all rows re-executed red then green). Each ledger row is stored
under its item id and carries `{status, test, fixed, mutation, file, package,
sourceHash, testFileHash, testFile, integrationTest}` -- ten keys, where
`fixed` is the SHA-256 of the pre-fix text and `mutation` the SHA-256 of the
fix, and `sourceHash`/`testFileHash` are the content hashes of the mutated
source file and the test file. On every run the harness re-checks all eleven
facts before republishing a row: `status` is still `"PROVEN"`, the old text
still occurs exactly once, both content hashes still match the files on disk,
`fixed` still equals the hash of the table's `old`, `mutation` still equals the
hash of its `new`, and `test`, `file`, `package`, `testFile` and
`integrationTest` all still match the table row. Any mismatch deletes the row. Editing the table, repointing a row at
another test, renaming a file, changing the source file or changing the test
file invalidates it on the next run. Any change to either of those two files'
contents -- including edits outside the mutated fragment -- drops the row, so a
carried-forward PROVEN row does describe the current tree, not just a tree that
once passed. `testFile` is
populated on **all 51 rows** and always names a file that exists: for an
integration target it is `crates/gateway/tests/<name>.rs`; for a `#[cfg(test)]`
module it is the source file that contains that module. Measured over the
ledger: `testFile` equals the mutated file on **44 rows**, and differs on
**7** -- four `#[cfg(test)]` rows whose regression test lives beside the code
under test (`L3-02`/`L3-03` in `compat/render.rs`, `L4-02` in
`pre_expiry_refresh.rs`, `L4-08` in `management/scalars.rs`) and three
integration-target rows whose test file is `crates/gateway/tests/<name>.rs`
(`L3-09` in `generic_chat_responses.rs`, `L1-10`/`L1-11` in
`t12_provider_relay.rs`). The
cargo invocation uses a separate `integrationTest` field, because only
integration targets take `--test`; a lib target is reached with `--lib` at the
module path recorded in `test`. `testFile` is required on every row and is
bound independently. `integrationTest` is optional in the table, but the key is written on **all
51** ledger rows: non-null on the three integration-target rows and `null` on
the 48 lib rows, which run with `--lib`. When non-null it is bound like any
other identity field, and a load-time check rejects a table whose `testFile`
and `integrationTest` disagree.

Two accounting facts that are deliberate, not gaps. First, `L6-03` has no
ledger row because it is **RECLASSIFIED as DEVIATION**, not fixed: local
execution is the documented default of this gateway, so inventing a "fix" for
it would misdescribe our behaviour. Its hardening counterpart is `L6-EXEC`,
which is proven red/green and declares `hardening: true` in
`proofs/mutations.json` because it is not a defect against either reference.
The register therefore holds 50 `fixed` rows + 1 reclassified + 1 hardening =
51 proofs. Second, this relation is no longer prose: a non-`--only` run of
`mutate-proof.mjs` throws unless every table id names a register row (or is a
declared hardening entry) **and** every register row whose outcome is `fixed`
has a table entry, so no future edit can silently break the correspondence in
either direction.

C003 — the suite. `cargo test -p mahoquot-gateway -p mahoquot-providers
-p mahoquot-router` ran **107 test binaries** (`Running` entries: 4 unit-test, 103 integration) and
**3 doc-test blocks**, which is 110 top-level result blocks, plus one nested
child-process result emitted inside the gateway-lib block = **111 result blocks**.
The sums are **1760 passed, 0 failed**, and the log ends with the recorded
sentinel `EXIT=0` (`/Volumes/T9-Mac/tmp/final10.log`, produced by the command
above with `> final10.log 2>&1; echo EXIT=$?`). `cargo check --workspace` was run
separately and was clean; its output is not part of that log. The three L1-09/L1-10/L1-11 cache-cap
behaviours are covered by `t12_provider_relay` (`-- breakpoint` → 2 passed).

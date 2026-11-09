# r1-claude repair

Owned source: `crates/gateway/src/compat/claude.rs` (all ranges below refer to this file).

| ID | Changed ranges | Repair / regression |
| --- | --- | --- |
| L1-01 | 219-225, 1274-1278; tests 1800-1848, 2837-2915 | Canonical JSON/stream prompt includes raw input + cache read + cache write; totals include output. Raw counters/details unchanged. Added `anthropic_cache_usage_is_inclusive_json_and_stream` including actual chat chunk rendering. |
| L1-02 | 1409, 1466, 1528-1545, 1724; tests 1851-1918 | Shared boundary usage conversion emits exclusive input and known cache fields, preserving known zero/unknown absence and saturating subtraction. Covers buffered JSON, buffered stream snapshot, live stream terminal. Added `anthropic_render_usage_excludes_known_caches`. |
| L1-03 | 476-478, 974-978; tests 1921-1944 | Maps string/array stop to stop_sequences; maps native stop_sequences to stop; null ignored and legacy fallback preserved. Added `claude_stop_controls_cross_map`. |
| L1-04 | 908; tests 1947-1957 | max_tokens precedes max_completion_tokens, then 32768 default. Added `claude_honors_max_completion_tokens`. |
| L1-05 | 329-333, 388-392; tests 1960-1975 | Converts URL images in ordinary user content and tool-result content to image_url. Added `anthropic_url_images_survive_chat_translation`. |
| L1-06 | 425, removed old trailing emission; tests 1978-1995 | Emits tool results before companion user follow-up; removed unused has_tool_results. Added `anthropic_tool_results_precede_user_followup`. |
| L1-07 | 479-496; tests 1998-2025 | Preserves disabled, explicit effort, budget-derived effort, and adaptive/enabled auto controls. Added `anthropic_thinking_controls_survive_codex_plan`, including final Codex reasoning.effort assertions. |
| L1-08 | 822-839, 982-1010; tests 2028-2070 | Prepends signed Claude reasoning before text/tools; keeps thinking-only messages; replays explicit redacted marker data. Plausibility check recognizes decoded Claude envelope and rejects unsigned/foreign placeholders; preserves opaque signature bytes. Added `claude_replays_signed_assistant_reasoning_before_tools` using captured fable-5 signature via Responses->chat->Claude, plus unsigned/foreign negatives and thinking-only/redacted checks. |
| L1-09 | unchanged | Deliberately not implemented: documented explicit-cache policy deviation per falsification report and task instructions. |

Existing tests updated:
- `relay_usage_reported_only_on_message_delta_is_kept`: prompt now `3 + 12705 + 9`.
- `anthropic_usage_with_cache_creation_input_tokens_parses_into_cache_write_tokens`: prompt 200, total 225; added JSON prompt 200 assertion.

Verification: `rustfmt --edition 2021 --check crates/gateway/src/compat/claude.rs` and `git diff --check -- crates/gateway/src/compat/claude.rs` both passed. No cargo/build/test execution or provider network requests were performed, as required; root session must compile and run the eight new regressions and updated existing tests. Other lanes' concurrently changing source was read only when needed to check integration API shape; no other source files were edited.

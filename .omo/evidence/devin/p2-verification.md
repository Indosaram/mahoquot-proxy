# Devin P2 — Wire Codec Independent Verification Report

**Task ID:** `st_01a09312` (Independent Verification)  
**Verifier:** hephaestus (child task)  
**Parent Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Root Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Model:** `mahoquot/gemini-3.8-flash-high` (sole verifier, no subdelegation)  
**Date:** 2026-09-12  
**Target Deliverable:** `/Users/indo/code/project/mahoquot-proxy/.omo/evidence/devin/p2-verification.md`  
**Plan Reference:** Plan P2 in `/Users/indo/code/project/mahoquot-proxy/.omo/plans/devin-provider-integration.md`  
**Producer Report Reference:** `/Users/indo/code/project/mahoquot-proxy/.omo/evidence/devin/p2-codec.md`  

---

## 1. Executive Summary & Verification Verdict

### Verification Verdict: PASS (ALL PROTOCOL REQUIREMENTS & REGRESSIONS VERIFIED)
- **Wire Codec Suite (`devin_wire`):** **PASS** (40/40 tests pass, Exit Code: `0`)
- **Golden Fixtures Suite (`devin_wire_fixtures`):** **PASS** (15/15 tests pass, Exit Code: `0`)
- **Gateway Compilation (`cargo check -p mahoquot-gateway`):** **PASS** (Exit Code: `0`)
- **Shared Renderer Regressions (`provider_finish_contracts`):** **PASS** (6/6 tests pass, Exit Code: `0`)
- **Compat Unit Suite (`--lib compat`):** **PASS** (89/89 tests pass, Exit Code: `0`)
- **Shared Renderer Tests (`compat::render`):** **PASS** (7/7 tests pass, Exit Code: `0`)
- **Devin Bounded Allocation Unit Seam (`compat::devin`):** **PASS** (1/1 tests pass, Exit Code: `0`)
- **Final Acceptance:** Remains with the gateway lead.

### Summary of Verified Remediations
1. **Output Limit Precedence Over Partial Tool Calls (`provider_finish_contracts`):**
   In `crates/gateway/src/compat/render.rs`, `GeminiChunkRenderer::close_open_calls` distinguishes incomplete tool calls caused by reaching output limits (`self.output_limit_reached`) from malformed tool calls in successful completions. When `output_limit_reached` is true, partial/unparseable tool calls are omitted from output without triggering a `400 INVALID_ARGUMENT` error frame, and the stream terminates cleanly with `finishReason: "MAX_TOKENS"`. When `output_limit_reached` is false, strict `INVALID_ARGUMENT` rejection is preserved. All 6 tests in `provider_finish_contracts.rs` pass unmodified.

2. **Interleaved Tool Calls & Streaming Text in `GeminiChunkRenderer`:**
   In `crates/gateway/src/compat/render.rs`, premature calls to `close_open_calls` in `CodexEvent::TextDelta` and `CodexEvent::ToolCallBegin` have been removed. Unrelated text deltas stream immediately without buffering or interfering with active tool calls. Tool calls are tracked per-index across interleaved begins, arguments, and text until terminal stream closure, where each complete call is validated as a JSON object and emitted once with original IDs and names. Verified via new regression `defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args` and updated `defect_4d`.

3. **Deterministic Error Handling & Secret Redaction (No Heuristics):**
   The heuristic `redact_sensitive` regex scanner was removed from `devin.rs`. Connect error codes are normalized against a finite standard set (`normalize_connect_code`), and safe local descriptions (`default_code_description`) are emitted downstream. Upstream error messages and remote image URLs (with query parameters, path fragments, or embedded userinfo) are never reflected. `DevinRequestParams` custom `Debug` formatting redacts tokens to `"[REDACTED]"`.

4. **Root Completion Lifecycle & Idempotent Termination:**
   `DevinDecoder::decode_end_stream` processes the terminal frame, records outcome metadata, and sets `outcome.terminated = true`, but **never** emits `CodexEvent::Completed`. `Completed` is emitted exclusively inside `DevinDecoder::finish` upon verifying EOF with zero trailing bytes. Trailing bytes in the same chunk or subsequent chunks fail the stream immediately. The temporary Devin-only deferral wrapper was cleanly removed from `crates/gateway/src/compat/mod.rs`.

5. **Opaque Tool IDs & Resolution Invariants:**
   Numeric suffix parsing (`tool_index_from_id`) was removed. Tool block indices are allocated monotonically. `ToolCallBegin` is deferred until both `id` and `name` are present, buffering initial argument deltas. Ambiguous ID-less tool deltas across multiple calls fail explicitly, and missing tool IDs or names at terminal fail instead of emitting empty strings.

6. **Bounded Buffer Allocation:**
   Oversized frames (> 16 MiB) fail immediately upon inspecting the 5-byte header, without allocating buffer capacity or accumulating payload bytes. Verified via private unit seam test `test_bounded_allocation_private_unit_seam` asserting buffer capacity `< 1 MiB` and retained length `<= 5 bytes`.

7. **Discovery Capability Schema Fields:**
   `ClientModelConfig` in `crates/gateway/src/compat/devin_proto.rs` includes pinned capability fields 10 (`provider: Option<i32>`), 11 (`is_recommended: Option<bool>`), 15 (`is_new: Option<bool>`), and 20 (`is_capacity_limited: Option<bool>`).

---

## 2. Independent Command Execution & Exit Codes

All commands were executed directly from `/Users/indo/code/project/mahoquot-proxy`:

| Command | Exit Code | Verification Status | Exact Result |
|---|---|---|---|
| `cargo check -p mahoquot-gateway` | **0** | **PASSED** | 0 warnings, 0 errors |
| `cargo test -p mahoquot-gateway --test devin_wire` | **0** | **PASSED** | 40 passed, 0 failed, 0 ignored |
| `cargo test -p mahoquot-gateway --test devin_wire_fixtures` | **0** | **PASSED** | 15 passed, 0 failed, 0 ignored |
| `cargo test -p mahoquot-gateway --test provider_finish_contracts` | **0** | **PASSED** | 6 passed, 0 failed, 0 ignored |
| `cargo test -p mahoquot-gateway compat::render` | **0** | **PASSED** | 7 passed, 0 failed, 315 filtered out |
| `cargo test -p mahoquot-gateway compat::devin` | **0** | **PASSED** | 1 passed, 0 failed, 321 filtered out |
| `cargo test -p mahoquot-gateway --lib compat` | **0** | **PASSED** | 89 passed, 0 failed, 233 filtered out |
| `cargo test -p mahoquot-gateway --test devin_credentials` | **0** | **PASSED** | 16 passed, 0 failed, 0 ignored |

---

## 3. LSP Diagnostics & Path Reporting

As required by lead review instructions, LSP diagnostics were evaluated for both logical and physical paths:
- **Logical Path (`/Users/indo/code/project/mahoquot-proxy/crates/gateway/src/compat/render.rs`):**  
  `lsp_diagnostics` reported `file not found` due to the workstation symlink (`mahoquot-proxy -> /Volumes/T9-Mac/project/mahoquot-proxy`).
- **Physical Path (`/Volumes/T9-Mac/project/mahoquot-proxy/crates/gateway/src/compat/render.rs`):**  
  `lsp_diagnostics` returned **`No diagnostics found`** (0 errors, 0 warnings).
- **Physical Path (`.../compat/devin.rs`):** `No diagnostics found`.
- **Physical Path (`.../compat/devin_proto.rs`):** `No diagnostics found`.
- **Physical Path (`.../compat/mod.rs`):** `No diagnostics found`.

---

## 4. Exact Command Outputs

### 4.1. Wire Codec Suite: `cargo test -p mahoquot-gateway --test devin_wire`
```text
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.28s
     Running tests/devin_wire.rs (target/debug/deps/devin_wire-3f5d258a23f67c59)

running 40 tests
test defect_5_response_format_strict_and_json_schema_are_rejected ... ok
test devin_binary_stream_passes_open_stream_protocol_gate ... ok
test defect_1_debug_and_errors_redact_secrets_and_credentials ... ok
test absent_options_use_reference_defaults ... ok
test defect_3_terminated_stream_rejects_subsequent_chunks_and_trailing_data_never_emits_completed ... ok
test forced_tool_choice_n_1_strict_output_are_rejected ... ok
test duplicate_terminal_and_trailing_data_are_rejected ... ok
test full_history_preserves_roles_tool_reasoning_signature_and_images ... ok
test ambiguous_idless_tool_delta_fails_but_single_tool_attaches ... ok
test devin_stream_aggregates_into_chat_completion_with_tools ... ok
test code_only_terminal_error_fails_with_code_preserved ... ok
test interleaved_tool_indices_are_stable_across_text_and_reasoning ... ok
test malformed_proto_and_malformed_endstream_json_fail ... ok
test metadata_keeps_api_key_and_literal_header_contract ... ok
test missing_endstream_at_every_frame_boundary_fails ... ok
test oversize_frame_is_rejected_without_buffering ... ok
test malformed_final_arguments_are_never_repaired_to_empty_object ... ok
test minimal_request_matches_independent_wire_vector ... ok
test outcome_reports_stop_reason_and_error_metadata ... ok
test korean_text_survives_every_chunk_split ... ok
test output_limit_stop_reasons_surface_output_limit_reached ... ok
test output_limits_temperature_and_top_p_are_preserved ... ok
test defect_2_oversized_frame_header_rejected_before_allocating_payload_or_appending_chunk ... ok
test defect_6_malformed_tool_arguments_preserved_or_rejected_across_all_render_paths ... ok
test tool_result_without_call_id_is_rejected ... ok
test every_chunk_split_produces_identical_events ... ok
test remote_images_and_non_vision_models_are_rejected_upstream_of_wire ... ok
test response_encoding_matches_pinned_field_numbers ... ok
test schema_descriptions_and_definitions_are_not_stripped ... ok
test stop_frame_alone_does_not_emit_success_until_valid_endstream ... ok
test defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args ... ok
test tool_begin_is_delayed_until_name_is_known ... ok
test stream_decodes_thinking_signature_redaction_text_tool_and_usage ... ok
test unnegotiated_compression_flags_are_rejected ... ok
test unary_model_request_has_no_connect_envelope ... ok
test truncated_frame_fails_at_eof ... ok
test tool_choice_none_removes_tools_but_auto_keeps_them ... ok
test defect_4_delayed_id_and_name_resolution_and_interleaved_tools_with_renderers ... ok
test usage_is_none_when_absent_and_final_snapshot_wins_without_double_count ... ok
test defect_3_compat_stream_rejects_same_chunk_and_split_terminal_junk ... ok

test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

### 4.2. Golden Fixtures Suite: `cargo test -p mahoquot-gateway --test devin_wire_fixtures`
```text
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.18s
     Running tests/devin_wire_fixtures.rs (target/debug/deps/devin_wire_fixtures-e7267d5fae0f7367)

running 15 tests
test malformed_utf8_rejected_negative_fixture ... ok
test terminal_code_only_error ... ok
test ambiguous_tool_calls_rejected_negative_fixture ... ok
test proto_parseable_invalid_utf8_rejected_negative_case ... ok
test reasoning_signature_and_redaction ... ok
test unary_unframed_request_and_response ... ok
test interleaved_tool_frames_across_frames ... ok
test actual_model_uid_contract ... ok
test usage_and_cache_snapshot ... ok
test literal_basic_dummy_token_header ... ok
test korean_utf8_complete_strings_and_arbitrary_transport_splits ... ok
test chat_framed_request ... ok
test chat_framed_response_stream ... ok
test all_positive_fixtures_have_valid_utf8_strings ... ok
test tag_checks_length_semantics_normalized ... ok

test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

### 4.3. Shared Finish Regressions: `cargo test -p mahoquot-gateway --test provider_finish_contracts`
```text
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.23s
     Running tests/provider_finish_contracts.rs (target/debug/deps/provider_finish_contracts-b362a539666d9d13)

running 6 tests
test anthropic_json_output_limit_is_length ... ok
test other_codex_incomplete_reasons_remain_failures ... ok
test codex_output_limit_is_not_an_upstream_failure ... ok
test anthropic_output_limit_survives_protocol_conversion ... ok
test output_limit_takes_precedence_over_partial_tool_calls ... ok
test antigravity_output_limit_survives_protocol_conversion ... ok

test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

### 4.4. Devin Private Unit Seam: `cargo test -p mahoquot-gateway compat::devin`
```text
     Running unittests src/lib.rs (target/debug/deps/mahoquot_gateway-dee0e90e6f9cfa8b)

running 1 test
test compat::devin::tests::test_bounded_allocation_private_unit_seam ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 321 filtered out; finished in 0.00s
```

### 4.5. Shared Renderer Suite: `cargo test -p mahoquot-gateway compat::render`
```text
     Running unittests src/lib.rs (target/debug/deps/mahoquot_gateway-dee0e90e6f9cfa8b)

running 7 tests
test compat::render::gemini_stream_tests::reasoning_signature_is_emitted_as_thought_signature_part ... ok
test compat::render::gemini_stream_tests::created_event_overrides_response_id ... ok
test compat::render::openai_stream_tests::reasoning_deltas_forward_as_reasoning_content ... ok
test compat::render::gemini_stream_tests::text_deltas_stream_immediately_and_usage_rides_the_terminal_frame ... ok
test compat::render::gemini_stream_tests::terminal_frame_sets_stop_and_stream_has_no_done_sentinel ... ok
test compat::render::gemini_stream_tests::non_streaming_gemini_carries_tool_calls_and_a_matching_finish_reason ... ok
test compat::render::gemini_stream_tests::tool_calls_stream_as_function_call_parts_with_restored_arguments ... ok

test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 315 filtered out; finished in 0.02s
```

### 4.6. Gateway Compilation Check: `cargo check -p mahoquot-gateway`
```text
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.34s
```

---

## 5. Detailed Requirement & Coverage Audit Matrix

| Requirement Area | Implementation Source Reference | Verification Test Coverage | Verification Result |
|---|---|---|---|
| **1. Chunk split robustness** | `DevinDecoder::decode` (`devin.rs:524-573`) splits and reassembles across 5-byte headers & arbitrarily chunked payload boundaries. | `korean_text_survives_every_chunk_split`, `every_chunk_split_produces_identical_events`, `korean_utf8_complete_strings_and_arbitrary_transport_splits`. | **PASS**: Multibyte Korean text and mixed content identical across all 1-byte, 2-byte, and random chunk splits. |
| **2. Bounded allocation (< 16 MiB)** | `devin.rs:541-547` checks frame length before buffer allocation. | `defect_2_oversized_frame_header_rejected_before_allocating_payload_or_appending_chunk`, `compat::devin::tests::test_bounded_allocation_private_unit_seam`. | **PASS**: 20 MiB header with 512 KiB payload fails immediately; buffer capacity `< 1 MiB`, retained len `<= 5`. |
| **3. Stream completion lifecycle** | `decode_end_stream` (`devin.rs:770-801`) sets `outcome.terminated = true`; `finish` (`devin.rs:575-620`) emits `Completed` strictly on clean EOF. | `stop_frame_alone_does_not_emit_success_until_valid_endstream`, `missing_endstream_at_every_frame_boundary_fails`, `defect_3_terminated_stream_...`, `defect_3_compat_stream_...`. | **PASS**: No premature `Completed`. Trailing data in same chunk or subsequent chunks fails stream immediately; no success emitted. |
| **4. Redaction & safe errors** | `devin.rs:59-71` (`DevinRequestParams` Debug redacting `token`), `normalize_connect_code` (`devin.rs:74-94`), `default_code_description` (`devin.rs:98-119`). | `defect_1_debug_and_errors_redact_secrets_and_credentials`, `code_only_terminal_error_fails_with_code_preserved`, `terminal_code_only_error`. | **PASS**: Arbitrary upstream messages and URL parameters are never reflected. Codes normalized to finite standard set. Token redacted in Debug. |
| **5. Opaque tool IDs & delayed begin** | `next_block` monotonic counter (`devin.rs:509`), `decode_tool_delta` (`devin.rs:693-768`) delays `ToolCallBegin` until both ID and name are resolved. | `tool_begin_is_delayed_until_name_is_known`, `defect_4_delayed_id_and_name_resolution_and_interleaved_tools_with_renderers` (4a, 4b, 4c). | **PASS**: Monotonic index assignment without numeric suffix guessing; args buffered until begin; missing ID/name at stream end fails. |
| **6. Ambiguous tool correlation** | `devin.rs:709-717`, `726-731` fails on multiple unresolved tools missing ID or ID-less deltas with multiple tools. | `ambiguous_idless_tool_delta_fails_but_single_tool_attaches`, `defect_4` (4e), `ambiguous_tool_calls_rejected_negative_fixture`. | **PASS**: Unresolvable tool deltas strictly fail and never complete. |
| **7. Gemini functionCall argument integrity** | `GeminiChunkRenderer::close_open_calls` (`render.rs:85-115`) validates parsed JSON object, rejecting non-object or malformed JSON with 400 INVALID_ARGUMENT. | `defect_6_malformed_tool_arguments_preserved_or_rejected_across_all_render_paths`, `malformed_final_arguments_are_never_repaired_to_empty_object`. | **PASS**: Malformed JSON and valid non-object JSON (`null`, array, string) fail with structured INVALID_ARGUMENT error; never repaired to `{}`. |
| **8. Output limit precedence** | `render.rs:99-104` checks `self.output_limit_reached`; omits truncated partial calls without error frames; emits `finishReason: "MAX_TOKENS"`. | `crates/gateway/tests/provider_finish_contracts.rs` (`output_limit_takes_precedence_over_partial_tool_calls`). | **PASS**: Incomplete calls on max tokens do not emit invalid functionCall args or error frames; finishReason is MAX_TOKENS; usage is preserved. |
| **9. Gemini interleaved tool calls & streaming text** | `render.rs:139` (`TextDelta` streams without closing tools); `render.rs:154-169` (`ToolCallBegin` maintains per-index accumulators); closure at terminal. | `defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args`, `defect_4` (4d). | **PASS**: Text deltas stream immediately; interleaved tools maintain state; candidate functionCall parts contain exact IDs, names, and arguments. |
| **10. Reasoning, signatures & redaction** | `ChatMessageResponse` tags 9, 10, 11 decoded to `ReasoningDelta`, `ReasoningSignature`, `ReasoningRedacted` (`devin.rs:645-655`). | `stream_decodes_thinking_signature_redaction_text_tool_and_usage`, `reasoning_signature_and_redaction`, `full_history_preserves_...`. | **PASS**: Thinking text, signatures, and redaction flags decode and render correctly across OpenAI and Gemini wire. |
| **11. Authoritative usage snapshots** | `devin.rs:662-674` captures final usage snapshot without summing; tracks cache read/write tokens separately. | `usage_is_none_when_absent_and_final_snapshot_wins_without_double_count`, `usage_and_cache_snapshot`. | **PASS**: Final snapshot wins; absent usage yields `None`; no double counting of cache tokens. |
| **12. Discovery capability schema fidelity** | `devin_proto.rs:219-234` restores `ClientModelConfig` fields 10 (`provider`), 11 (`is_recommended`), 15 (`is_new`), 20 (`is_capacity_limited`). | `actual_model_uid_contract`, `response_encoding_matches_pinned_field_numbers`. | **PASS**: Pinned protobuf contract matches field definitions; P4 capability filtering supported. |

---

## 6. Verification Conclusion & Recommendations

The Devin P2 wire codec and shared renderer integration are **fully verified** against all Plan P2 requirements, lead directives, and existing regression test suites.

- **Defects Identified:** None remaining.
- **Breaking Regressions:** Zero regressions. All 6 tests in `provider_finish_contracts.rs` pass unmodified.
- **Compiler/LSP Cleanliness:** Clean compilation and zero LSP diagnostics on physical source paths.
- **Deliverable Status:** Deliverable `/Users/indo/code/project/mahoquot-proxy/.omo/evidence/devin/p2-verification.md` has been successfully updated with independent execution evidence and analysis.
- **Final Acceptance:** Recommended for final acceptance by the gateway lead.

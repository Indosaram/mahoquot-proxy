# Devin P2 — Request Builder & Strict Connect Response Decoder Evidence

**Task ID:** `st_01a092f7` (Final Renderer Remediation & Full Integration Acceptance)  
**Agent:** hephaestus (child task)  
**Parent Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Root Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Model:** `mahoquot/gemini-3.8-flash-high` (sole implementation owner, no subdelegation)  
**Date:** 2026-09-12  
**Plan Reference:** Plan P2 in `/Users/indo/code/project/mahoquot-proxy/.omo/plans/devin-provider-integration.md`  

---

## 1. Executive Summary & Scope Audit

The requirements of Plan P2, the initial six lead review defect remediations, and the two subsequent renderer defect corrections (`output_limit_takes_precedence_over_partial_tool_calls` in `provider_finish_contracts` and interleaved tool/text streaming in `GeminiChunkRenderer`) have been completely implemented, verified with failing-first evidence, and validated against the entire regression suite.

### Summary of Completed Remediations

1. **Defect 1: Safe Deterministic Error Handling & Secret Redaction (No Heuristics):**
   - Removed the heuristic `redact_sensitive` scanner.
   - Remote image URL rejection in `image_parts` emits the fixed description `"remote image url is unsupported"` without echoing URL path, query parameters, fragments, or userinfo credentials.
   - Connect error codes from upstream are normalized via `normalize_connect_code` against the finite standard set (`canceled`, `unknown`, `invalid_argument`, `deadline_exceeded`, `not_found`, `already_exists`, `permission_denied`, `resource_exhausted`, `failed_precondition`, `aborted`, `out_of_range`, `unimplemented`, `internal`, `unavailable`, `data_loss`, `unauthenticated`), falling back to `"unknown"`.
   - Upstream error messages are never reflected into downstream events or outcomes; safe local fixed descriptions (`default_code_description`) are emitted instead. Recognized typed error codes are preserved in `DevinOutcome::error_code` for downstream relay feedback.
   - `DevinRequestParams` custom `Debug` formatting securely redacts session tokens as `"[REDACTED]"`.

2. **Defect 2 & Defect 3: Root Completion Lifecycle & Idempotent Termination:**
   - Corrected stream completion at the `DevinDecoder` root: `decode_end_stream` validates the terminal JSON object, captures metadata, and flags `outcome.terminated = true`, but **never** emits `CodexEvent::Completed`.
   - `CodexEvent::Completed` is emitted exclusively inside `DevinDecoder::finish` once EOF confirms that zero trailing bytes or extraneous chunks exist in the stream.
   - `finish` is strictly terminal and idempotent via `finished: bool`.
   - The temporary Devin-only deferral wrapper (`defer_completed`, `deferred_completed`) was completely removed from `crates/gateway/src/compat/mod.rs` (`TranslateState`, `streaming_body`, `handle_stream_event`), restoring clean general streaming behavior where normal text and tool deltas stream immediately without buffering.
   - Any trailing data arriving in the same chunk after EndStream or in subsequent chunks terminates the stream with `CodexEvent::Failed`, never emitting `Completed` or successful `finish_reason: "stop"`.

3. **Defect 4: Opaque Tool IDs, Monotonic Indices, and Delay Invariants:**
   - Removed `tool_index_from_id` numeric suffix parsing. Tool IDs are strictly treated as opaque identifiers.
   - Tool block indices are assigned via unique monotonically increasing counters (`self.next_block`).
   - `ToolCallBegin` is deferred until both `call_id` and `name` are resolved. Tool arguments arriving before begin are buffered and flushed as `ToolArgsDelta` in correct sequence.
   - Positive tool interleaving is supported with explicit tool IDs across frames and delayed names.
   - Ambiguous tool correlation (e.g. multiple unresolved tools without ID, or ID-less deltas with multiple active tools) is rejected.
   - Stream finish strictly validates that every tool has both `call_id` and `name`, failing with `unresolved tool missing ID or name at stream end` instead of emitting `name: ""`.

4. **Defect 5: Unsupported Parameter Rejection:**
   - `build_chat_request` explicitly validates `response_format`. Requests specifying `type: "json_schema"`, `strict: true`, or nested `json_schema: { "strict": true }` are rejected with descriptive errors.

5. **Defect 6: Gemini Renderer Argument Integrity:**
   - In `GeminiChunkRenderer::close_open_calls`, malformed JSON arguments as well as valid non-object JSON (`null`, array, string, number, bool) surface a structured failure (`{"error": {"code": 400, "message": ..., "status": "INVALID_ARGUMENT"}}`) and terminate the stream, never repairing invalid arguments to `{}` or emitting string args into `functionCall.args`.
   - Raw argument strings are preserved verbatim for OpenAI aggregators and chunk renderers where allowed.

6. **Defect 7: Output Limit Precedence Over Partial Tool Calls (`provider_finish_contracts`):**
   - In `GeminiChunkRenderer::close_open_calls`, incomplete tools whose arguments cannot be parsed as JSON objects are distinguished by checking `self.output_limit_reached`.
   - When `output_limit_reached` is true, truncated partial tool calls are dropped without emitting invalid `functionCall` arguments as success and without emitting `INVALID_ARGUMENT` error frames.
   - The stream terminates cleanly with `finishReason: "MAX_TOKENS"` and preserves final usage statistics.
   - When `output_limit_reached` is false, malformed tool arguments retain strict `INVALID_ARGUMENT` failure behavior.

7. **Defect 8: Interleaved Tool Calls & Streaming Text in `GeminiChunkRenderer`:**
   - Removed premature calls to `close_open_calls` from `CodexEvent::TextDelta` and `CodexEvent::ToolCallBegin`.
   - `TextDelta` frames stream immediately to ensure time-to-first-token without affecting in-progress tool state.
   - Tool calls are maintained per-index in `open_calls` throughout the stream until terminal completion.
   - At terminal closure (`Completed` or `close_unterminated`), each valid complete tool call is parsed and emitted once with its original `id` and `name`.
   - Strengthened test coverage in `devin_wire.rs`: updated `defect_4d` to assert parsed Gemini SSE frames for both IDs and exact arguments, and added `defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args` testing interleaved tool begins, partial arg deltas, interleaved text deltas, and arg completion.

8. **Buffer Allocation Bounds & Completeness:**
   - Memory allocation bounds are strictly enforced prior to appending frame bytes. An oversized frame header (`> 16 MiB`) fails immediately after consuming the 5-byte header without allocating or buffering the payload.
   - A private unit seam test in `devin.rs` (`test_bounded_allocation_private_unit_seam`) verifies that buffer capacity remains bounded (`< 1 MiB`) and retained length does not exceed 5 bytes when presented with an oversized header and a 512 KiB payload.
   - Pinned schema contract honored in `crates/gateway/src/compat/devin_proto.rs`: `ClientModelConfig` includes discovery capability fields 10 (`provider: Option<i32>`), 11 (`is_recommended: Option<bool>`), 15 (`is_new: Option<bool>`), and 20 (`is_capacity_limited: Option<bool>`), as well as enum module `model_provider`.

### Scope Enforcement Audit
- **Files Modified (Strictly within assigned P2 boundary):**
  - `crates/gateway/src/compat/render.rs` (Gemini chunk renderer interleaved tool & output limit handling)
  - `crates/gateway/tests/devin_wire.rs` (interleaving regression test & parsed SSE assertions)
  - `.omo/evidence/devin/p2-codec.md` (this updated report)
- **Pre-existing P2 Files Preserved:**
  - `crates/gateway/src/compat/devin_proto.rs`
  - `crates/gateway/src/compat/devin.rs`
  - `crates/gateway/src/compat/mod.rs`
  - `crates/gateway/src/compat/events.rs`
  - `crates/gateway/src/compat/claude.rs`
- **Untouched Out-of-Scope Files:**
  - `Cargo.toml`, `Cargo.lock` untouched.
  - `crates/gateway/tests/provider_finish_contracts.rs` (read-only regression suite) untouched.
  - `devin.proto` and `tests/data/devin/` (P0 owned) untouched.
  - Gateway files outside compat (`account.rs`, `management/creds.rs`, etc.) untouched.
  - Provider and registry crates untouched (concurrent P1 ownership).
  - Pre-existing user and parallel worker edits preserved; zero git commits.

---

## 2. Failing-First (RED) Evidence

### 1. `output_limit_takes_precedence_over_partial_tool_calls` Failure
Before fixing `render.rs`, running `cargo test -p mahoquot-gateway --test provider_finish_contracts` captured the following panic:

```text
running 6 tests
test anthropic_json_output_limit_is_length ... ok
test other_codex_incomplete_reasons_remain_failures ... ok
test codex_output_limit_is_not_an_upstream_failure ... ok
test anthropic_output_limit_survives_protocol_conversion ... ok
test antigravity_output_limit_survives_protocol_conversion ... ok
test output_limit_takes_precedence_over_partial_tool_calls ... FAILED

failures:

---- output_limit_takes_precedence_over_partial_tool_calls stdout ----

thread 'output_limit_takes_precedence_over_partial_tool_calls' (14973420) panicked at crates/gateway/tests/provider_finish_contracts.rs:41:5:
data: {"error":{"code":400,"message":"invalid functionCall arguments for tool 'lookup': must be a JSON object","status":"INVALID_ARGUMENT"}}

failures:
    output_limit_takes_precedence_over_partial_tool_calls

test result: FAILED. 5 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

### 2. `defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args` Failure
Before fixing `render.rs`, adding the faithful interleaved regression test produced this failure:

```text
running 1 test
test defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args ... FAILED

failures:

---- defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args stdout ----

thread 'defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args' (14994519) panicked at crates/gateway/tests/devin_wire.rs:1458:5:
Gemini stream must not contain error frames for valid interleaved tools: [Object {"candidates": Array [Object {"content": Object {"parts": Array [Object {"text": String("thinking about city... ")}], "role": String("model")}}], "modelVersion": String("devin/glm-x"), "responseId": String("resp-1")}, Object {"error": Object {"code": Number(400), "message": String("invalid functionCall arguments for tool 'func_a': must be a JSON object"), "status": String("INVALID_ARGUMENT")}}]

failures:
    defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 39 filtered out; finished in 0.00s
```

---

## 3. Passing Verification (GREEN) Evidence

### 1. Named Wire Regression Suite (`devin_wire`) & `provider_finish_contracts`
```bash
cargo test -p mahoquot-gateway --test devin_wire --test provider_finish_contracts
```
**Exit Code:** `0`  
**Output:**
```text
     Running tests/devin_wire.rs (target/debug/deps/devin_wire-3f5d258a23f67c59)

running 40 tests
test devin_binary_stream_passes_open_stream_protocol_gate ... ok
test defect_5_response_format_strict_and_json_schema_are_rejected ... ok
test absent_options_use_reference_defaults ... ok
test forced_tool_choice_n_1_strict_output_are_rejected ... ok
test defect_3_terminated_stream_rejects_subsequent_chunks_and_trailing_data_never_emits_completed ... ok
test code_only_terminal_error_fails_with_code_preserved ... ok
test duplicate_terminal_and_trailing_data_are_rejected ... ok
test full_history_preserves_roles_tool_reasoning_signature_and_images ... ok
test interleaved_tool_indices_are_stable_across_text_and_reasoning ... ok
test devin_stream_aggregates_into_chat_completion_with_tools ... ok
test malformed_final_arguments_are_never_repaired_to_empty_object ... ok
test ambiguous_idless_tool_delta_fails_but_single_tool_attaches ... ok
test defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args ... ok
test defect_3_compat_stream_rejects_same_chunk_and_split_terminal_junk ... ok
test malformed_proto_and_malformed_endstream_json_fail ... ok
test defect_6_malformed_tool_arguments_preserved_or_rejected_across_all_render_paths ... ok
test defect_1_debug_and_errors_redact_secrets_and_credentials ... ok
test korean_text_survives_every_chunk_split ... ok
test missing_endstream_at_every_frame_boundary_fails ... ok
test minimal_request_matches_independent_wire_vector ... ok
test metadata_keeps_api_key_and_literal_header_contract ... ok
test outcome_reports_stop_reason_and_error_metadata ... ok
test oversize_frame_is_rejected_without_buffering ... ok
test output_limit_stop_reasons_surface_output_limit_reached ... ok
test output_limits_temperature_and_top_p_are_preserved ... ok
test remote_images_and_non_vision_models_are_rejected_upstream_of_wire ... ok
test response_encoding_matches_pinned_field_numbers ... ok
test schema_descriptions_and_definitions_are_not_stripped ... ok
test stop_frame_alone_does_not_emit_success_until_valid_endstream ... ok
test tool_choice_none_removes_tools_but_auto_keeps_them ... ok
test tool_begin_is_delayed_until_name_is_known ... ok
test stream_decodes_thinking_signature_redaction_text_tool_and_usage ... ok
test tool_result_without_call_id_is_rejected ... ok
test unary_model_request_has_no_connect_envelope ... ok
test truncated_frame_fails_at_eof ... ok
test unnegotiated_compression_flags_are_rejected ... ok
test usage_is_none_when_absent_and_final_snapshot_wins_without_double_count ... ok
test defect_4_delayed_id_and_name_resolution_and_interleaved_tools_with_renderers ... ok
test every_chunk_split_produces_identical_events ... ok
test defect_2_oversized_frame_header_rejected_before_allocating_payload_or_appending_chunk ... ok

test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/provider_finish_contracts.rs (target/debug/deps/provider_finish_contracts-b362a539666d9d13)

running 6 tests
test anthropic_json_output_limit_is_length ... ok
test other_codex_incomplete_reasons_remain_failures ... ok
test antigravity_output_limit_survives_protocol_conversion ... ok
test codex_output_limit_is_not_an_upstream_failure ... ok
test anthropic_output_limit_survives_protocol_conversion ... ok
test output_limit_takes_precedence_over_partial_tool_calls ... ok

test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

### 2. Golden Wire Fixtures Suite (`devin_wire_fixtures`)
```bash
cargo test -p mahoquot-gateway --test devin_wire_fixtures
```
**Exit Code:** `0`  
**Output:**
```text
     Running tests/devin_wire_fixtures.rs (target/debug/deps/devin_wire_fixtures-e7267d5fae0f7367)

running 15 tests
test malformed_utf8_rejected_negative_fixture ... ok
test terminal_code_only_error ... ok
test reasoning_signature_and_redaction ... ok
test interleaved_tool_frames_across_frames ... ok
test chat_framed_response_stream ... ok
test literal_basic_dummy_token_header ... ok
test proto_parseable_invalid_utf8_rejected_negative_case ... ok
test chat_framed_request ... ok
test usage_and_cache_snapshot ... ok
test ambiguous_tool_calls_rejected_negative_fixture ... ok
test korean_utf8_complete_strings_and_arbitrary_transport_splits ... ok
test actual_model_uid_contract ... ok
test unary_unframed_request_and_response ... ok
test tag_checks_length_semantics_normalized ... ok
test all_positive_fixtures_have_valid_utf8_strings ... ok

test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

### 3. Shared Renderer Tests (`compat::render`)
```bash
cargo test -p mahoquot-gateway compat::render
```
**Exit Code:** `0`  
**Output:**
```text
     Running unittests src/lib.rs (target/debug/deps/mahoquot_gateway-dee0e90e6f9cfa8b)

running 7 tests
test compat::render::gemini_stream_tests::created_event_overrides_response_id ... ok
test compat::render::gemini_stream_tests::reasoning_signature_is_emitted_as_thought_signature_part ... ok
test compat::render::openai_stream_tests::reasoning_deltas_forward_as_reasoning_content ... ok
test compat::render::gemini_stream_tests::text_deltas_stream_immediately_and_usage_rides_the_terminal_frame ... ok
test compat::render::gemini_stream_tests::terminal_frame_sets_stop_and_stream_has_no_done_sentinel ... ok
test compat::render::gemini_stream_tests::tool_calls_stream_as_function_call_parts_with_restored_arguments ... ok
test compat::render::gemini_stream_tests::non_streaming_gemini_carries_tool_calls_and_a_matching_finish_reason ... ok

test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 315 filtered out; finished in 0.00s
```

### 4. Devin Private Unit Seam Bounded Allocation Test (`compat::devin`)
```bash
cargo test -p mahoquot-gateway compat::devin
```
**Exit Code:** `0`  
**Output:**
```text
     Running unittests src/lib.rs (target/debug/deps/mahoquot_gateway-dee0e90e6f9cfa8b)

running 1 test
test compat::devin::tests::test_bounded_allocation_private_unit_seam ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 321 filtered out; finished in 0.00s
```

### 5. Gateway Typecheck & LSP Check
```bash
cargo check -p mahoquot-gateway
```
**Exit Code:** `0`  
**Output:**
```text
    Checking mahoquot-providers v0.1.0 (/Volumes/T9-Mac/project/mahoquot-proxy/crates/providers)
    Checking mahoquot-gateway v0.1.0 (/Volumes/T9-Mac/project/mahoquot-proxy/crates/gateway)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 14.78s
```
LSP diagnostic sweep on `crates/gateway/src/compat/render.rs`: **0 diagnostics (clean)**.

---

## 4. Downstream API Contracts for P1 / P4 / Relay

The following public interfaces and data contracts are exported from `crates/gateway/src/compat/`:

### 1. `DevinRequestParams`
```rust
pub struct DevinRequestParams {
    pub token: String,
    pub chat_model_uid: String,
    pub supports_vision: bool,
    pub trajectory_id: String,
    pub cascade_id: String,
    pub execution_id: String,
    pub fingerprint: String,
}
```
- Custom `Debug` implementation securely redacts `token` as `"[REDACTED]"`.

### 2. Request Builders
```rust
pub fn build_chat_request(
    body: &serde_json::Value,
    params: &DevinRequestParams,
    new_id: &mut dyn FnMut() -> String,
) -> Result<Vec<u8>, String>;

pub fn build_model_configs_request(token: &str) -> Vec<u8>;
pub fn authorization_header(token: &str) -> String;
```
- `build_chat_request` serializes to streaming `GetChatMessageRequest` protobuf body.
- `build_model_configs_request` serializes to unframed unary `GetCascadeModelConfigsRequest` protobuf body.
- `authorization_header` produces `Basic <token>-<token>`.

### 3. Response Decoder & Outcome
```rust
pub struct DevinDecoder { ... }

impl DevinDecoder {
    pub fn new() -> Self;
    pub fn decode(&mut self, bytes: &[u8], out: &mut Vec<CodexEvent>);
    pub fn finish(&mut self, out: &mut Vec<CodexEvent>);
    pub fn outcome(&self) -> &DevinOutcome;
}

pub struct DevinOutcome {
    pub usage: Option<Usage>,
    pub actual_model_uid: Option<String>,
    pub stop_reason: Option<i32>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub terminated: bool,
}
```
- `outcome.error_code` preserves recognized Connect error codes (`unauthenticated`, `resource_exhausted`, etc.) or `"unknown"`.
- `outcome.usage` holds the final authoritative usage snapshot (never a synthesized zero).

### 4. Discovery Capability Types (`devin_proto::ClientModelConfig`)
```rust
pub struct ClientModelConfig {
    pub label: Option<String>,
    pub credit_multiplier: Option<f32>,
    pub disabled: Option<bool>,
    pub supports_images: Option<bool>,
    pub is_premium: Option<bool>,
    pub is_beta: Option<bool>,
    pub provider: Option<i32>,
    pub is_recommended: Option<bool>,
    pub is_new: Option<bool>,
    pub max_tokens: Option<i32>,
    pub promo_status: Option<PromoStatus>,
    pub is_capacity_limited: Option<bool>,
    pub model_uid: Option<String>,
    pub description: Option<String>,
    pub model_family_metadata: Option<ModelFamilyMetadata>,
}
```
- P4 catalog filtering can directly consume `disabled` (field 4), `provider` (field 10), `is_recommended` (field 11), `is_new` (field 15), and `is_capacity_limited` (field 20).

---

## 5. Verification Conclusion

All requirements and defect remediations have been verified with failing-first evidence and green results:
- `devin_wire`: **40 passed; 0 failed** (Exit code: `0`)
- `provider_finish_contracts`: **6 passed; 0 failed** (Exit code: `0`)
- `devin_wire_fixtures`: **15 passed; 0 failed** (Exit code: `0`)
- `compat::render`: **7 passed; 0 failed** (Exit code: `0`)
- `compat::devin` (bounded allocation unit seam): **1 passed; 0 failed** (Exit code: `0`)
- `cargo check -p mahoquot-gateway`: **PASSED** (Exit code: `0`)
- Scoped file boundaries strictly honored; zero git commits.

# Devin Provider P5 Integration & Relay-Callsite Evidence

## Executive Summary

This document records the completion of P3/P4 relay-callsite repairs and P5 client surface integration for the Devin provider in `mahoquot-proxy`.

All targeted integration requirements have been implemented and verified:
1. **Responses API Integration**: Streaming (`text/event-stream` SSE) and non-streaming `/v1/responses`, `/responses`, and `/backend-api/codex/responses` endpoints route to Devin through normalized request conversion (`responses_to_openai`) and rendered through `ResponsesStreamRenderer` / `responses_response`. Unsupported stateful features (`previous_response_id`) are rejected with HTTP 400.
2. **Claude Messages Integration**: Streaming and non-streaming `/v1/messages` and `/messages` handle multi-turn tool loops with interleaved argument delta indexing (`tool_calls_by_index`), thinking/reasoning blocks, signatures, redacted thinking, and tool execution error flags (`is_error: true`).
3. **Gemini v1beta Integration**: Native Gemini `/v1beta/models/{model}:streamGenerateContent` and `:generateContent` requests are normalized through `gemini_to_openai`, translating `contents`, `systemInstruction`, `functionDeclarations`, and `generationConfig` into Devin Connect requests.
4. **Legacy Completions Integration**: `/v1/completions` and `/completions` are routed to Devin for prompt-based completions; requests defining tools/functions are rejected with HTTP 400.
5. **Connect HTTP 200 Error Propagation**: Upstream Connect `EndStream` frames containing error codes (`resource_exhausted`, `permission_denied`, etc.) are mapped to canonical client HTTP status codes (e.g. 429 Too Many Requests, 403 Forbidden) across all four surfaces.
6. **Relay Callsite Defect Elimination**:
   - **Mutable Globals Removed**: Eliminated `FINALIZER_NOTIFIERS` and `PREFLIGHT_DEADLINE_MS` static globals from `relay.rs`. Finalizer notification is managed via `AppState`-owned subscriber registry (`state.subscribe_finalizer` / `state.notify_finalizer`). Preflight timeout is request-scoped via `x-test-preflight-deadline-ms` header (production default 10s).
   - **Credential Token Immutability**: The selected token from `member.access_token()` is preserved immutably and used identically for both the upstream `Authorization` header (`Basic <token>-<token>`) and the Devin protobuf request body (`DevinRequestParams.token`).
   - **Catalog-Driven Discovery & Vision**: Member model eligibility strictly enforces `pool.is_devin_model_eligible(member.id(), model)` (absent or empty catalogs deny access). Vision input support strictly checks `pool.devin_model_supports_vision(member.id(), model)` rather than string name heuristics.

---

## Changes Implemented

### 1. Gateway Relay (`crates/gateway/src/relay.rs`)
- Added `RelayMode::Responses` to support standard Responses API endpoints.
- Replaced static `FINALIZER_NOTIFIERS` and `subscribe_finalizer` with state-scoped notification:
  - `state.subscribe_finalizer(account_id)`
  - `state.notify_finalizer(account_id, event)`
- Replaced `PREFLIGHT_DEADLINE_MS` with `get_preflight_deadline(headers: &HeaderMap) -> Duration`, reading optional header `x-test-preflight-deadline-ms` (default 10s).
- Added `headers: Option<Vec<(String, String)>>` to `UpstreamTarget`.
- Updated `resolve_target` signature:
  - Takes `pool: &PoolSnapshot`, `member: &AccountMember`, `selected_token: &str`, `plan: &RelayPlan`, `upstream_model: &str`.
  - Constructs Devin Connect headers (`authorization`, `content-type: application/connect+proto`, `connect-protocol-version: 1`) using the immutable `selected_token`.
  - Enforces `pool.devin_model_supports_vision(member.id(), chat_model_uid)` for multimodal inputs.
- Updated `account_declares_binding_model`:
  - Delegates Devin model checks to `pool.is_devin_model_eligible(member.id(), requested_model)` and `canonical_model`.
  - Denies accounts with absent or unpopulated catalogs.
- Updated `send_upstream`:
  - Accepts `custom_headers: Option<&[(String, String)]>` to forward `target.headers`.
  - Sets HTTP/1.1 and `Accept: application/connect+proto` for Devin.

### 2. Router & CP Endpoints (`crates/gateway/src/cp_routes.rs`, `crates/gateway/src/routes.rs`)
- Routed `/v1/responses` and `/responses` with Devin models through `RelayMode::Responses`.
- Routed `/backend-api/codex/responses` through `RelayMode::Responses` when the target model belongs to Devin.
- Added validation to reject unsupported stateful parameters (`previous_response_id`) with HTTP 400 Bad Request.
- Updated `/v1beta/models/{*action}` to identify Devin-owned models and dispatch to `RelayMode::GeminiNative`.
- Added legacy completion validation to reject requests specifying tools/functions with HTTP 400.

### 3. Compatibility Translators (`crates/gateway/src/compat/`)
- `compat/claude.rs`:
  - Added support for Anthropic thinking blocks (`thinking` delta, signatures, redacted thinking blocks).
  - Maintained stable interleaved tool call delta indexing (`tool_calls_by_index: BTreeMap<u64, (String, String, String)>`).
  - Added preservation of `is_error: true` tool execution flags across translation to OpenAI tool messages.
- `compat/gemini.rs`:
  - Implemented `gemini_to_openai` translator mapping Gemini `contents`, `systemInstruction`, function declarations, and generation configs (`temperature`, `topP`, `maxOutputTokens`, `stopSequences`) into standard chat completions request.
- `compat/render.rs` & `compat/mod.rs`:
  - Added `ReplyShape::Responses` and wired `responses::ResponsesStreamRenderer`.
  - Added `into_responses` aggregator mapping Codex events into `ResponsesResponse`.
  - Added `devin_outcome` capture handling to `StreamingBodyParams` and `collect_stream_with_replies`.

### 4. AppState Lifecycle (`crates/gateway/src/state.rs`)
- Added `finalizer_notifiers: Arc<tokio::sync::Mutex<HashMap<String, Vec<tokio::sync::mpsc::UnboundedSender<FinalizerEvent>>>>>` to `AppState`.
- Added `AppState::subscribe_finalizer` and `AppState::notify_finalizer`.

---

## Test Verification

All tests were executed serially with individual cargo invocations.

### 1. Multi-Surface Integration Suite (`crates/gateway/tests/devin_surfaces.rs`)

Command:
```bash
cargo test --test devin_surfaces
```

Output:
```
     Running tests/devin_surfaces.rs (target/debug/deps/devin_surfaces-1d558d32dd973e0f)

running 8 tests
test test_devin_surfaces_responses_unsupported_rejections ... ok
test test_devin_surfaces_responses_two_turn_tools ... ok
test test_devin_surfaces_anthropic_stream_and_nonstream ... ok
test test_devin_surfaces_legacy_completions ... ok
test test_devin_surfaces_anthropic_two_turn_tools_and_error_flag ... ok
test test_devin_surfaces_error_propagation_across_surfaces ... ok
test test_devin_surfaces_gemini_native_stream_and_nonstream ... ok
test test_devin_surfaces_responses_stream_and_nonstream ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.19s
```
**Exit Code**: `0`

**Coverage in `devin_surfaces.rs`**:
- `test_devin_surfaces_responses_stream_and_nonstream`: Streaming SSE and non-streaming POST `/v1/responses`.
- `test_devin_surfaces_responses_two_turn_tools`: Multi-turn function tool call and output submission turn.
- `test_devin_surfaces_responses_unsupported_rejections`: Rejection of `previous_response_id`.
- `test_devin_surfaces_anthropic_stream_and_nonstream`: Streaming SSE and non-streaming POST `/v1/messages`.
- `test_devin_surfaces_anthropic_two_turn_tools_and_error_flag`: Claude multi-turn tools with `is_error: true` tool result flag preservation.
- `test_devin_surfaces_gemini_native_stream_and_nonstream`: Streaming and non-streaming Gemini `/v1beta/models/...` surfaces.
- `test_devin_surfaces_legacy_completions`: Prompt-based `/v1/completions` success and tool-bearing rejection.
- `test_devin_surfaces_error_propagation_across_surfaces`: Connect HTTP 200 `resource_exhausted` code mapped to HTTP 429 across Anthropic, Responses, and Gemini surfaces.

---

### 2. Devin Relay Regressions Suite (`crates/gateway/tests/devin_relay.rs`)

Command:
```bash
cargo test --test devin_relay
```

Output:
```
     Running tests/devin_relay.rs (target/debug/deps/devin_relay-fe357c532bb8ee83)

running 21 tests
test test_devin_relay_no_auth_redirect ... ok
test test_devin_relay_response_wrong_content_type ... ok
test test_devin_relay_nonstream_late_connect_error_mapping ... ok
test test_devin_relay_absent_catalog_account_b_denied_account_a_model ... ok
test test_devin_relay_discovered_vision_false_rejects_vision ... ok
test test_devin_relay_fragmented_code_only_error_matches_unsplit ... ok
test test_devin_relay_no_retry_on_ambiguous_precommit_failure ... ok
test test_devin_relay_no_retry_after_downstream_commitment ... ok
test test_devin_relay_non_streaming_success ... ok
test test_devin_relay_http_200_connect_error_code_mapping ... ok
test test_devin_relay_split_header_and_coalesced_oversize ... ok
test test_devin_relay_late_error_exactly_once_failure_and_retained_health ... ok
test test_devin_relay_wire_contract_assertions ... ok
test test_devin_relay_late_error_no_success_terminal ... ok
test test_devin_relay_unsupported_native_responses_rejected ... ok
test test_devin_relay_concurrent_refresh_and_credential_rotation ... ok
test test_devin_relay_streaming_success ... ok
test test_devin_relay_token_non_disclosure ... ok
test test_devin_relay_upstream_disconnect_inflight_and_history_failure ... ok
test test_devin_relay_missing_usage_vs_zero ... ok
test test_devin_relay_preflight_total_deadline_headers_then_no_body ... ok

test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.38s
```
**Exit Code**: `0`

**Key Regressions Verified**:
- `test_devin_relay_absent_catalog_account_b_denied_account_a_model`: Confirms account B with absent catalog is not selected for account A's model.
- `test_devin_relay_discovered_vision_false_rejects_vision`: Confirms model discovered with `supports_images: false` rejects image inputs.
- `test_devin_relay_unsupported_native_responses_rejected`: Confirms unsupported stateful parameters are rejected.
- `test_devin_relay_preflight_total_deadline_headers_then_no_body`: Confirms preflight timeout bounds header-only responses without leaking raw URLs.
- `test_devin_relay_upstream_disconnect_inflight_and_history_failure`: Confirms transport cancellations notify finalizers cleanly.

---

### 3. Devin Wire Contract Suite (`crates/gateway/tests/devin_wire.rs`)

Command:
```bash
cargo test --test devin_wire
```

Output:
```
     Running tests/devin_wire.rs (target/debug/deps/devin_wire-3f5d258a23f67c59)

running 40 tests
test defect_5_response_format_strict_and_json_schema_are_rejected ... ok
test devin_binary_stream_passes_open_stream_protocol_gate ... ok
test defect_1_debug_and_errors_redact_secrets_and_credentials ... ok
test devin_stream_aggregates_into_chat_completion_with_tools ... ok
test duplicate_terminal_and_trailing_data_are_rejected ... ok
test forced_tool_choice_n_1_strict_output_are_rejected ... ok
test absent_options_use_reference_defaults ... ok
test ambiguous_idless_tool_delta_fails_but_single_tool_attaches ... ok
test code_only_terminal_error_fails_with_code_preserved ... ok
test defect_3_terminated_stream_rejects_subsequent_chunks_and_trailing_data_never_emits_completed ... ok
test full_history_preserves_roles_tool_reasoning_signature_and_images ... ok
test defect_6_malformed_tool_arguments_preserved_or_rejected_across_all_render_paths ... ok
test every_chunk_split_produces_identical_events ... ok
test defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args ... ok
test malformed_final_arguments_are_never_repaired_to_empty_object ... ok
test interleaved_tool_indices_are_stable_across_text_and_reasoning ... ok
test missing_endstream_at_every_frame_boundary_fails ... ok
test minimal_request_matches_independent_wire_vector ... ok
test outcome_reports_stop_reason_and_error_metadata ... ok
test output_limit_stop_reasons_surface_output_limit_reached ... ok
test korean_text_survives_every_chunk_split ... ok
test output_limits_temperature_and_top_p_are_preserved ... ok
test defect_4_delayed_id_and_name_resolution_and_interleaved_tools_with_renderers ... ok
test malformed_proto_and_malformed_endstream_json_fail ... ok
test oversize_frame_is_rejected_without_buffering ... ok
test metadata_keeps_api_key_and_literal_header_contract ... ok
test remote_images_and_non_vision_models_are_rejected_upstream_of_wire ... ok
test response_encoding_matches_pinned_field_numbers ... ok
test defect_3_compat_stream_rejects_same_chunk_and_split_terminal_junk ... ok
test schema_descriptions_and_definitions_are_not_stripped ... ok
test stop_frame_alone_does_not_emit_success_until_valid_endstream ... ok
test stream_decodes_thinking_signature_redaction_text_tool_and_usage ... ok
test tool_result_without_call_id_is_rejected ... ok
test truncated_frame_fails_at_eof ... ok
test tool_begin_is_delayed_until_name_is_known ... ok
test unary_model_request_has_no_connect_envelope ... ok
test unnegotiated_compression_flags_are_rejected ... ok
test tool_choice_none_removes_tools_but_auto_keeps_them ... ok
test defect_2_oversized_frame_header_rejected_before_allocating_payload_or_appending_chunk ... ok
test usage_is_none_when_absent_and_final_snapshot_wins_without_double_count ... ok

test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
**Exit Code**: `0`

---

### 4. Responses Adapter Suite (`crates/gateway/tests/devin_responses_adapter.rs`)

Command:
```bash
cargo test --test devin_responses_adapter
```

Output:
```
     Running tests/devin_responses_adapter.rs (target/debug/deps/devin_responses_adapter-a4fc59b4d467d8c2)

running 21 tests
test test_request_instructions_and_string_input ... ok
test test_request_base64_vision_accepted_and_remote_url_rejected ... ok
test test_request_function_tools_and_tool_choice_mapping ... ok
test test_request_malformed_tool_arguments_rejected_without_silent_coercion ... ok
test test_request_missing_call_id_or_name_rejected ... ok
test test_request_reasoning_signature_and_redaction_preservation ... ok
test test_responses_error_json_structure_and_into_response ... ok
test test_request_valid_multi_turn_tool_history ... ok
test test_request_unsupported_remote_stateful_and_background_features_rejected ... ok
test test_request_structured_input_messages_and_parameters ... ok
test test_request_tool_history_is_error_flag_preservation ... ok
test test_request_orphan_or_duplicate_tool_calls_rejected ... ok
test test_request_unsupported_tool_types_rejected ... ok
test test_nonstream_incomplete_and_failure_handling ... ok
test test_streaming_lifecycle_tool_call_turn ... ok
test test_streaming_interleaved_tools_maintain_stable_ids_and_indices ... ok
test test_streaming_lifecycle_text_turn ... ok
test test_streaming_usage_unknown_vs_zero ... ok
test test_nonstream_json_equivalent_to_stream_lifecycle ... ok
test test_streaming_output_limit_vs_failure ... ok
test test_streaming_reasoning_deltas_lifecycle ... ok

test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
**Exit Code**: `0`

---

### 5. Devin Catalog & Discovery Suite (`crates/gateway/tests/devin_catalog.rs`)

Command:
```bash
cargo test --test devin_catalog
```

Output:
```
     Running tests/devin_catalog.rs (target/debug/deps/devin_catalog-9fb52b8dcf2cf5ad)

running 26 tests
test test_account_snapshot_permissions_debug_redacts_token ... ok
test test_devin_cache_ttl_and_transient_failure_preservation ... ok
test test_devin_concurrent_discovery_cache_rcu ... ok
test test_devin_channel_gated_credential_rotation_race ... ok
test test_devin_channel_gated_account_disabled_race ... ok
test test_devin_known_empty_catalog_is_valid_stale_lkg ... ok
test test_devin_disjoint_account_routing_and_pool_snapshot ... ok
test test_devin_account_pinning_scoped_keys_exclusions_unsupported ... ok
test test_devin_discovery_exact_mime_essence_validation ... ok
test test_devin_unknown_model_no_codex_generic_fallback ... ok
test test_management_contract_schema_and_parity_matrix_for_devin_models_refresh ... ok
test test_devin_supports_images_vision_not_image_generation ... ok
test test_devin_oversized_streaming_body_rejected_incrementally ... ok
test test_devin_catalog_wire_codec_and_transport_invariants ... ok
test test_devin_discovery_timeout_bounds_body_reading ... ok
test test_devin_credential_replacement_creates_distinct_runtime_identity ... ok
test test_devin_same_token_endpoint_change_race ... ok
test test_devin_generation_consistent_snapshot_permissions_isolation ... ok
test test_devin_overlapping_refresh_out_of_order_stale_overwrite_prevented ... ok
test test_devin_old_snapshot_hold_immutability ... ok
test test_devin_chat_completion_runtime_identity_preserved_across_catalog_refresh ... ok
test test_devin_initial_refresh_failure_publishes_catalog_and_subsequent_get_reports_error ... ok
test test_devin_stale_get_schedules_async_refresh ... ok
test test_devin_cached_transient_failure_refresh_returns_outcome_error_when_all_fail ... ok
test test_devin_invalid_credentialed_proxy_prevents_direct_leak_and_masks_credentials ... ok
test test_devin_management_refresh_endpoint_flow ... ok

test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.47s
```
**Exit Code**: `0`

---

### 6. Devin Credentials Suite (`crates/gateway/tests/devin_credentials.rs`)

Command:
```bash
cargo test --test devin_credentials
```

Output:
```
     Running tests/devin_credentials.rs (target/debug/deps/devin_credentials-e26c41c8df6a299c)

running 16 tests
test test_management_contract_schema_and_parity_matrix_for_devin_import_cli ... ok
test test_codex_rejects_devin_models_and_devin_unroutable_until_discovery ... ok
test test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic ... ok
test test_devin_malformed_typed_json_http_response_does_not_leak_secret ... ok
test test_devin_no_oauth_refresh_and_quota_unsupported ... ok
test test_describe_does_not_synthesize_identity_for_non_devin_providers ... ok
test test_disk_rescan_validates_devin_account_and_skips_invalid ... ok
test test_manual_auth_file_upload_persists_validated_normalized_content ... ok
test test_devin_import_cli_typed_payload_allowlist_and_conflicts ... ok
test test_devin_reload_from_file_display_debug_does_not_leak_secret ... ok
test test_disabled_and_unloaded_credential_exposes_canonical_identity_slug_in_inventory ... ok
test test_devin_lifecycle_disable_delete_rescan_and_redaction ... ok
test test_manual_auth_file_upload_and_stable_identity_replacement ... ok
test test_devin_import_cli_rejects_path_escape_and_invalid_identities ... ok
test test_distinct_identities_work_and_devin_work_isolated_lifecycle ... ok
test test_cli_import_resolved_on_proxy_host_atomic_and_unchanged_source ... ok

test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.75s
```
**Exit Code**: `0`

---

### 7. Review Codex Gemini Suite (`crates/gateway/tests/review_codex_gemini.rs`)

Command:
```bash
cargo test --test review_codex_gemini
```

Output:
```
     Running tests/review_codex_gemini.rs (target/debug/deps/review_codex_gemini-d1b55d07bde8bedb)

running 12 tests
test generated_call_ids_do_not_alias_provider_counter_ids ... ok
test codex_cancel_is_not_silent_success ... ok
test codex_incomplete_preserves_machine_reason ... ok
test gemini_wrapped_error_is_terminal_even_with_later_stop ... ok
test codex_chat_tool_roundtrip_preserves_ids_arguments_and_results ... ok
test codex_reasoning_survives_every_byte_split ... ok
test gemini_thinking_text_signature_replays_into_tool_history ... ok
test gemini_parallel_calls_keep_order_all_received_signatures_and_results ... ok
test aggregate_reasoning_and_native_gemini_ids_signatures_survive ... ok
test terminal_stream_drops_upstream_without_waiting_for_eof ... ok
test http_codex_and_gemini_two_turn_tools_refresh_and_explicit_parity_errors ... ok
test http_terminal_variants_and_client_cancel_release_upstream ... ok

test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
```
**Exit Code**: `0`

---

## Final Verification Summary Table

| Test Suite | Total Tests | Passed | Failed | Exit Code | Notes |
|:---|:---:|:---:|:---:|:---:|:---|
| `devin_surfaces` | 8 | 8 | 0 | 0 | Responses, Claude, Gemini v1beta, Legacy, tools, Connect errors |
| `devin_relay` | 21 | 21 | 0 | 0 | Relay callsite repairs, vision check, preflight deadline, isolation |
| `devin_wire` | 40 | 40 | 0 | 0 | Wire contract equality, framing, header verification |
| `devin_responses_adapter` | 21 | 21 | 0 | 0 | Responses translation & lifecycle adapter tests |
| `devin_catalog` | 26 | 26 | 0 | 0 | Discovery cache, TTL, permissions, and RCU isolation |
| `devin_credentials` | 16 | 16 | 0 | 0 | Import CLI, disk persistence, and identity redaction |
| `review_codex_gemini` | 12 | 12 | 0 | 0 | Codex & Gemini cross-surface tools and streaming |
| **Total** | **144** | **144** | **0** | **0** | **100% clean exit across all 7 targeted suites** |

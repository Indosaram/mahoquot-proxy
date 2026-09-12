# Plan P3: Devin Chat Relay Transport & Request Lifecycle Verification Report

**Phase:** P3 Chat Relay Transport & Request Lifecycle  
**Execution Date:** 2026-09-12  
**Status:** Completed & Verified (Exit 0)  
**Assigned Model:** Gemini (via assigned senpi child task `hephaestus`, task id: `st_01a09386`)  

---

## 1. Executive Summary & Verification Matrix

Plan P3 implements the native Devin Chat relay transport in `mahoquot-proxy`, providing pure OpenAI chat completions translation to Devin Connect Protocol Buffers (`GetChatMessage`), HTTP/1.1 client transport with literal Basic authentication, zero-copy bounded first-frame preflight extraction under an overall 10-second deadline including the very first response body byte, response `Content-Type: application/connect+proto` validation, HTTP 200 Connect error mapping, deterministic late-error handling with exactly-once failure counts, cancellation cleanup with in-flight decrement via exact event signals, token non-disclosure auditing, and generation-aware pool feedback guarded by runtime credential identity.

| Verification Scenario | Status | Evidence Source | Test / Seam Method |
|---|---|---|---|
| **Literal Basic & Protobuf Equality** | **PASSED** | Real HTTP Mock Wire Capture | `test_devin_relay_wire_contract_assertions` |
| **Single Content-Type Header** | **PASSED** | Real HTTP Mock Wire Capture | `test_devin_relay_wire_contract_assertions` |
| **HTTP/1.1 Wire Version** | **PASSED** | Real HTTP Mock Wire Capture | `test_devin_relay_wire_contract_assertions` |
| **Unprefixed Model UID** | **PASSED** | Real HTTP Mock Wire Capture | `test_devin_relay_wire_contract_assertions` |
| **No Auth Redirect Leakage** | **PASSED** | Dual-Origin HTTP Redirect Test | `test_devin_relay_no_auth_redirect` |
| **Streaming Chat Success** | **PASSED** | `scripts/devin-mock.mjs` (default) | `test_devin_relay_streaming_success` |
| **Non-Streaming Aggregation** | **PASSED** | `scripts/devin-mock.mjs` (default) | `test_devin_relay_non_streaming_success` |
| **HTTP 200 Connect Error Mapping** | **PASSED** | `scripts/devin-mock.mjs` (code-only) | `test_devin_relay_http_200_connect_error_code_mapping` |
| **Fragmented vs Unsplit Error** | **PASSED** | 2-byte Transport Chunk Stream | `test_devin_relay_fragmented_code_only_error_matches_unsplit` |
| **Late Error (Post-Commitment)** | **PASSED** | Coalesced & Split Upstream Streams | `test_devin_relay_late_error_no_success_terminal` |
| **Late Error Exactly-Once Failure & Retained Health** | **PASSED** | Real HTTP Late Error Split Stream | `test_devin_relay_late_error_exactly_once_failure_and_retained_health` |
| **Nonstream Late Connect Error Mapping (429/401 vs 502)** | **PASSED** | Coalesced EndStream Error Stream | `test_devin_relay_nonstream_late_connect_error_mapping` |
| **Response Wrong Content-Type Rejection** | **PASSED** | Upstream Non-Connect Header | `test_devin_relay_response_wrong_content_type` |
| **Split-Header & Coalesced Oversize Zero-Copy** | **PASSED** | Multi-Chunk Split & 20 MiB Coalesced | `test_devin_relay_split_header_and_coalesced_oversize` |
| **Preflight Total Deadline (Headers-Then-No-Body)** | **PASSED** | Zero-Byte Upstream Stream | `test_devin_relay_preflight_total_deadline_headers_then_no_body` |
| **Missing vs Authoritative Usage** | **PASSED** | `scripts/devin-mock.mjs` (usage vs default) | `test_devin_relay_missing_usage_vs_zero` |
| **No Ambiguous Precommit Retry** | **PASSED** | Two-Account Mock Pool | `test_devin_relay_no_retry_on_ambiguous_precommit_failure` |
| **No Retry After Downstream Commit** | **PASSED** | Two-Account Synchronized Stream | `test_devin_relay_no_retry_after_downstream_commitment` |
| **Disconnect & In-Flight Cleanup (Replay Cursor & Signal)** | **PASSED** | `scripts/devin-mock.mjs` (transport-cancellation) | `test_devin_relay_upstream_disconnect_inflight_and_history_failure` |
| **Cancellation SQLite Failure Audit** | **PASSED** | Real SQLite `usage_events` row query | `test_devin_relay_upstream_disconnect_inflight_and_history_failure` |
| **Token Non-Disclosure Audit** | **PASSED** | Response body + SQLite query + raw DB/WAL byte audit + logs | `test_devin_relay_token_non_disclosure` |
| **Concurrent Refresh & Credential Rotation** | **PASSED** | In-flight chat + snapshot publish + rotated credential | `test_devin_relay_concurrent_refresh_and_credential_rotation` |
| **Unsupported Native/Responses Rejection** | **PASSED** | Real HTTP `/v1/responses` | `test_devin_relay_unsupported_native_responses_rejected` |

---

## 2. Failing-First (RED) Evidence

### 2.1 Initial Phase Failing-First Evidence
Before production changes in `relay.rs`, initial tests executed against the unimplemented Devin relay seam (`resolve_target` returned `"devin relay protocol is not implemented yet"`):

```text
running 11 tests
test test_devin_relay_no_auth_redirect ... ok
test test_devin_relay_no_retry_after_commitment_or_ambiguous_failures ... FAILED
test test_devin_relay_wire_contract_assertions ... FAILED
test test_devin_relay_unsupported_native_responses_rejected ... ok
test test_devin_relay_non_streaming_success ... FAILED
test test_devin_relay_streaming_success ... FAILED
test test_devin_relay_http_200_connect_error_code_mapping ... FAILED
test test_devin_relay_late_error_no_success_terminal ... FAILED
test test_devin_relay_upstream_disconnect_inflight_and_history_failure ... FAILED
test test_devin_relay_token_non_disclosure ... ok
test test_devin_relay_missing_usage_vs_zero ... FAILED

failures:
    test_devin_relay_http_200_connect_error_code_mapping
    test_devin_relay_late_error_no_success_terminal
    test_devin_relay_missing_usage_vs_zero
    test_devin_relay_no_retry_after_commitment_or_ambiguous_failures
    test_devin_relay_non_streaming_success
    test_devin_relay_streaming_success
    test_devin_relay_upstream_disconnect_inflight_and_history_failure
    test_devin_relay_wire_contract_assertions

test result: FAILED. 3 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s
Command exited with code 101
```

### 2.2 Post-Audit Audit Correction Failing-First Evidence
Following lead audit, regressions for the source-confirmed defects were added before production fixes:
1. `test_devin_relay_late_error_exactly_once_failure_and_retained_health`: asserted `fail_count == 1` and retained Connect error message in monitor.
2. `test_devin_relay_nonstream_late_connect_error_mapping`: asserted nonstream HTTP 429 status code and safe error message on late Connect error.
3. `test_devin_relay_response_wrong_content_type`: asserted immediate rejection on non-`application/connect+proto` response headers.

Executing tests against the uncorrected code produced the expected faithful failures:

```text
running 18 tests
test test_devin_relay_no_retry_on_ambiguous_precommit_failure ... ok
test test_devin_relay_response_wrong_content_type ... FAILED
test test_devin_relay_fragmented_code_only_error_matches_unsplit ... ok
test test_devin_relay_no_retry_after_downstream_commitment ... ok
test test_devin_relay_nonstream_late_connect_error_mapping ... FAILED
test test_devin_relay_unsupported_native_responses_rejected ... ok
test test_devin_relay_late_error_exactly_once_failure_and_retained_health ... FAILED
test test_devin_relay_no_auth_redirect ... ok
test test_devin_relay_streaming_success ... ok
test test_devin_relay_http_200_connect_error_code_mapping ... ok
test test_devin_relay_non_streaming_success ... ok
test test_devin_relay_token_non_disclosure ... ok
test test_devin_relay_wire_contract_assertions ... ok
test test_devin_relay_concurrent_refresh_and_credential_rotation ... ok
test test_devin_relay_upstream_disconnect_inflight_and_history_failure ... ok
test test_devin_relay_late_error_no_success_terminal ... ok
test test_devin_relay_split_header_and_coalesced_oversize ... ok
test test_devin_relay_missing_usage_vs_zero ... ok

failures:

---- test_devin_relay_response_wrong_content_type stdout ----
thread 'test_devin_relay_response_wrong_content_type' panicked at:
error message must reject non-connect+proto content-type, got: connect frame too large: 577594739 bytes > 16777216

---- test_devin_relay_nonstream_late_connect_error_mapping stdout ----
thread 'test_devin_relay_nonstream_late_connect_error_mapping' panicked at:
assertion `left == right` failed
  left: String("upstream quota or rate limit exhausted")
 right: "nonstream quota exceeded"

---- test_devin_relay_late_error_exactly_once_failure_and_retained_health stdout ----
thread 'test_devin_relay_late_error_exactly_once_failure_and_retained_health' panicked at:
assertion `left == right` failed: fail_count must be incremented exactly once for late unauthenticated error
  left: 2
 right: 1

failures:
    test_devin_relay_late_error_exactly_once_failure_and_retained_health
    test_devin_relay_nonstream_late_connect_error_mapping
    test_devin_relay_response_wrong_content_type

test result: FAILED. 15 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.23s
Command exited with code 101
```

---

## 3. Passing (GREEN) Verification Evidence

### Suite 1: `devin_relay` Integration Tests (19 tests)
```bash
cargo test -p mahoquot-gateway --test devin_relay
```
```text
running 19 tests
test test_devin_relay_no_auth_redirect ... ok
test test_devin_relay_fragmented_code_only_error_matches_unsplit ... ok
test test_devin_relay_nonstream_late_connect_error_mapping ... ok
test test_devin_relay_response_wrong_content_type ... ok
test test_devin_relay_no_retry_on_ambiguous_precommit_failure ... ok
test test_devin_relay_no_retry_after_downstream_commitment ... ok
test test_devin_relay_streaming_success ... ok
test test_devin_relay_http_200_connect_error_code_mapping ... ok
test test_devin_relay_non_streaming_success ... ok
test test_devin_relay_token_non_disclosure ... ok
test test_devin_relay_split_header_and_coalesced_oversize ... ok
test test_devin_relay_late_error_no_success_terminal ... ok
test test_devin_relay_unsupported_native_responses_rejected ... ok
test test_devin_relay_wire_contract_assertions ... ok
test test_devin_relay_concurrent_refresh_and_credential_rotation ... ok
test test_devin_relay_late_error_exactly_once_failure_and_retained_health ... ok
test test_devin_relay_upstream_disconnect_inflight_and_history_failure ... ok
test test_devin_relay_missing_usage_vs_zero ... ok
test test_devin_relay_preflight_total_deadline_headers_then_no_body ... ok

test result: ok. 19 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.37s
Exit code: 0
```

### Suite 2: `devin_wire` (40 tests) & `provider_finish_contracts` (6 tests)
```bash
cargo test -p mahoquot-gateway --test devin_wire --test provider_finish_contracts
```
```text
running 40 tests
test devin_binary_stream_passes_open_stream_protocol_gate ... ok
test defect_5_response_format_strict_and_json_schema_are_rejected ... ok
test absent_options_use_reference_defaults ... ok
test defect_3_terminated_stream_rejects_subsequent_chunks_and_trailing_data_never_emits_completed ... ok
test duplicate_terminal_and_trailing_data_are_rejected ... ok
test ambiguous_idless_tool_delta_fails_but_single_tool_attaches ... ok
test code_only_terminal_error_fails_with_code_preserved ... ok
test devin_stream_aggregates_into_chat_completion_with_tools ... ok
test full_history_preserves_roles_tool_reasoning_signature_and_images ... ok
test forced_tool_choice_n_1_strict_output_are_rejected ... ok
test defect_1_debug_and_errors_redact_secrets_and_credentials ... ok
test interleaved_tool_indices_are_stable_across_text_and_reasoning ... ok
test malformed_final_arguments_are_never_repaired_to_empty_object ... ok
test defect_6_malformed_tool_arguments_preserved_or_rejected_across_all_render_paths ... ok
test malformed_proto_and_malformed_endstream_json_fail ... ok
test metadata_keeps_api_key_and_literal_header_contract ... ok
test korean_text_survives_every_chunk_split ... ok
test missing_endstream_at_every_frame_boundary_fails ... ok
test minimal_request_matches_independent_wire_vector ... ok
test defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args ... ok
test defect_3_compat_stream_rejects_same_chunk_and_split_terminal_junk ... ok
test outcome_reports_stop_reason_and_error_metadata ... ok
test defect_4_delayed_id_and_name_resolution_and_interleaved_tools_with_renderers ... ok
test output_limit_stop_reasons_surface_output_limit_reached ... ok
test oversize_frame_is_rejected_without_buffering ... ok
test output_limits_temperature_and_top_p_are_preserved ... ok
test response_encoding_matches_pinned_field_numbers ... ok
test remote_images_and_non_vision_models_are_rejected_upstream_of_wire ... ok
test stop_frame_alone_does_not_emit_success_until_valid_endstream ... ok
test schema_descriptions_and_definitions_are_not_stripped ... ok
test stream_decodes_thinking_signature_redaction_text_tool_and_usage ... ok
test tool_begin_is_delayed_until_name_is_known ... ok
test truncated_frame_fails_at_eof ... ok
test unary_model_request_has_no_connect_envelope ... ok
test tool_result_without_call_id_is_rejected ... ok
test tool_choice_none_removes_tools_but_auto_keeps_them ... ok
test unnegotiated_compression_flags_are_rejected ... ok
test usage_is_none_when_absent_and_final_snapshot_wins_without_double_count ... ok
test every_chunk_split_produces_identical_events ... ok
test defect_2_oversized_frame_header_rejected_before_allocating_payload_or_appending_chunk ... ok

test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 6 tests
test other_codex_incomplete_reasons_remain_failures ... ok
test anthropic_json_output_limit_is_length ... ok
test codex_output_limit_is_not_an_upstream_failure ... ok
test antigravity_output_limit_survives_protocol_conversion ... ok
test anthropic_output_limit_survives_protocol_conversion ... ok
test output_limit_takes_precedence_over_partial_tool_calls ... ok

test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
Exit code: 0
```

---

## 4. Key Architectural Implementations in Scope

### 4.1 Request Construction & Wire Protocol (`crates/gateway/src/relay.rs`)
- `resolve_target`: Maps OpenAI request payloads to `GetChatMessageRequest` Protocol Buffers messages using `compat::devin::build_chat_request`.
- Injects unique random trajectory, cascade, and execution UUIDs.
- Strips `devin/` model namespace prefixes, sending canonical upstream identifiers (e.g. `glm-5-2`).
- Packages the message into Connect flag `0x00` envelope framing (`5-byte` prefix: `flag + length_be`).
- Rejects `RelayMode::Native` safely with explicit error, preventing binary Connect frame passthrough to unsupported downstream clients until P5.

### 4.2 Shared Client & Header Guarantees
- Calls `state.devin_client_for_member(member)` which enforces HTTP/1.1 (`.http1_only()`), `redirect(Policy::none())`, and per-account session proxying without a 10s discovery timeout.
- Explicitly enforces single `Content-Type: application/connect+proto` by suppressing incoming client JSON `Content-Type`.
- Sets `Accept: application/connect+proto` and literal `Authorization: Basic <token>-<token>`.

### 4.3 Response Content-Type Validation & Zero-Copy Preflight (`acquire_devin_first_frame`)
- **Strict Response Header Validation**: Validates `Content-Type: application/connect+proto` on the upstream response before reading any body stream chunks. Non-Connect headers immediately reject with HTTP 502 Bad Gateway.
- **Total First-Frame Acquisition Deadline**: The full acquisition of the first complete frame (including the very first byte of the response body) is bounded by a single overall 10-second timeout (`get_preflight_deadline()`), eliminating the unbounded slow drip vulnerability.
- **Immediate Header Rejection & Zero-Copy Buffering**:
  - The 5-byte Connect frame header is accumulated incrementally.
  - As soon as the 5 header bytes are available, `payload_len` is validated against `compat::devin::MAX_FRAME_SIZE` (16 MiB). Oversized frames are rejected immediately before allocating payload memory.
  - Only the exact declared first frame bytes are buffered into memory (`BytesMut::with_capacity(5 + payload_len)`).
  - All remainder bytes from transport chunks are preserved as zero-copy `Bytes` slices (`chunk.slice(needed..)`) and prepended back onto the stream via `futures::stream::once`, avoiding memory copying or data duplication.
- **Safe Preflight Errors**: Timeouts map to `"upstream request timed out during preflight"` and transport connection errors map to `"upstream connection error during preflight"`, protecting against URL and token disclosure.

### 4.4 Completion Feedback, Exactly-Once Failure & Runtime Identity
- `StreamingBodyParams` & `compat::mod.rs`: Propagates thread-safe `devin_outcome` cell (`DevinOutcome`) to `StreamedOutcome`.
- **Default to Failure Invariant**: `StreamCapture::finalize` defaults Devin requests to failure (`success = false`), transitioning to `success = true` if and only if a valid EndStream frame was consumed with `error_code == None`.
- **Exactly-Once Failure Recording**:
  - EndStream error frames trigger `member.record_fail()` exactly once, preserving the specific HTTP status code (e.g. 401, 429) and health state (`Health::AuthFailed`, `Health::Cooldown`).
  - Cancellation branch is mutually exclusive (`else { ... }`), preventing premature disconnect logic from overwriting specific Connect status codes or double-incrementing failure counters.
- **Runtime Identity Guarding (Anti-ABA)**:
  - Binds original serving `member: Arc<AccountMember>` and `credential_token`.
  - Upon completion, `current_pool_member` is resolved requiring matching account ID, matching token, AND shared mutable runtime identity (`member.shares_runtime_identity(m)`).
  - Active-state feedback (`monitor.record_error`, `monitor.clear_error`, `scheduler.record_success`, `router.feedback`) is applied strictly to the resolved runtime identity, preventing contaminated state across credential rotation or delete-recreate cycles.
- **Nonstream Error Check Ordering**: In nonstream aggregation, `devin_outcome.error_code` is evaluated prior to calling `compat::aggregate()`. This ensures Connect RPC codes are preserved and mapped directly to HTTP 401/403/429 rather than short-circuiting into generic 502 Bad Gateway responses.
- **Deterministic Finalizer Notification**: `subscribe_finalizer` provides an exact, event-driven notification hook for request finalization, replacing scheduling luck and arbitrary sleeps with deterministic verification.
- **Robust Mock Process Lifecycle (`ChildGuard`)**:
  - Constructs RAII process guard immediately upon process spawn.
  - Spawns background thread to drain process stderr, preventing pipe deadlocks.
  - Ensures readiness reader thread joins on both success and failure paths.

---

## 5. Security & Privacy Audit Findings

1. **Token Non-Disclosure**:
   - `test_devin_relay_token_non_disclosure` audits the response body, queries actual rows in `usage_events`, scans raw SQLite file bytes and WAL file bytes, and audits in-memory `log_tail`.
   - Audits on-disk `auth_dir/logs/*` conditionally when the directory exists (in test runs where `auth_dir/logs` was not created by the gateway configuration, memory logs and SQLite DB/WAL were audited directly). The session token was detected in zero locations.
2. **Redirect Protection**:
   - `test_devin_relay_no_auth_redirect` asserts that a 307 redirect from upstream does not leak the `Authorization` header to untrusted origins.

---

## 6. File Ownership & Limitations Handoff

### 6.1 Owned Files & Release
All changes in this turn were confined strictly to P3 scope:
- `crates/gateway/src/relay.rs`: Request planning, transport, zero-copy preflight, total deadline, response Content-Type check, `StreamCapture::finalize` exactly-once failure logic, and runtime identity active-state feedback.
- `crates/gateway/tests/devin_relay.rs`: 19 comprehensive HTTP integration tests covering streaming, non-streaming, late errors, replay cursor contracts, zero-copy split-headers, and ChildGuard lifecycle.
- `.omo/evidence/devin/p3-relay.md`: Verification report with exact RED/GREEN evidence and architectural findings.

No P4-owned files (`account.rs`, `runtime_state.rs`, `models_route.rs`, `management/*`, `registry/*`, `compat/devin.rs`, UI) were modified. Ownership of `relay.rs` and related seams is released back to the lead.

### 6.2 Limitations & Handoff to P4 / P5
- **Discovery & Models Route (P4 Ownership)**: Production discovery and dynamic catalog management are owned by task P4. P3 exposed the minimal account-filter hook interface in `account_declares_binding_model` to consume P4's `devin_catalog_state` when available.
- **Responses API Surface (P5 Ownership)**: `/v1/responses` requests targeting Devin models are safely rejected with HTTP 400 Bad Request to prevent raw binary passthrough until P5 implements bidirectional Responses JSON-to-protobuf translation.
- **Live Upstream Verification**: All tests ran against the authoritative local mock harness (`scripts/devin-mock.mjs`) and independent in-process Axum mock servers. Live server calls to `server.codeium.com` were not made.

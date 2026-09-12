# Plan P5: Devin Responses Request and Event Adapter Verification

**Date:** 2026-09-12  
**Task ID:** st_01a093d0  
**Phase:** P5 Client API Surfaces & Multi-Turn Tool Integration (Responses Adapter)  
**Deliverables:** `crates/gateway/src/compat/responses.rs`, `crates/gateway/tests/devin_responses_adapter.rs`, `.omo/evidence/devin/p5-responses-adapter.md`  
**Status:** PASS (Green, 21/21 integration tests passing)

---

## 1. Documented Entrypoints (Ready for P5 Integration)

1. `responses_to_openai(req: &serde_json::Value) -> Result<serde_json::Value, ResponsesError>`
   - Converts OpenAI Responses input (`instructions`, `input` string or structured array, function tools, `tool_choice`, `max_output_tokens`) into standard OpenAI chat-completions shape for Devin Connect encoding.
   - Preserves historical tool calls (`function_call`), tool results (`function_call_output`), `is_error` flag, reasoning thinking/signatures, and base64 vision images.
   - Enforces strict validation: rejects unsupported server-side state (`previous_response_id`, `background=true`, `store=true`), remote image URLs, non-function tools, orphan/duplicate tool calls, and malformed JSON arguments without silent coercion.

2. `ResponsesStreamRenderer`
   - State machine translating incremental `CodexEvent` streams to canonical Responses SSE frames:
     - `response.created`: initial response envelope (`id`, `status: in_progress`, `model`)
     - `response.output_item.added` & `response.content_part.added`: message and function call declarations
     - `response.output_text.delta` & `response.reasoning_text.delta`: streamed textual deltas
     - `response.function_call_arguments.delta` & `response.function_call_arguments.done`: incremental arguments
     - `response.output_item.done`: completed output items with stable IDs (`msg_0`, `fc_1`, etc.)
     - `response.completed` / `response.incomplete` / `response.failed`: terminal frames
   - Methods: `new(model, created)`, `render(event) -> Vec<Bytes>`, `close_unterminated() -> Vec<Bytes>`, `current_response(&self) -> Value`, `into_response(self) -> Value`, `terminated(&self) -> bool`, `failure(&self) -> Option<&str>`.

3. `responses_response(events: &[CodexEvent], model: &str, created: i64) -> Result<serde_json::Value, String>`
   - Non-streaming aggregator: directly executes `ResponsesStreamRenderer` over event slice and extracts final response object.
   - Guarantees 100% structural and lifecycle equivalence between stream and non-stream outputs without parallel duplicate code.

4. `ResponsesError`
   - Enum with `InvalidRequest` and `Unsupported` variants; provides `to_json_value()` producing standard OpenAI `invalid_request_error` JSON objects.

---

## 2. Test Execution & RED / GREEN Evidence

### Real RED Exit (Before Implementation)
```text
$ cargo test -p mahoquot-gateway --test devin_responses_adapter
   Compiling mahoquot-gateway v0.1.0 (/Volumes/T9-Mac/project/mahoquot-proxy/crates/gateway)
error: couldn't read `crates/gateway/tests/../src/compat/responses.rs`: No such file or directory (os error 2)
  --> crates/gateway/tests/devin_responses_adapter.rs:12:1
   |
12 | mod responses;
   | ^^^^^^^^^^^^^^

error: could not compile `mahoquot-gateway` (test "devin_responses_adapter") due to 1 previous error
Exit Code: 101
```

### Real GREEN Exit (After Minimal Implementation)
```text
$ cargo test -p mahoquot-gateway --test devin_responses_adapter
   Compiling mahoquot-gateway v0.1.0 (/Volumes/T9-Mac/project/mahoquot-proxy/crates/gateway)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 4.72s
     Running tests/devin_responses_adapter.rs (target/debug/deps/devin_responses_adapter-a4fc59b4d467d8c2)

running 21 tests
test test_request_unsupported_remote_stateful_and_background_features_rejected ... ok
test test_responses_error_json_structure_and_into_response ... ok
test test_request_instructions_and_string_input ... ok
test test_request_missing_call_id_or_name_rejected ... ok
test test_request_valid_multi_turn_tool_history ... ok
test test_request_malformed_tool_arguments_rejected_without_silent_coercion ... ok
test test_request_tool_history_is_error_flag_preservation ... ok
test test_request_unsupported_tool_types_rejected ... ok
test test_request_function_tools_and_tool_choice_mapping ... ok
test test_request_reasoning_signature_and_redaction_preservation ... ok
test test_request_orphan_or_duplicate_tool_calls_rejected ... ok
test test_request_structured_input_messages_and_parameters ... ok
test test_request_base64_vision_accepted_and_remote_url_rejected ... ok
test test_nonstream_incomplete_and_failure_handling ... ok
test test_streaming_usage_unknown_vs_zero ... ok
test test_streaming_output_limit_vs_failure ... ok
test test_streaming_reasoning_deltas_lifecycle ... ok
test test_streaming_lifecycle_tool_call_turn ... ok
test test_streaming_interleaved_tools_maintain_stable_ids_and_indices ... ok
test test_streaming_lifecycle_text_turn ... ok
test test_nonstream_json_equivalent_to_stream_lifecycle ... ok

test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
Exit Code: 0
```

---

## 3. Boundary & Contract Audit

- **Ownership & Isolation:** Strictly owns only `crates/gateway/src/compat/responses.rs`, `crates/gateway/tests/devin_responses_adapter.rs`, and `.omo/evidence/devin/p5-responses-adapter.md`. No edits to P3 files (`relay.rs`, `compat/mod.rs`, `usage.rs`) or P4 files.
- **Import Contract:** Module imports public `crate::compat::events::{CodexEvent, Usage}` without defining redundant copies. Integration test bridges via `#[path = "../src/compat/responses.rs"]`.
- **Usage Contract:** Unknown usage (`usage: None`) yields `null` (never fabricated zeros); exact usage (`usage: Some(...)`) maps actual token counters.
- **Limit vs Failure:** `OutputLimitReached` maps to `response.incomplete` with `incomplete_details: {"reason": "max_output_tokens"}`; `Failed` maps to `response.failed` with error envelope.
- **No Parallel Aggregate:** Non-stream aggregation invokes `ResponsesStreamRenderer` directly.

# P5 Adapter Verification Report: Client API Surfaces

**Date:** 2026-09-12  
**Task ID:** st_01a093dc  
**Repository:** `/Users/indo/code/project/mahoquot-proxy`  
**Phase:** P5 Client API Surfaces (Adapter Layer Verification)  
**Assigned Model:** `mahoquot/gemini-3.8-flash-high`  
**Overall Verdict:** FAIL (1/3 PASS, 2/3 PRODUCER UNFULFILLED)

---

## 1. Multi-Target Test Execution & RED Evidence

```text
$ cd /Users/indo/code/project/mahoquot-proxy && cargo test -p mahoquot-gateway --test devin_responses_adapter --test devin_messages_adapter --test devin_gemini_adapter
error: no test target named `devin_messages_adapter` in `mahoquot-gateway` package
help: available test targets:
    anthropic_callback_contract
    catalog_lkg_recovery
    config_endpoints_tests
    devin_catalog
    devin_credentials
    devin_relay
    devin_responses_adapter
    devin_wire
    devin_wire_fixtures
    ...
Exit Code: 101
```
*Finding:* Execution returned actual failure Exit 101 unchanged. Producer tasks for Messages (`st_01a093d1`) and Gemini (`st_01a093d2`) aborted at turn 2 without implementing deliverables or test targets.

---

## 2. Adapter-by-Adapter Source & Contract Audit

| Adapter Surface | Producer Task & Status | Test Target & Status | Source Artifact | Verdict |
|---|---|---|---|---|
| **OpenAI Responses** | `st_01a093d0` (Completed) | `devin_responses_adapter` (21/21 PASS) | `compat/responses.rs` | **PASS** |
| **Anthropic Messages** | `st_01a093d1` (Aborted turn 2) | `devin_messages_adapter` (MISSING) | `compat/claude.rs` | **FAIL** |
| **Gemini v1beta** | `st_01a093d2` (Aborted turn 2) | `devin_gemini_adapter` (MISSING) | `compat/gemini.rs` | **FAIL** |

### 2.1 OpenAI Responses Adapter (`compat/responses.rs`) - PASS
- **Test Command:** `cargo test -p mahoquot-gateway --test devin_responses_adapter` (21 passed, 0 failed, 0.00s, Exit 0).
- **LSP Diagnostics:** 0 errors, 0 warnings on `compat/responses.rs` and `tests/devin_responses_adapter.rs`.
- **Event Schemas & IDs:** Strictly emits canonical lifecycle (`response.created`, `response.output_item.added`, `response.content_part.added`, `response.output_text.delta`, `response.function_call_arguments.delta/done`, `response.output_item.done`, `response.completed`/`incomplete`/`failed`). Emits stable IDs (`resp_{created}`, `msg_{idx}`, `fc_{idx}`).
- **Multi-Turn & Preservation:** Preserves `function_call` and `function_call_output` turns, propagates `is_error: true`, preserves reasoning text, signature, and redaction flags.
- **Rejection Contracts:** Enforces explicit HTTP 400 `ResponsesError::invalid_request` / `unsupported` for `previous_response_id`, `background=true`, `store=true`, remote image URLs, non-function tools, orphan tool outputs, duplicate call IDs, and malformed argument JSON (no silent coercion).
- **Stream/Non-Stream Parity:** `responses_response()` reuses `ResponsesStreamRenderer` state machine directly, guaranteeing identical IDs and payload structures.
- **Test Coverage Inspection:** Non-vacuous. Tests assert exact machine-consumed JSON fields, error message substrings, stable IDs, and token counter mappings (`Usage::None` -> `null` vs 0-token counters).

### 2.2 Anthropic Messages Adapter (`compat/claude.rs`) - FAIL (Gaps Unresolved)
- **Producer Status:** Task `st_01a093d1` delivered no report and no test file `tests/devin_messages_adapter.rs`.
- **Unresolved Source Gaps:**
  1. `anthropic_to_openai()` strips `is_error` from `tool_result` blocks; Devin protobuf `tool_result_is_error` is never set.
  2. `anthropic_to_openai()` drops `thinking` blocks and signatures from assistant history.
  3. Non-streaming collector in `compat/mod.rs:550` drops `CodexEvent::Failed` silently and corrupts interleaved tool arguments (`tool_calls.last_mut()`).

### 2.3 Gemini v1beta Adapter (`compat/gemini.rs`) - FAIL (Gaps Unresolved)
- **Producer Status:** Task `st_01a093d2` delivered no report and no test file `tests/devin_gemini_adapter.rs`.
- **Unresolved Source Gaps:**
  1. No `gemini_to_openai()` inbound normalizer exists for `contents`, `systemInstruction`, `functionCall`, or `functionResponse`.
  2. `relay.rs:1186` leaves `openai_body = None` for `RelayMode::GeminiNative`.
  3. `relay.rs:517` hard-gates `RelayMode::GeminiNative` to `ProviderKind::Antigravity`, rejecting Devin.
  4. Thought signature round-trip via `signature_ledger` is not wired for Devin.

---

## 3. Scope & Integration Status

- **Product Edits:** 0 files modified during verification (read-only audit).
- **P5 Status:** Incomplete. Responses adapter is code-ready for integration, but Messages and Gemini adapters require re-execution of their respective producer tasks before Phase P5b relay integration.

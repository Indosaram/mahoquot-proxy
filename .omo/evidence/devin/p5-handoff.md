# Plan P5 Handoff: Client API Surfaces & Multi-Turn Tool Integration

**Phase:** P5 Client API Surfaces & Multi-Turn Tool Integration  
**Date:** 2026-09-12  
**Status:** Pre-implementation Handoff & Technical Seam Audit  
**Assigned Model:** Gemini (`mahoquot/gemini-3.8-flash-high`)  
**Context:** P3 (`relay.rs`, `url.rs`, `compat/mod.rs`, `usage.rs`, `request_history.rs`) and P4 (`account.rs`, `runtime_state.rs`, `state.rs`, `metrics.rs`, `management/*`, `proxy_policy.rs`) active. No product edits, no test runs, no subdelegation in this task.

---

## 1. Executive Summary & Verification Boundary

Plan P5 expands the Devin LLM integration beyond raw chat completions to support all four primary client surfaces:
1. **OpenAI Responses API** (`/v1/responses`, `/responses`, `/backend-api/codex/responses`)
2. **Anthropic Messages API** (`/v1/messages`, `/messages`)
3. **Google Gemini v1beta API** (`/v1beta/models/{model}:generateContent`, `:streamGenerateContent`)
4. **Legacy OpenAI Text Completions** (`/v1/completions`, `/completions`)

### Critical P3/P4 Baseline Caveat
Existing green test suites (`devin_relay` with 14 tests, `devin_wire` with 40 tests) verify **only** OpenAI chat completions transport, basic wire serialization, and raw passthrough rejection. They do **not** constitute proof of multi-surface support:
- Anthropic streaming eagerly records success before frames arrive and ignores Connect errors.
- Anthropic non-streaming returns false HTTP 200 success on upstream stream errors and discards token usage.
- Anthropic non-streaming corrupts interleaved tool call arguments.
- Responses API is hard-rejected (`"Devin does not support native/responses relay mode yet"`).
- Gemini API is hard-gated to Antigravity accounts and lacks an inbound request normalizer.
- P3 lifecycle repairs are currently pending in `relay.rs` and `compat/mod.rs`.

P5 implementation must separate **isolated adapter development** from **final relay integration** to prevent merge collisions with ongoing P3/P4 work.

---

## 2. Lead Seam Findings (Product Defects & Gaps)

### Finding 1: Anthropic Stream Branch Eager Success & Missing Outcome
- **Location:** `crates/gateway/src/relay.rs:1858-1884`
- **Defect:**
  ```rust
  if plan.mode == RelayMode::Anthropic && plan.client_stream {
      member.record_ok();
      state.metrics.served.fetch_add(1, Ordering::Relaxed);
      state.router.feedback(member.id(), Outcome::Success);
      let upstream_capture = Arc::new(std::sync::Mutex::new(None));
      let body = compat::streaming_body(compat::StreamingBodyParams {
          first,
          upstream: stream,
          model,
          created,
          include_usage: false,
          shape: compat::ReplyShape::Anthropic,
          session,
          upstream_capture: Some(Arc::clone(&upstream_capture)),
          devin_outcome: None, // <-- Hardcoded None
      });
      ...
      response.extensions_mut().insert(upstream_capture);
      // <-- devin_outcome is never inserted into response extensions!
      return Ok(response);
  }
  ```
- **Consequence:**
  1. `member.record_ok()`, metrics increment, and router feedback are dispatched **eagerly** before a single downstream byte is delivered.
  2. `devin_outcome: None` is supplied to `StreamingBodyParams`, so the decoder's EndStream outcome is ignored during streaming.
  3. `devin_outcome` is omitted from `response.extensions_mut()`, causing `relay.rs:2458` (`StreamedOutcome`) to receive `None`.
  4. If Devin upstream returns an HTTP 200 Connect error envelope (`resource_exhausted`, `unauthenticated`, `permission_denied`), account health is not updated to `Cooldown` or `AuthFailed`, monitor errors are not recorded, and the failed request is permanently classified as successful.

### Finding 2: Anthropic Non-Streaming False Success & Missing Usage Capture
- **Location:** `crates/gateway/src/relay.rs:1886-1897` and `crates/gateway/src/compat/mod.rs:550-575`
- **Defect:**
  In `relay.rs:1886`:
  ```rust
  if plan.mode == RelayMode::Anthropic {
      let raw = compat::collect_stream(first, stream).await?;
      member.record_ok();
      state.metrics.served.fetch_add(1, Ordering::Relaxed);
      state.router.feedback(member.id(), Outcome::Success);
      return Ok(compat::anthropic_response(&raw, &model, created, protocol, plan.client_stream));
  }
  ```
  In `compat/mod.rs:550-575` (`anthropic_response`):
  ```rust
  for event in events {
      match event {
          CodexEvent::TextDelta(t) => text.push_str(&t),
          CodexEvent::ReasoningSignature(sig) => reasoning_signature = Some(sig),
          CodexEvent::Completed { usage: u } => usage = u,
          CodexEvent::OutputLimitReached => finish = "length",
          CodexEvent::ToolCallBegin { call_id, name, .. } => ...
          CodexEvent::ToolArgsDelta { delta, .. } => ...
          _ => {} // <-- CodexEvent::Failed is silently ignored!
      }
  }
  ```
- **Consequence:**
  1. `collect_stream` does not capture usage or `devin_outcome` (unlike `collect_stream_with_replies`).
  2. `record_ok()` is called eagerly before examining stream events.
  3. If Devin returns an error, `ProtocolParser` emits `CodexEvent::Failed { message }`. In `anthropic_response`, `CodexEvent::Failed` matches `_ => {}` and is dropped. The function proceeds to build a 200 OK `claude::messages_payload` with whatever partial text was gathered (or empty text).
  4. The returned response lacks the `upstream_capture` extension. In `relay.rs:2421`, buffered body usage extraction calls `extract_response_token_usage`, which requires either `prompt_tokens` or `input_tokens_details.cached_tokens`. Anthropic payloads emit only top-level `input_tokens` and `output_tokens`, so extraction returns `None`. Zero tokens are recorded in `usage_events` and request history.

### Finding 3: Interleaved Tool Call Argument Corruption in `anthropic_response`
- **Location:** `crates/gateway/src/compat/mod.rs:564-573`
- **Defect:**
  ```rust
  CodexEvent::ToolCallBegin { call_id, name, .. } => {
      if finish != "length" {
          finish = "tool_calls";
      }
      tool_calls.push((call_id, name, String::new()));
  }
  CodexEvent::ToolArgsDelta { delta, .. } => {
      if let Some(last) = tool_calls.last_mut() {
          last.2.push_str(&delta);
      }
  }
  ```
- **Consequence:**
  `ToolArgsDelta` ignores `output_index`. If Devin streams interleaved tool calls (e.g. Call 0 begin, Call 1 begin, Call 0 chunk, Call 1 chunk), Call 0's argument delta is appended to Call 1. Arguments for earlier tools are truncated or corrupted, and later tools receive invalid combined JSON.

### Finding 4: Contrast with Generic Chat / Gemini Stream Branch
- **Location:** `crates/gateway/src/relay.rs:1900-1950`
- **Observation:**
  The Chat / Gemini branch already demonstrates the correct pattern:
  - Streaming: Allocates `devin_outcome` cell when `protocol == Protocol::Devin`, omits eager `record_ok`, passes the cell to `StreamingBodyParams`, and inserts it into `response.extensions_mut()`.
  - Non-streaming: Calls `compat::collect_stream_with_replies(first, stream, session).await?`, inspects `devin.error_code` and `!devin.terminated`, returns early errors if failed, clears errors on success, and inserts `upstream_usage` into extensions.
  - Anthropic stream and nonstream branches must be refactored to mirror this pattern.

### Finding 5: Hard Rejection of Responses Surface
- **Location:** `crates/gateway/src/relay.rs:545`, `:1775`, and `crates/gateway/src/cp_routes.rs:358`
- **Defect:**
  - `cp_routes.rs:358`: For any non-Google model (including Devin), routes to `handle_relay(..., RelayMode::Native, CODEX_RESPONSES_PATH, ...)`.
  - `relay.rs:545`: `if matches!(plan.mode, RelayMode::Native) { return Err("Devin does not support native/responses relay mode yet".to_string()); }`
  - `relay.rs:1775`: Same hardcoded rejection in `finish_success`.
  - `crates/gateway/src/compat/responses.rs` does **not** exist (currently proposed/absent).
  - No streaming or non-streaming Responses event translation is wired for Devin.

---

## 3. Surface-by-Surface Transformation & Seam Mapping

### 3.1 OpenAI Responses Surface

#### Inbound Endpoints & Aliases (`routes.rs` / `cp_routes.rs`)
| Inbound Path | HTTP Method | Gateway Handler | Relay Mode |
|---|---|---|---|
| `/v1/responses` | POST | `cp_routes::responses` | Currently `RelayMode::Native` (hard-rejected for Devin) |
| `/responses` | POST | `cp_routes::responses` | Currently `RelayMode::Native` (hard-rejected for Devin) |
| `/backend-api/codex/responses` | POST | `routes.rs::codex_responses_handler` | Direct `RelayMode::Native` (bypasses `cp_routes::responses`) |
| `/v1/responses/compact` | POST | `cp_routes::responses_compact` | 501 `NOT_IMPLEMENTED` for non-openai |
| `/responses/compact` | POST | `cp_routes::responses_compact` | 501 `NOT_IMPLEMENTED` for non-openai |
| `/backend-api/codex/responses/compact` | POST | `cp_routes::responses_compact` | 501 `NOT_IMPLEMENTED` for non-openai |
| `/v1/alpha/search`, `/alpha/search` | POST | `cp_routes::alpha_search` | 503 `SERVICE_UNAVAILABLE` for non-openai |
| `/v1/responses`, `/responses` | GET | `cp_routes::ws_upgrade` | 101 `UPGRADE_REQUIRED` (close handshake) |

#### Rejection Seams
Responses API features that require server-side state or unsupported capabilities must be explicitly rejected with HTTP 400 `invalid_request_error`:
1. `previous_response_id`: Devin does not store remote server-side turn state. Must reject:
   `{"error": {"type": "invalid_request_error", "message": "previous_response_id is not supported for Devin models"}}`
2. `background: true`: Background execution requires asynchronous persistent job queues. Must reject:
   `{"error": {"type": "invalid_request_error", "message": "background=true is not supported"}}`
3. `store: true`: Upstream Devin does not persist response objects. Must reject or force `store: false`.
4. Unsupported tool types: `web_search`, `file_search`, `computer` must be rejected; only `function` tools are valid.

#### Request Normalization (Responses -> Devin Protobuf)
- Top-level `instructions` (string) -> `ChatMessagePrompt` (system instructions).
- `input` (string or array):
  - Array item `type: "message"` -> user or assistant prompt.
  - Array item `type: "function_call"` -> assistant turn carrying tool call (`call_id`, `name`, `arguments`).
  - Array item `type: "function_call_output"` -> tool result turn (`tool_call_id`, `content`).
- Output limits: Map `max_output_tokens` -> `max_tokens` in `CompletionConfiguration`.

#### Response Decoder & Renderer (`compat/responses.rs`)
- **Streaming:** Implement `ResponsesStreamRenderer` emitting canonical SSE events:
  1. `response.created` (initial response object envelope)
  2. `response.output_item.added` (for text message or function call item)
  3. `response.content_part.added`
  4. `response.output_text.delta` (from `CodexEvent::TextDelta`)
  5. `response.function_call_arguments.delta` (from `CodexEvent::ToolArgsDelta`)
  6. `response.output_item.done`
  7. `response.completed` (carrying `usage` from `CodexEvent::Completed`)
- **Non-Streaming:** Aggregate events into a single JSON response object:
  ```json
  {
    "id": "resp_...",
    "object": "response",
    "status": "completed",
    "model": "devin/...",
    "output": [
      {
        "id": "msg_0",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "..."}]
      },
      {
        "id": "fc_0",
        "type": "function_call",
        "call_id": "...",
        "name": "...",
        "arguments": "{...}"
      }
    ],
    "usage": { "input_tokens": ..., "output_tokens": ..., "total_tokens": ... }
  }
  ```

---

### 3.2 Anthropic Messages Surface

#### Inbound Endpoints & Aliases (`routes.rs`)
| Inbound Path | HTTP Method | Gateway Handler | Relay Mode |
|---|---|---|---|
| `/v1/messages` | POST | `routes.rs::messages_handler` | `RelayMode::Anthropic` |
| `/messages` | POST | `routes.rs::messages_handler` | `RelayMode::Anthropic` |
| `/v1/messages/count_tokens` | POST | `routes.rs::count_tokens_handler` | Handled locally via `estimate_input_tokens` (if `ModelCapability::CountTokens`) |
| `/messages/count_tokens` | POST | `routes.rs::count_tokens_handler` | Same as above |

#### Conversion & Rejection Seams in `compat/claude.rs`
1. **Thinking & Reasoning Loss:**
   - In `anthropic_to_openai`: Blocks with `type: "thinking"` are ignored in `match block.get("type")`. Assistant turn history that previously emitted thinking/signatures is stripped before reaching Devin.
   - In `compat/mod.rs:550` (`anthropic_response`): `CodexEvent::ReasoningDelta` is ignored in the non-stream event loop. `messages_payload` only injects signature with an empty string: `{"type": "thinking", "thinking": "", "signature": sig}`.
2. **Tool Result `is_error` Loss:**
   - In `anthropic_to_openai:185`: `is_error` in `tool_result` blocks is not copied into the transformed OpenAI tool message. `build_chat_request` checks `message.get("is_error")`; because it was stripped, Devin's `prompt.tool_result_is_error` is never set.
3. **Interleaved Tool Arguments:**
   - `anthropic_response` must track tool calls by `output_index` or ID instead of `tool_calls.last_mut()`.
   - `AnthropicStreamRenderer` (`compat/claude.rs:910`) must support interleaved tool deltas by output index without closing the preceding tool prematurely.

---

### 3.3 Google Gemini v1beta Surface

#### Inbound Endpoints & Actions (`routes.rs` / `cp_routes.rs`)
| Inbound Path | HTTP Method | Action / Verb | Relay Mode |
|---|---|---|---|
| `/v1beta/models/{model}:generateContent` | POST | `GeminiAction::Generate` | `RelayMode::GeminiNative` (`stream: false`) |
| `/v1beta/models/{model}:streamGenerateContent` | POST | `GeminiAction::StreamGenerate` | `RelayMode::GeminiNative` (`stream: true`) |
| `/v1beta/models/{model}:countTokens` | POST | `GeminiAction::CountTokens` | `RelayMode::GeminiCountTokens` |
| `/models/{model}:*` | GET / POST | Same actions | Same as above |

#### Conversion & Rejection Seams
1. **Hard Account Provider Gate:**
   - `relay.rs:517` requires `member.kind() == ProviderKind::Antigravity` for `RelayMode::GeminiNative`. Non-Antigravity requests fail with `"gemini-native requests need an antigravity account"`.
2. **Missing Inbound Request Normalizer:**
   - `build_plan(RelayMode::GeminiNative)` (`relay.rs:1186`) leaves `openai_body = None`.
   - Devin requires `openai_body_with_model(plan, upstream_model)` in `resolve_target`.
   - P5 must implement `gemini_to_openai` in `compat/gemini.rs` to convert:
     - `contents[].parts[].text` -> `role: "user"` / `role: "assistant"`
     - `contents[].parts[].functionCall` -> assistant `tool_calls`
     - `contents[].parts[].functionResponse` -> `role: "tool"`
     - `systemInstruction` -> `role: "system"`
     - `generationConfig.temperature`, `topP`, `maxOutputTokens` -> standard parameters
3. **Thought Signature Round-Trip:**
   - Gemini embeds `thoughtSignature` on `functionCall` parts or standalone parts.
   - `compat/signature_ledger.rs` (`remember` and `recall`) must be leveraged so signatures are preserved across two-turn tool execution.

---

### 3.4 Legacy OpenAI Completions Surface

#### Inbound Endpoints (`routes.rs`)
| Inbound Path | HTTP Method | Gateway Handler | Relay Mode |
|---|---|---|---|
| `/v1/completions` | POST | `routes.rs::completions_handler` | `RelayMode::LegacyCompletions` |
| `/completions` | POST | `routes.rs::completions_handler` | `RelayMode::LegacyCompletions` |

#### Conditional Support & Limitations
- `completions_handler` lifts `prompt` (string or string array) into `messages: [{"role": "user", "content": prompt}]`.
- **Non-Streaming:** Uses `Aggregator::into_text_completion`, producing `{"object": "text_completion", "choices": [{"text": "...", "finish_reason": "stop"}]}`. Fully supported.
- **Streaming Limitation:** In `compat/mod.rs:345`, `ReplyShape::TextCompletion` currently reuses `ChunkRenderer`, which emits `chat.completion.chunk` (with `delta.content`) instead of legacy text chunks (with `text`).
- **Conditional Restrictions:** Multi-turn tools, structured function calls, and multimodal images have no representation in legacy completions and must be rejected if requested on `/v1/completions`.

---

## 4. Reasoning, Signature, Redaction & Tool Preservation Matrix

| Feature | OpenAI Chat (`/v1/chat/...`) | Anthropic Messages (`/v1/messages`) | Gemini Native (`/v1beta/...`) | OpenAI Responses (`/v1/responses`) |
|---|---|---|---|---|
| **Reasoning Delta** | Supported via `delta.reasoning_content` and non-stream `reasoning_content` | Stream: `thinking_delta`. **Non-stream: DEFECTIVE (dropped in `anthropic_response`)** | Stream: `part.thought = true`. Non-stream: candidate text parts | **Missing (needs Responses renderer)** |
| **Reasoning Signature** | Preserved in `Aggregator`, not rendered in OpenAI chunk/JSON | Stream: `signature_delta`. Non-stream: `messages_payload` with empty thinking | `part.thoughtSignature` via `signature_ledger` | **Missing (needs Responses renderer)** |
| **Reasoning Redacted** | State marker in `CodexEvent`, ignored in renderer | Dropped (`CodexEvent::ReasoningRedacted => {}`) | Dropped in renderer | **Missing (needs Responses renderer)** |
| **Tool Call Begin & Args** | Stable indices via `slot(output_index)`, arg chunks streamed | Stream: `tool_use` + `input_json_delta`. **Non-stream: DEFECTIVE (interleaved corrupts last tool)** | Stream: buffered in `open_calls`, emitted on terminal STOP | **Missing (needs Responses renderer)** |
| **Tool History Replay (Turn 2)** | Assistant `tool_calls` + role `tool` mapped to proto `source: 2` & `source: 4` | **DEFECTIVE: `is_error` and `thinking` dropped in `anthropic_to_openai`** | **DEFECTIVE: No inbound request converter for `functionResponse`** | **DEFECTIVE: Hard-rejected in `relay.rs`** |

---

## 5. Owned Files & Dependency Separation

To guarantee clean parallel execution without conflicting with active P3/P4 work, P5 work must be split into two sequential phases:

```
┌─────────────────────────────────────────────────────────────┐
│ Phase P5a: Adapter & Codec Modules (P5 Exclusively Owns)   │
│ - crates/gateway/src/compat/responses.rs (NEW FILE)         │
│ - crates/gateway/src/compat/claude.rs                       │
│ - crates/gateway/src/compat/gemini.rs                       │
│ - crates/gateway/src/compat/render.rs                       │
└──────────────────────────────┬──────────────────────────────┘
                               │ (Awaits P3 release of relay.rs)
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ Phase P5b: Relay & Dispatch Integration                     │
│ - crates/gateway/src/relay.rs (Anthropic stream/nonstream)  │
│ - crates/gateway/src/cp_routes.rs (Responses routing)       │
│ - crates/gateway/src/compat/mod.rs (ReplyShape::Responses)  │
│ - crates/gateway/tests/devin_surfaces.rs (NEW TEST FILE)    │
└─────────────────────────────────────────────────────────────┘
```

### 5.1 Strictly Owned Files for Phase P5a (Adapter Layer)
These files can be created and modified immediately without merge hazards:
1. `crates/gateway/src/compat/responses.rs` (**NEW FILE**)
   - `responses_to_openai(req: &Value) -> Result<Value, String>`
   - `ResponsesStreamRenderer` (SSE frame builder for `response.*` events)
   - `responses_response(events: &[CodexEvent], model: &str, created: i64) -> Value`
   - Rejection logic for `previous_response_id`, `background`, `store`.
2. `crates/gateway/src/compat/claude.rs`
   - In `anthropic_to_openai`: Preserve `is_error` in tool results; preserve `thinking` blocks and signatures in assistant history.
3. `crates/gateway/src/compat/gemini.rs`
   - Add `gemini_to_openai(req: &Value) -> Result<Value, String>` converting `contents` and `systemInstruction` to chat messages.
4. `crates/gateway/src/compat/render.rs`
   - Add `into_responses(self) -> Value` on `Aggregator`.

### 5.2 Serialized Files for Phase P5b (Relay Integration)
These files are modified **only after P3 lifecycle repairs merge**:
1. `crates/gateway/src/relay.rs`
   - Refactor Anthropic stream branch (line 1858): thread `devin_outcome`, defer `record_ok`, insert into response extensions.
   - Refactor Anthropic nonstream branch (line 1886): use `collect_stream_with_replies`, handle `CodexEvent::Failed`, attach `upstream_usage`.
   - Update `RelayMode::Native` / Responses handling: route Devin Responses requests through the Responses plan rather than hard-rejecting.
   - Update `RelayMode::GeminiNative` in `resolve_target`: permit Devin provider and inject converted `openai_body`.
2. `crates/gateway/src/cp_routes.rs`
   - In `responses`: Update provider check so Devin routes to P5 Responses handling instead of falling into `CODEX_RESPONSES_PATH`.
3. `crates/gateway/src/compat/mod.rs`
   - Add `ReplyShape::Responses` to `enum ReplyShape`.
   - Update `anthropic_response`: fix `ToolArgsDelta` interleaved tool corruption and handle `CodexEvent::Failed`.
4. `crates/gateway/tests/devin_surfaces.rs` (**NEW TEST FILE**)
   - Dedicated integration suite testing all four client surfaces end-to-end.

---

## 6. Required Multi-Turn Verification Matrix for P5

Before P5 can be declared complete, the new test suite `crates/gateway/tests/devin_surfaces.rs` must verify the following scenarios against the mock harness (`scripts/devin-mock.mjs`):

### Scenario 1: Anthropic Messages 2-Turn Tool Flow & Thinking Preservation
1. Turn 1 (User asks question) -> Upstream streams thinking + `tool_use` -> Downstream client receives SSE `content_block_start(thinking)`, `content_block_delta(thinking_delta)`, `content_block_start(tool_use)`, `content_block_delta(input_json_delta)`.
2. Turn 2 (Client submits `tool_result` with `is_error: true` and replays assistant `thinking`) -> Gateway preserves `is_error` in protobuf `tool_result_is_error = true` and preserves thinking in assistant history -> Model emits final text answer.
3. Assert: No eager `record_ok` on stream start; late errors surface as Anthropic error events; usage is fully recorded in SQLite `usage_events`.

### Scenario 2: Anthropic Interleaved Multi-Tool Non-Streaming
1. Upstream delivers two simultaneous tool calls with interleaved argument deltas across chunks.
2. Gateway aggregates into non-streaming Anthropic JSON reply (`/v1/messages`).
3. Assert: Tool 0 and Tool 1 have distinct, correct arguments; no argument bleed or corruption.

### Scenario 3: Responses API 2-Turn Streaming & Rejection Contracts
1. Inbound POST `/v1/responses` with `previous_response_id: "resp_123"` -> Returns HTTP 400 with `invalid_request_error`.
2. Inbound POST `/v1/responses` with `background: true` -> Returns HTTP 400 with `invalid_request_error`.
3. Turn 1 (`input`: "Calculate sum") -> Gateway translates to Devin Connect -> Upstream emits `function_call` -> Downstream receives SSE `response.created`, `response.output_item.added`, `response.function_call_arguments.delta`, `response.completed`.
4. Turn 2 (`input` array with `function_call` and `function_call_output`) -> Gateway maps to tool exchange -> Upstream answers final text -> Downstream receives completed response object with correct `usage`.

### Scenario 4: Gemini v1beta 2-Turn Native Exchange
1. Inbound POST `/v1beta/models/devin/glm-5-2:streamGenerateContent` with `contents`.
2. Gateway normalizes `contents` to OpenAI chat shape, preserves thought signature in ledger, and calls Devin Connect.
3. Downstream client receives Gemini-formatted SSE `candidates[0].content.parts[0].functionCall`.
4. Turn 2 sends `functionResponse` -> Gateway matches signature via `signature_ledger::recall` and relays -> Downstream receives final text answer with `usageMetadata`.

### Scenario 5: Connect HTTP 200 Error Propagation Across All Surfaces
1. Mock upstream returns HTTP 200 with EndStream error payload (`"error": {"code": "resource_exhausted"}`).
2. Verify across all four surfaces (Chat, Messages, Gemini, Responses) that:
   - Stream yields surface-appropriate error frame (not normal completion).
   - Account health transitions to `Health::Cooldown`.
   - `member.record_fail()` increments.
   - Request history records HTTP 429 / failed outcome.

---

## 7. Exit State & Hand-off Verification

- **Report Path:** `/Users/indo/code/project/mahoquot-proxy/.omo/evidence/devin/p5-handoff.md`
- **Product Edits:** 0 files modified.
- **Commands Executed:** None (read-only analysis).
- **Stop Condition:** Handoff report successfully written and verified at destination path.

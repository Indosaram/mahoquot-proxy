# Devin Connect Wire Contract (Plan P0)

**Document Version:** 1.1.0  
**Date:** 2026-09-12  
**Status:** Pinned Specification & Golden Contract (Plan P0)  
**Classification:** Reference Implementation & Wire Parity  

---

## 1. Overview & Purpose

This document defines the wire contract for integrating Devin as a native LLM provider in `mahoquot-proxy`, corresponding to phase **P0** of `.omo/plans/devin-provider-integration.md`.

Devin uses **Connect RPC** (over HTTP/1.1 or HTTP/2) with Protocol Buffers (`proto2`) syntax against `https://server.codeium.com`. It provides two primary RPC endpoints:
1. `GetCascadeModelConfigs` (Unary RPC): Model catalog discovery and capability querying.
2. `GetChatMessage` (Server-Streaming RPC): Chat completions, reasoning (thinking), tool use, and token usage telemetry.

This specification serves as the authoritative contract for downstream implementation stages:
- **P1**: Credential validation, storage, and identity isolation.
- **P2**: Wire codec, envelope framing, and prost message definitions.
- **P3**: Chat relay, streaming transform, cancellation, and error translation.
- **P4**: Dynamic model catalog discovery, capability filtering, and account pooling.
- **P5**: OpenAI Chat, Anthropic Messages, and Gemini surface adaptations.
- **P6**: Management API, usage tracking, and Monitor UI integration.

---

## 2. Provenance & Upstream References

The wire definitions and behaviors specified herein are derived strictly from fixed upstream commits:

- **Primary Source:** [`Arborsm/dsh-plugin-devin-bridge`](https://github.com/Arborsm/dsh-plugin-devin-bridge)  
  **Pinned Commit:** `ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4`  
  **Key Files:**
  - `src/proto/devin.proto`: Wire protobuf definitions with explicit field numbers.
  - `src/adapter/transport.ts`: Connect transport setup, HTTP headers, proxy logic.
  - `src/adapter/devin.ts`: Model catalog fetch, request construction, metadata injection.
  - `src/adapter/decoder.ts`: Streaming response frame decoder, usage extraction, error codes.
  - `LICENSE`: MIT License (reproduced below).
- **Secondary Source:** [`sotayamashita/devin-sse-proxy`](https://github.com/sotayamashita/devin-sse-proxy)  
  **Pinned Commit:** `7672e2ddf6eaf4aac7bda8f74071e804e7efbadd`  
  *Finding:* This repository provides an SSE bridge for Devin MCP tools (`https://mcp.devin.ai/sse`), which is deprecated upstream and fundamentally different from the LLM provider transport. It is **not** used for the LLM wire protocol.

### Upstream MIT License Reproduction

```text
MIT License

Copyright (c) 2026 Arborsm

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

---

## 3. Verified vs. Unverified Boundaries

To ensure absolute engineering rigor, the contract distinguishes between what has been verified from source and what remains unverified/experimental:

### Verified from Pinned Upstream Source Code
1. **RPC Methods:** Exact service and method names under `exa.api_server_pb.ApiServerService`.
2. **Authorization Header:** Literal `Authorization: Basic <token>-<token>` where `<token>` is repeated with an ASCII hyphen; it is **not** a base64-encoded `user:password` pair.
3. **Dual Credential Delivery:** The exact same token is transmitted in both the HTTP `Authorization` header and the binary protobuf field `Metadata.api_key` (field 1 -> field 3).
4. **Protobuf Field Numbers & Types:** Preserved verbatim from `src/proto/devin.proto` using `proto2` optional fields.
5. **Framing Differences:** Unary calls use unframed binary protobuf (`application/proto`); streaming calls use Connect 5-byte envelope framing (`application/connect+proto`).
6. **Connect Error Protocol:** HTTP status code is 200; RPC terminal errors arrive in the EndStreamResponse frame (flag `0x02`) encoded as JSON. Errors may be code-only (e.g. `{"error":{"code":"resource_exhausted"}}`) without a `message` string.
7. **Model UID Identification:** Upstream uses model identifiers like `glm-5-2` and `swe-1-7` in request field `chat_model_uid` (field 21) and response field `actual_model_uid` (field 23).
8. **Token Usage Semantics:** `ModelUsageStats` (field 7) is an authoritative point-in-time snapshot containing `input_tokens`, `output_tokens`, `cache_write_tokens`, and `cache_read_tokens`. Total tokens = `input_tokens + output_tokens`. Cache tokens are breakdowns, not additive.
9. **Reasoning Semantics:** Thinking content (`delta_thinking`), opaque cryptographic signature (`delta_signature`), and redaction flag (`thinking_redacted`) precede output text deltas.
10. **String Validity Invariant:** Protobuf `string` fields require valid UTF-8 per protobuf specifications. Every individual message frame contains complete, valid UTF-8 strings. Slicing incomplete codepoints across message frames is malformed protobuf.
11. **Transport Chunking Invariant:** Connect transport byte streams may be split at arbitrary byte boundaries across network packets; streaming assemblers must buffer until complete 5-byte headers and declared payload lengths are received.

### Unverified / Experimental
1. **Live Devin Server Compatibility:** Live calls to `https://server.codeium.com` were not executed during P0; compatibility rests on parity with the pinned reference commit.
2. **Account Entitlements & Server Quotas:** Actual account token allowances, rate limits, credit billing multipliers, and server-side timeouts are unmeasured and must be treated as `unknown`.
3. **Hard Context / Max Output Limits:** Upstream README mentions 128k/16k limits while source code specifies 200k/262k; proxy implementations must treat these as discovery observations rather than immutable truths.
4. **Image & Attachment Support:** Image transport exists in schema (`ExaCodeiumCommonPb_ImageData`) but live server behavior with large base64 payloads is unverified.

---

## 4. Endpoints & Transport Layer

### Service Base URL
- Default: `https://server.codeium.com`
- Overridable via account configuration `api_server_url`.

### RPC Paths
| RPC Method | Path | Protocol | Request Content-Type | Response Content-Type |
|---|---|---|---|---|
| `GetCascadeModelConfigs` | `/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs` | Unary | `application/proto` | `application/proto` |
| `GetChatMessage` | `/exa.api_server_pb.ApiServerService/GetChatMessage` | Server-Streaming | `application/connect+proto` | `application/connect+proto` |

### Required Headers
```http
POST /exa.api_server_pb.ApiServerService/GetChatMessage HTTP/1.1
Host: server.codeium.com
Authorization: Basic <token>-<token>
Connect-Protocol-Version: 1
Content-Type: application/connect+proto
Accept: application/connect+proto
```

#### Authorization Header Invariant
- Format: `Basic <token>-<token>`
- Construction: Prefix string `"Basic "` followed by `<token>`, ASCII hyphen `'-'`, and `<token>`.
- **Caution:** Never use `reqwest::RequestBuilder::basic_auth()` or standard HTTP basic auth encoders. Doing so base64-encodes the string and causes upstream authentication rejection.
- **Token Validation Invariant:** The token must consist of valid credential characters. If a token contains whitespace, newlines (`\r`, `\n`), or non-printable ASCII control characters, the proxy must reject it immediately with an explicit validation error rather than silently stripping or altering it. Silently mutating credentials can cause unexpected auth failures or hide configuration defects.

---

## 5. Framing Protocols

### 5.1 Unary Framing (`GetCascadeModelConfigs`)
- **Content-Type:** `application/proto`
- **Wire Format:** Direct, raw Protocol Buffer message bytes without any envelope prefix.
- **Request Body:** Direct serialization of `GetCascadeModelConfigsRequest`.
- **Response Body:** Direct serialization of `GetCascadeModelConfigsResponse`.
- The first byte of the wire payload is a standard protobuf tag (e.g., `0x0A` for field 1, wire type 2).

### 5.2 Server-Streaming Framing (`GetChatMessage`)
- **Content-Type:** `application/connect+proto`
- **Wire Format:** Connect Envelope Framing. Every frame consists of a 5-byte header followed by the payload:
  ```text
  +-----------------+------------------------------------+
  | Flag (1 byte)   | Length (4 bytes, Big-Endian uint32) |
  +-----------------+------------------------------------+
  | Payload (Length bytes)                               |
  +------------------------------------------------------+
  ```
- **Flag Byte Definitions:**
  - `0x00`: Data Frame (payload is binary Protocol Buffer message `GetChatMessageResponse`).
  - `0x01`: Compressed Data Frame (bit 0 set; uncompressed frames use `0x00`).
  - `0x02`: EndStreamResponse Frame (bit 1 set; payload is a UTF-8 JSON object indicating stream termination and optional error status).

#### Connect Stream Request
The client sends exactly one data frame (`flag = 0x00`) containing the serialized `GetChatMessageRequest` protobuf bytes.

#### Connect Stream Response & EndStreamResponse
The upstream server streams zero or more data frames (`flag = 0x00`), followed by exactly one terminal EndStreamResponse frame (`flag = 0x02`).
The EndStreamResponse payload is JSON-encoded:
```json
{
  "error": null,
  "hasSyncPoints": false
}
```
Or on error:
```json
{
  "error": {
    "code": "resource_exhausted"
  }
}
```
**CRITICAL:** The upstream HTTP status code is `200 OK` even when an RPC error occurs. The proxy must inspect the EndStreamResponse JSON to identify failures.

---

## 6. Protobuf Schema & Field Mapping

Protobuf definitions are located at `crates/gateway/src/compat/devin.proto`.

### 6.1 Shared Metadata (`ExaCodeiumCommonPb_Metadata`)
Included in every request message under field 1.
| Field # | Name | Type | Description |
|---|---|---|---|
| 1 | `ide_name` | string | Hardcoded client identifier (e.g., `"chisel"`) |
| 2 | `extension_version` | string | Hardcoded client version (e.g., `"3000.2.17"`) |
| 3 | `api_key` | string | Session authentication token (must match HTTP header) |
| 4 | `locale` | string | Locale string (e.g., `"en"`) |
| 5 | `os` | string | Operating system identifier (e.g., `"win"`, `"mac"`, `"linux"`) |
| 7 | `ide_version` | string | IDE version (e.g., `"3000.2.17"`) |
| 12 | `extension_name` | string | Extension identifier (e.g., `"chisel"`) |
| 31 | `f` | string | 366-byte random hex fingerprint (used in reference implementation) |

### 6.2 Model Discovery (`GetCascadeModelConfigs`)
#### Request: `GetCascadeModelConfigsRequest`
- Field 1: `metadata` (`ExaCodeiumCommonPb_Metadata`)

#### Response: `GetCascadeModelConfigsResponse`
- Field 1: `client_model_configs` (repeated `ExaCodeiumCommonPb_ClientModelConfig`)

#### Model Config: `ExaCodeiumCommonPb_ClientModelConfig`
| Field # | Name | Type | Description |
|---|---|---|---|
| 1 | `label` | string | Display label (e.g., `"GLM-5.2"`, `"SWE-1.7"`) |
| 3 | `credit_multiplier` | float | Credit multiplier / billing weight |
| 4 | `disabled` | bool | Whether the model is disabled |
| 5 | `supports_images` | bool | Image / multimodal input support |
| 7 | `is_premium` | bool | Premium account entitlement flag |
| 9 | `is_beta` | bool | Beta model flag |
| 10 | `provider` | enum (`ExaCodeiumCommonPb_ModelProvider`) | Model provider enum |
| 11 | `is_recommended` | bool | Recommended model flag |
| 15 | `is_new` | bool | New model flag |
| 18 | `max_tokens` | int32 | Context/token threshold (declared as `int32` in pinned proto) |
| 19 | `promo_status` | `ExaCodeiumCommonPb_PromoStatus` | Promotional status metadata |
| 20 | `is_capacity_limited` | bool | Capacity limit flag |
| 22 | `model_uid` | string | Canonical model identifier used in chat requests (e.g., `"glm-5-2"`) |
| 27 | `description` | string | Model description text |
| 30 | `model_family_metadata` | `ExaCodeiumCommonPb_ModelFamilyMetadata` | Model family metadata |

### 6.3 Chat Streaming (`GetChatMessage`)
#### Request: `GetChatMessageRequest`
| Field # | Name | Type | Description |
|---|---|---|---|
| 1 | `metadata` | `ExaCodeiumCommonPb_Metadata` | Client and auth metadata |
| 2 | `prompt` | string | System instructions / prompt |
| 3 | `chat_message_prompts` | repeated `ExaChatPb_ChatMessagePrompt` | Conversation history and user turn |
| 7 | `request_type` | enum (`ChatMessageRequestType`) | Request type (value `5` = `CHAT_MESSAGE_REQUEST_TYPE_CASCADE`) |
| 8 | `configuration` | `ExaCodeiumCommonPb_CompletionConfiguration` | Completion config: `num_completions`, `max_tokens`, `temperature`, `top_k`, `top_p` |
| 10 | `tools` | repeated `ExaChatPb_ChatToolDefinition` | Declared tools available for model invocation (field 10 in pinned proto) |
| 15 | `trajectory_reference` | `ExaCortexPb_CortexTrajectoryReference` | Cortex trajectory execution reference |
| 16 | `cascade_id` | string | Session / cascade conversation identifier |
| 20 | `planner_mode` | enum (`ExaCodeiumCommonPb_ConversationalPlannerMode`) | Planner mode (value `1` = `CONVERSATIONAL_PLANNER_MODE_DEFAULT`) |
| 21 | `chat_model_uid` | string | Model UID to invoke (e.g., `"glm-5-2"`) |
| 22 | `execution_id` | string | Optional client execution identifier |

#### Response: `GetChatMessageResponse`
| Field # | Name | Type | Description |
|---|---|---|---|
| 1 | `message_id` | string | Server message identifier |
| 2 | `timestamp` | `GoogleProtobuf_Timestamp` | Server-emitted response timestamp (`seconds`, `nanos`) |
| 3 | `delta_text` | string | Output content chunk |
| 5 | `stop_reason` | enum (`ExaCodeiumCommonPb_StopReason`) | Upstream termination reason code |
| 6 | `delta_tool_calls` | repeated `ExaCodeiumCommonPb_ChatToolCall` | Tool invocation chunk |
| 7 | `usage` | `ExaCodeiumCommonPb_ModelUsageStats` | Authoritative point-in-time token usage snapshot |
| 9 | `delta_thinking` | string | Reasoning / chain-of-thought delta chunk |
| 10 | `delta_signature` | string | Opaque reasoning verification signature |
| 11 | `thinking_redacted` | bool | Flag indicating reasoning was redacted |
| 23 | `actual_model_uid` | string | The actual upstream model UID executing the prompt |

#### Stop Reasons (`ExaCodeiumCommonPb_StopReason`)
| Code | Upstream Protobuf Identifier | Upstream Bridge FinishReason (`decoder.ts`) | Proxy Wire Mapping (OpenAI / Anthropic) | Semantics & Protocol Role |
|---|---|---|---|---|
| 0 | `ExaCodeiumCommonPb_StopReason_STOP_REASON_UNSPECIFIED` | `{ kind: 'stop' }` (default on stream end) | `stop` (`end_turn`) | Default intermediate streaming value; signals clean completion if stream closes without another reason code. |
| 1 | `ExaCodeiumCommonPb_StopReason_STOP_REASON_INCOMPLETE` | `{ kind: 'max-tokens' }` | `length` (`max_tokens`) | Generation halted before complete generation (incomplete output). |
| 3 | `ExaCodeiumCommonPb_StopReason_STOP_REASON_MAX_TOKENS` | `{ kind: 'max-tokens' }` | `length` (`max_tokens`) | Configured max tokens limit reached. |
| 9 | `ExaCodeiumCommonPb_StopReason_STOP_REASON_PARTIAL` | `{ kind: 'max-tokens' }` | `length` (`max_tokens`) | Partial content delivered (e.g. truncated generation). |
| 10 | `ExaCodeiumCommonPb_StopReason_STOP_REASON_FUNCTION_CALL` | `{ kind: 'tool-calls' }` | `tool_calls` (`tool_use`) | Generation stopped because the model emitted tool call(s) requiring client execution. |
| 13 | `ExaCodeiumCommonPb_StopReason_STOP_REASON_ERROR` | `{ kind: 'error', failure: ... }` | Error termination | Upstream generation error during stream processing. |

**Important Note on Enum Identifiers vs. Proxy Mappings:**
- Pinned `devin.proto` defines code 1 as `ExaCodeiumCommonPb_StopReason_STOP_REASON_INCOMPLETE` and code 9 as `ExaCodeiumCommonPb_StopReason_STOP_REASON_PARTIAL`. There is **no** `STOP_REASON_STOP` identifier in the Protocol Buffers schema.
- Normal clean completion is signaled in Connect streaming when the stream ends (EndStream `0x02` frame with `{"error": null}`) while `stop_reason` remained unspecified (`0`). In upstream `decoder.ts`, `mapFinishReason` defaults to `{ kind: 'stop' }`.
- Downstream proxy surfaces (OpenAI `finish_reason` and Anthropic `stop_reason`) map code 10 (`FUNCTION_CALL`) to `tool_calls`/`tool_use`, codes 1, 3, and 9 to `length`/`max_tokens`, code 13 to error, and default stream termination to `stop`/`end_turn`. Inventing a `STOP_REASON_STOP` enum in protobuf definitions is strictly forbidden.

#### Usage Statistics: `ExaCodeiumCommonPb_ModelUsageStats`
| Field # | Name | Type | Semantics |
|---|---|---|---|
| 2 | `input_tokens` | uint64 | Prompt tokens processed |
| 3 | `output_tokens` | uint64 | Completion tokens generated |
| 4 | `cache_write_tokens` | uint64 | Tokens written to upstream prompt cache |
| 5 | `cache_read_tokens` | uint64 | Tokens read from upstream prompt cache |
| 9 | `model_uid` | string | Model UID for usage attribution |

---

## 7. Edge Cases & Invariants for Downstream Implementation

### 7.1 Separation of Transport-Layer Chunking vs. Protobuf String Validity
- **Transport Layer Chunking:** The HTTP/TCP network layer may fragment the Connect wire stream at arbitrary byte offsets (e.g., across 5-byte envelope headers, within varints, or midway through payload bytes). Downstream network adapters must buffer incoming transport chunks and assemble complete Connect frames by declared length before dispatching to the message decoder.
- **Protobuf String Validity:** Within each framed protobuf message, all fields of type `string` (including `delta_text`, `delta_thinking`, `prompt`, etc.) **must be complete, valid UTF-8 strings**.
- **Fatal Rejection of Mid-Codepoint Splits in Protobuf:** Slicing a multi-byte UTF-8 codepoint across separate protobuf messages (e.g., placing the first byte of a Korean character in frame $N$ and continuation bytes in frame $N+1$) is **invalid protobuf syntax**. Standard decoders (such as Prost or Google Protobuf) reject such messages as fatal deserialization errors.
- **Contract Mandate:** Decoders must **never** expect or advertise concatenation across invalid string fragments. Positive fixtures (`chat_utf8_boundary.json`, `chat_response.json`) provide complete valid strings per frame (`"가"`, `"나"`). Malformed UTF-8 fragments are preserved strictly as a negative rejection test (`chat_utf8_malformed_negative.json`) that decoders must reject.

### 7.2 Reasoning Stream Sequencing & Signature Preservation
- Reasoning blocks precede text output:
  1. `delta_thinking` delivers incremental thinking content.
  2. `delta_signature` delivers an opaque cryptographic token string.
  3. `thinking_redacted: true` flags redacted reasoning.
  4. Subsequent `delta_text` signals the end of the reasoning block and start of visible assistant text.
- **Decoder Invariant:** The opaque signature must be preserved verbatim without modification; it is required for downstream multi-turn reasoning continuity.

### 7.3 Tool Calls & Explicit Identification Invariant
- Multiple tool calls can be concurrently active during generation.
- **Unambiguous Identification:** When multiple tool calls exist, every tool delta frame must provide an explicit `id` field (e.g., `call_001`, `call_002`) to indicate which call the delta arguments attach to.
- **Ambiguity as Protocol Error:** Omitting an `id` field when more than one tool call is active is ambiguous. Decoders must **not** guess attachment by heuristic (such as attaching to the last opened call or first call). An ID-less delta received while multiple calls are active must be rejected as an ambiguous tool call protocol error (see `chat_tools_ambiguous_negative.json`).
- Only when exactly one tool call is active may an ID-less delta be attached unambiguously.

### 7.4 Usage Snapshot Accumulation Rule
- `ModelUsageStats` is emitted as an **authoritative point-in-time snapshot**, not an incremental delta.
- **Accounting Invariant:** Downstream usage collectors must overwrite their usage tally with the latest snapshot, never sum multiple usage frames.
- **Total Calculation:** Total tokens = `input_tokens + output_tokens`. `cache_read_tokens` and `cache_write_tokens` are sub-categories of input tokens and must never be added to total tokens again.

### 7.5 Connect Code-Only Error Frames
- In Connect RPC, errors on HTTP 200 streams arrive in the `0x02` EndStreamResponse frame.
- Some upstream error envelopes contain only `{"error":{"code":"..."}}` without a `"message"` attribute.
- **Error Invariant:** The error parser must cleanly handle code-only error payloads (e.g. `resource_exhausted` -> `HTTP 429 Too Many Requests`), generating appropriate default messages.

### 7.6 Tag Checks Length Semantics Normalization
- All `tag_checks` entries in fixture metadata strictly follow the normalized convention where `len` represents the exact byte length of the delimited payload (the varint value in protobuf wire type 2, or the 4-byte BE uint32 in Connect frame headers).
- Validation must strictly navigate the declared frame index and field path rather than scanning for arbitrary byte matches across the stream.

### 7.7 Schema-Aware Protobuf Field Traversal vs. Heuristic Guessing
- Decoders and validators must determine field types from the authoritative schema (`devin.proto`), not by heuristic trial parsing of length-delimited payloads.
- **The Protobuf-Parseable Invalid UTF-8 Vulnerability:** Arbitrary byte sequences such as `[0x08, 0x80, 0x01]` are syntactically parseable as a valid protobuf message (field 1 = varint 128), but byte `0x80` is an invalid UTF-8 byte. A naive heuristic that treats any payload that parses as protobuf as a submessage will bypass UTF-8 string validation for string fields containing such bytes.
- **Invariant:** When a field is declared as `string` in schema (e.g. `delta_text` field 3 in `GetChatMessageResponse`), decoders must validate UTF-8 directly using `std::str::from_utf8` and must never treat it as a submessage. Negative fixture `chat_utf8_proto_parseable_negative.json` demonstrates and asserts this rejection.

### 7.8 Fixture Classification vs. Production Decoder Implementation
- Fixture classification (`positive` vs `negative`) categorizes golden wire contract definitions:
  - `positive`: Syntactically and semantically valid protocol interactions that conformant implementations must accept.
  - `negative`: Explicit protocol violations (ambiguous tool deltas, malformed UTF-8, proto-parseable invalid strings) that conformant implementations must reject.
- In phase P0, tests in `crates/gateway/tests/devin_wire_fixtures.rs` verify independent fixture assertions and schema-aware validation against these golden values; they do not test the production decoder (which will be implemented and tested in phase P2).

---

## 8. Golden Fixture Catalog

The following golden fixture files exist at `tests/data/devin/` and `crates/gateway/tests/data/devin/`. Each fixture is independently hand-specified from field definitions and verified in `crates/gateway/tests/devin_wire_fixtures.rs`.

| Fixture Name | RPC Method | Classification | Total Size | Framing / Frames | Key Assertions & Scenarios Tested |
|---|---|---|---|---|---|
| `unary_request.json` | `GetCascadeModelConfigs` | Positive | 15 B | Unframed `application/proto` | Unframed protobuf tag `0x0A`, literal `Basic dummy-token-dummy-token` header, metadata `api_key` matching header. Payload len: 15. |
| `unary_response.json` | `GetCascadeModelConfigs` | Positive | 58 B | Unframed `application/proto` | Repeated `ClientModelConfig`, varint `max_tokens` (200000, 262000), `model_uid`, `supports_images`, `is_premium`. Payload len: 58. |
| `chat_request.json` | `GetChatMessage` | Positive | 196 B | Framed `application/connect+proto` (1 frame) | 5-byte header flag `0x00`, payload len 191 (`0x000000BF`), metadata fields, system prompt, valid Korean prompt ("안녕하세요, 한 줄 소개를 작성해 주세요."), model UID `glm-5-2`. |
| `chat_response.json` | `GetChatMessage` | Positive | 389 B | Framed `application/connect+proto` (10 frames) | 9 data frames + 1 EndStream frame (payloads: [20, 16, 16, 32, 28, 13, 118, 30, 30, 36]). Text deltas, complete Korean ("가", "나"), thinking, signature, redaction, interleaved tools with explicit IDs, usage snapshot, stop reason 10, EndStream. |
| `chat_endstream_error.json` | `GetChatMessage` | Positive | 44 B | Framed `application/connect+proto` (1 frame) | Terminal error frame flag `0x02`, payload len 39 (`0x00000027`), code-only JSON `{"error":{"code":"resource_exhausted"}}`. |
| `chat_usage.json` | `GetChatMessage` | Positive | 76 B | Framed `application/connect+proto` (2 frames) | Isolated usage snapshot: flag `0x00` payload len 30 + EndStream payload len 36 (`{"error":null,"hasSyncPoints":false}`). Input 12, output 34, cache write 7, cache read 5, model UID `glm-5-2`. |
| `chat_reasoning.json` | `GetChatMessage` | Positive | 154 B | Framed `application/connect+proto` (5 frames) | 4 data frames + EndStream (payloads: [32, 28, 13, 20, 36]). Thinking delta, signature (`sig-opaque-0001`), `thinking_redacted: true`, followed by text delta (`Hello, `). |
| `chat_tools.json` | `GetChatMessage` | Positive | 247 B | Framed `application/connect+proto` (6 frames) | 5 data frames + EndStream (payloads: [47, 42, 32, 30, 30, 36], 6*5=30B headers + 217B payloads = 247B total). Interleaved calls (`call_001`, `call_002`) with explicit IDs on continuation frames 2 and 3; stop reason 10 (FUNCTION_CALL). |
| `chat_tools_ambiguous_negative.json` | `GetChatMessage` | Negative | 126 B | Framed `application/connect+proto` (3 frames) | Rejection fixture (payloads: [47, 42, 22]). 2 active calls with incoming ID-less delta; decoders must reject as ambiguous rather than guessing attachment. |
| `chat_utf8_boundary.json` | `GetChatMessage` | Positive | 83 B | Framed `application/connect+proto` (3 frames) | Complete valid UTF-8 strings per frame ("가", "나") + EndStream (payloads: [16, 16, 36]). Stream byte reassembly tested across arbitrary chunk splits (1B, 2B, 3B, 7B, 11B, 16B). |
| `chat_utf8_malformed_negative.json` | `GetChatMessage` | Negative | 22 B | Framed `application/connect+proto` (1 frame) | Rejection fixture (payload len 17). Delta_text contains incomplete codepoint bytes (`0xeab080eb`); decoders must reject as fatal UTF-8 deserialization error. |
| `chat_utf8_proto_parseable_negative.json` | `GetChatMessage` | Negative | 21 B | Framed `application/connect+proto` (1 frame) | Rejection fixture (payload len 16). Delta_text contains bytes `[0x08, 0x80, 0x01]` which parse as protobuf (field 1 = varint 128) but fail UTF-8 validation. Decoders must not bypass string checks. |


---

## 9. Downstream Implementation Dependencies

Downstream stages must adhere to the following contracts:
- **P1 (Credentials):** Parse `credentials.toml` from CLI path; validate token characters (rejecting invalid tokens containing whitespace, newlines, or control characters with an error instead of silently stripping or altering them); expose `authorization_header()` returning `("Authorization", "Basic <token>-<token>")`.
- **P2 (Codec):** Implement Connect framing encoder/decoder with Prost bindings generated from `crates/gateway/src/compat/devin.proto`. Reject malformed protobuf strings and ambiguous ID-less tool deltas.
- **P3 (Relay):** Ensure non-blocking stream parsing, error translation from EndStreamResponse JSON, and prompt cancellation handling.
- **P4 (Catalog):** Periodic invocation of `GetCascadeModelConfigs` with 5-minute cache TTL and account-specific model filtering.

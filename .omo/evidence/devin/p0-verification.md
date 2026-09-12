# Devin P0 — Independent Pinned Schema & Golden Fixture Verification Report

**Task ID:** `st_01a09317`  
**Verifier:** hephaestus (independent verification child agent)  
**Parent Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Root Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Model:** `mahoquot/gemini-3.8-flash-high` (exclusive execution, zero delegation)  
**Date:** 2026-09-12  
**Target Plan:** Plan P0 in `/Users/indo/code/project/mahoquot-proxy/.omo/plans/devin-provider-integration.md`  
**Deliverable Path:** `/Users/indo/code/project/mahoquot-proxy/.omo/evidence/devin/p0-verification.md`  

---

## 1. Executive Summary & Verification Verdict

### Verification Verdict: PASS

All Plan P0 artifacts (pinned schema, golden fixtures, contract documentation, and independent test harness) have been independently inspected, verified against the pinned upstream source commit `ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4`, audited against all prior lead rejection, lead audit, and final check findings, and validated via actual execution of `cargo test -p mahoquot-gateway --test devin_wire_fixtures` (exit code `0`, 15 passed, 0 failed, LSP clean).

### Summary of Audit & Verification Findings:

1. **Schema-Aware Field Traversal vs. Parse-Success Heuristic:**  
   - **Audited Defect:** `devin_wire_fixtures.rs` previously used a heuristic `audit_string_fields_recursive` that guessed string vs. submessage from parse success. Payload bytes `[0x08, 0x80, 0x01]` syntactically parse as a valid protobuf message (field 1 = varint 128), allowing malformed UTF-8 in string fields to evade validation.
   - **Resolution Verified:** Replaced heuristic with fixed schema-aware field traversal (`audit_fields_schema_aware`) based on declared `ProtoSchema` definitions matching `devin.proto`. String fields are strictly validated via `std::str::from_utf8`.
   - **Faithful Negative Test:** Verified `chat_utf8_proto_parseable_negative.json` and test `proto_parseable_invalid_utf8_rejected_negative_case`, proving that bytes `[0x08, 0x80, 0x01]` in `delta_text` parse as protobuf but are rejected with `"invalid UTF-8 in string field 3"`.

2. **Exact Path Navigation vs. Arbitrary Byte Occurrence Scanning:**  
   - **Audited Defect:** `tag_checks_length_semantics_normalized` previously scanned arbitrary matching bytes anywhere across the wire stream using `windows().any()`, allowing an invalid length at one path to pass if matching bytes occurred elsewhere in another field.
   - **Resolution Verified:** Replaced byte scanning with exact path-directed navigation (`assert_tag_check_exact_path`). The assertion walks the exact frame index, submessage path, tag bytes, length varint, and payload value declared in `path` (e.g. `frame6.field6.tag`, `metadata.field3.len`, `usage.field2.tag`).

3. **Recalculation of Measured Fixture Sizes:**  
   - **Audited Defect:** Stale reports misstated sizes (e.g. `chat_tools.json` claimed as 232 B instead of actual 247 B; `chat_response.json` claimed as 369 B instead of 389 B; `chat_tools_ambiguous_negative.json` claimed as 120 B instead of 126 B).
   - **Resolution Verified:** Recalculated all 12 fixture sizes directly from raw hex bytes:
     - `chat_tools.json`: 6 Connect frames (30 B headers) + payloads `[47, 42, 32, 30, 30, 36]` (217 B) = **247 B total**.
     - `chat_response.json`: 10 Connect frames (50 B headers) + payloads `[20, 16, 16, 32, 28, 13, 118, 30, 30, 36]` (339 B) = **389 B total**.
     - `chat_tools_ambiguous_negative.json`: 3 Connect frames (15 B headers) + payloads `[47, 42, 22]` (111 B) = **126 B total**.
     - Full catalog metrics across all 12 fixtures are documented in Section 4.

4. **Fixture Classification vs. Production Decoder Rejection:**  
   - Distinguishes clearly between P0 golden wire fixture specification/independent validation and P2 production decoder implementation. Negative tests in `devin_wire_fixtures.rs` verify that the fixture specification and independent validator correctly classify and reject protocol violations without depending on production prost decoders.

5. **No Speculative Cargo-Green Claims:**  
   - Removed speculative future build claims. The actual invocation of `cargo test -p mahoquot-gateway --test devin_wire_fixtures` was executed directly in this environment, succeeding with exit code `0` (see Section 7).

6. **UTF-8 Protobuf String Invariant in Positive Fixtures:**  
   - `chat_utf8_boundary.json` and `chat_response.json` contain 100% valid, complete UTF-8 strings per individual protobuf frame (Frame 0: `"가"`, Frame 1: `"나"`). No codepoint slicing across protobuf messages exists in positive fixtures.
   - Transport stream byte reassembly is tested across arbitrary chunk boundaries (1B, 2B, 3B, 5B, 7B, 11B, 13B, 16B, 23B).

7. **Negative Rejection Fixture for Malformed UTF-8:**  
   - `chat_utf8_malformed_negative.json` is preserved with classification `"negative"`. Contains incomplete codepoint bytes `0xeab080eb` in `delta_text`; verified that `std::str::from_utf8` fails fatally.

8. **Interleaved Tool Calling Without ID-Less Ambiguity:**  
   - `chat_tools.json` and `chat_response.json` provide explicit `id` fields (`call_001`, `call_002`) on all continuation deltas.
   - `chat_tools_ambiguous_negative.json` demonstrates rejection when 2 calls are concurrently active and an incoming delta lacks an ID.

9. **Contract Documentation Alignment & Schema Parity:**  
   - Audited `docs/devin-wire-contract.md` against pinned `devin.proto`:
     - Section 6.3 correctly specifies `GetChatMessageRequest` field 10 as `tools` (`repeated ExaChatPb_ChatToolDefinition tools`), rejecting the erroneous field 5 notation.
     - Section 6.2 correctly specifies `ExaCodeiumCommonPb_ClientModelConfig` field 18 `max_tokens` as `int32` (distinguishing it from `ExaCodeiumCommonPb_CompletionConfiguration` field 2 `max_tokens` which is `uint64`).
     - Stop reasons strictly match pinned proto enum identifiers: code 1 is `STOP_REASON_INCOMPLETE` and code 9 is `STOP_REASON_PARTIAL`. There is no invented `STOP_REASON_STOP` identifier in the protobuf schema; normal completion defaults from stream end with `stop_reason = 0 (UNSPECIFIED)`. Downstream proxy mappings to OpenAI/Anthropic reasons (`stop`, `length`, `tool_calls`) are explicitly distinguished from upstream protobuf names.
     - Token validation contract: credentials containing whitespace, newlines (`\r`, `\n`), or control characters must be rejected with an explicit validation error rather than silently altered or stripped.
     - Accurate 36 B EndStream payload documentation: `chat_usage.json` EndStream payload is accurately labeled as `{"error":null,"hasSyncPoints":false}` (36 B), not the minimal 14 B `{"error":null}`.

10. **Zero Real Credentials & Explicit Live Claims Boundary:**  
    - Exclusively synthetic dummy credentials (`dummy-token`, `Basic dummy-token-dummy-token`).
    - Live upstream server compatibility is explicitly flagged as **UNVERIFIED (experimental)**.

*Note: This report constitutes independent verification. Final acceptance rests with the lead.*

---

## 2. Upstream Source & Wire Tag Verification

### 2.1 Pinned Upstream Reference Coordinates
- **Repository:** `https://github.com/Arborsm/dsh-plugin-devin-bridge`
- **Pinned Commit:** `ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4`
- **Reference Files:** `src/proto/devin.proto`, `src/adapter/transport.ts`, `src/adapter/devin.ts`, `src/adapter/decoder.ts`, `LICENSE`.

### 2.2 Proto Schema Byte-for-Byte Comparison
The local schema at `crates/gateway/src/compat/devin.proto` was compared against raw upstream `src/proto/devin.proto` fetched directly from commit `ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4`:
- **Provenance Header:** Lines 1-11 contain mahoquot provenance attribution, reference commit hash, and MIT license notice.
- **Proto Schema Body:** Lines 12-202 match upstream `src/proto/devin.proto` **100% byte-for-byte** (verified via `diff -u <(tail -n +12 crates/gateway/src/compat/devin.proto) /tmp/upstream_devin.proto`, producing 0 differences).
- **Syntax & Package:** `syntax = "proto2";`, `package exa.api_server_pb;`.

### 2.3 Exhaustive Wire Tag & Field Number Verification Matrix

All field tags, wire types, and LEB128 encodings across every declared message in `devin.proto` were verified against the Protobuf wire specification and independent wire decoder:

| Message Name | Field # | Field Name | Type | Wire Type | Wire Tag Hex | LEB128 Encoding Computation | Verified Semantics |
|---|---|---|---|---|---|---|---|
| `GoogleProtobuf_Timestamp` | 1 | `seconds` | `int64` | 0 (varint) | `08` | `(1 << 3) \| 0 = 8` | Epoch timestamp seconds |
| `GoogleProtobuf_Timestamp` | 2 | `nanos` | `int32` | 0 (varint) | `10` | `(2 << 3) \| 0 = 16` | Subsecond nanoseconds |
| `ExaCodeiumCommonPb_Metadata` | 1 | `ide_name` | `string` | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Client name (`"chisel"`) |
| `ExaCodeiumCommonPb_Metadata` | 2 | `extension_version` | `string` | 2 (len-delim) | `12` | `(2 << 3) \| 2 = 18` | Extension version (`"3000.2.17"`) |
| `ExaCodeiumCommonPb_Metadata` | 3 | `api_key` | `string` | 2 (len-delim) | `1a` | `(3 << 3) \| 2 = 26` | Auth session token |
| `ExaCodeiumCommonPb_Metadata` | 4 | `locale` | `string` | 2 (len-delim) | `22` | `(4 << 3) \| 2 = 34` | Locale identifier (`"en"`) |
| `ExaCodeiumCommonPb_Metadata` | 5 | `os` | `string` | 2 (len-delim) | `2a` | `(5 << 3) \| 2 = 42` | OS platform (`"win"`) |
| `ExaCodeiumCommonPb_Metadata` | 7 | `ide_version` | `string` | 2 (len-delim) | `3a` | `(7 << 3) \| 2 = 58` | IDE version (`"3000.2.17"`) |
| `ExaCodeiumCommonPb_Metadata` | 12 | `extension_name` | `string` | 2 (len-delim) | `62` | `(12 << 3) \| 2 = 98` | Extension identifier (`"chisel"`) |
| `ExaCodeiumCommonPb_Metadata` | 31 | `f` | `string` | 2 (len-delim) | `fa01` | `(31 << 3) \| 2 = 250` -> `0xFA 0x01` | Client hex fingerprint |
| `ExaCodeiumCommonPb_CompletionConfiguration` | 1 | `num_completions` | `uint64` | 0 (varint) | `08` | `(1 << 3) \| 0 = 8` | Completions count |
| `ExaCodeiumCommonPb_CompletionConfiguration` | 2 | `max_tokens` | `uint64` | 0 (varint) | `10` | `(2 << 3) \| 0 = 16` | Token generation cap |
| `ExaCodeiumCommonPb_CompletionConfiguration` | 3 | `max_newlines` | `uint64` | 0 (varint) | `18` | `(3 << 3) \| 0 = 24` | Maximum newline limit |
| `ExaCodeiumCommonPb_CompletionConfiguration` | 5 | `temperature` | `double` | 1 (fixed64) | `29` | `(5 << 3) \| 1 = 41` | Sampling temperature |
| `ExaCodeiumCommonPb_CompletionConfiguration` | 7 | `top_k` | `uint64` | 0 (varint) | `38` | `(7 << 3) \| 0 = 56` | Top-K sampling cutoff |
| `ExaCodeiumCommonPb_CompletionConfiguration` | 8 | `top_p` | `double` | 1 (fixed64) | `41` | `(8 << 3) \| 1 = 65` | Nucleus sampling probability |
| `ExaCodeiumCommonPb_ImageData` | 1 | `base64_data` | `string` | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Base64-encoded image bytes |
| `ExaCodeiumCommonPb_ImageData` | 2 | `mime_type` | `string` | 2 (len-delim) | `12` | `(2 << 3) \| 2 = 18` | Image MIME type (`"image/png"`) |
| `ExaCodeiumCommonPb_ChatToolCall` | 1 | `id` | `string` | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Explicit call ID (`call_001`) |
| `ExaCodeiumCommonPb_ChatToolCall` | 2 | `name` | `string` | 2 (len-delim) | `12` | `(2 << 3) \| 2 = 18` | Tool function name |
| `ExaCodeiumCommonPb_ChatToolCall` | 3 | `arguments_json` | `string` | 2 (len-delim) | `1a` | `(3 << 3) \| 2 = 26` | Serialized JSON args delta |
| `ExaChatPb_ChatToolDefinition` | 1 | `name` | `string` | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Tool name |
| `ExaChatPb_ChatToolDefinition` | 2 | `description` | `string` | 2 (len-delim) | `12` | `(2 << 3) \| 2 = 18` | Tool purpose description |
| `ExaChatPb_ChatToolDefinition` | 3 | `json_schema_string` | `string` | 2 (len-delim) | `1a` | `(3 << 3) \| 2 = 26` | Tool parameters JSON schema |
| `ExaChatPb_ChatMessagePrompt` | 1 | `message_id` | `string` | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Prompt message ID |
| `ExaChatPb_ChatMessagePrompt` | 2 | `source` | `enum` | 0 (varint) | `10` | `(2 << 3) \| 0 = 16` | Message source (`USER = 1`) |
| `ExaChatPb_ChatMessagePrompt` | 3 | `prompt` | `string` | 2 (len-delim) | `1a` | `(3 << 3) \| 2 = 26` | Prompt text content |
| `ExaChatPb_ChatMessagePrompt` | 6 | `tool_calls` | Submessage | 2 (len-delim) | `32` | `(6 << 3) \| 2 = 50` | Repeated `ChatToolCall` |
| `ExaChatPb_ChatMessagePrompt` | 7 | `tool_call_id` | `string` | 2 (len-delim) | `3a` | `(7 << 3) \| 2 = 58` | Tool result call ID |
| `ExaChatPb_ChatMessagePrompt` | 9 | `tool_result_is_error` | `bool` | 0 (varint) | `48` | `(9 << 3) \| 0 = 72` | Tool result error flag |
| `ExaChatPb_ChatMessagePrompt` | 10 | `images` | Submessage | 2 (len-delim) | `52` | `(10 << 3) \| 2 = 82` | Repeated `ImageData` |
| `ExaChatPb_ChatMessagePrompt` | 11 | `thinking` | `string` | 2 (len-delim) | `5a` | `(11 << 3) \| 2 = 90` | Historical reasoning text |
| `ExaChatPb_ChatMessagePrompt` | 12 | `signature` | `string` | 2 (len-delim) | `62` | `(12 << 3) \| 2 = 98` | Historical reasoning signature |
| `ExaChatPb_ChatMessagePrompt` | 13 | `thinking_redacted` | `bool` | 0 (varint) | `68` | `(13 << 3) \| 0 = 104`| Historical redaction flag |
| `ExaCortexPb_CortexTrajectoryReference` | 1 | `trajectory_id` | `string` | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Cortex trajectory ID |
| `ExaCortexPb_CortexTrajectoryReference` | 3 | `trajectory_type` | `enum` | 0 (varint) | `18` | `(3 << 3) \| 0 = 24` | Cortex trajectory type (4) |
| `ExaCortexPb_CortexTrajectoryReference` | 4 | `step_type` | `enum` | 0 (varint) | `20` | `(4 << 3) \| 0 = 32` | Cortex step type (14) |
| `ExaCodeiumCommonPb_ModelUsageStats` | 2 | `input_tokens` | `uint64` | 0 (varint) | `10` | `(2 << 3) \| 0 = 16` | Authoritative input count |
| `ExaCodeiumCommonPb_ModelUsageStats` | 3 | `output_tokens` | `uint64` | 0 (varint) | `18` | `(3 << 3) \| 0 = 24` | Authoritative output count |
| `ExaCodeiumCommonPb_ModelUsageStats` | 4 | `cache_write_tokens`| `uint64` | 0 (varint) | `20` | `(4 << 3) \| 0 = 32` | Cache write breakdown |
| `ExaCodeiumCommonPb_ModelUsageStats` | 5 | `cache_read_tokens` | `uint64` | 0 (varint) | `28` | `(5 << 3) \| 0 = 40` | Cache read breakdown |
| `ExaCodeiumCommonPb_ModelUsageStats` | 9 | `model_uid` | `string` | 2 (len-delim) | `4a` | `(9 << 3) \| 2 = 74` | Attribution model UID |
| `ExaCodeiumCommonPb_PromoStatus` | 1 | `is_active` | `bool` | 0 (varint) | `08` | `(1 << 3) \| 0 = 8` | Promotion active flag |
| `ExaCodeiumCommonPb_PromoStatus` | 2 | `end_date` | Submessage | 2 (len-delim) | `12` | `(2 << 3) \| 2 = 18` | Promotion expiration timestamp |
| `ExaCodeiumCommonPb_PromoStatus` | 3 | `label` | `string` | 2 (len-delim) | `1a` | `(3 << 3) \| 2 = 26` | Promotion label text |
| `ExaCodeiumCommonPb_ModelFamilyMetadata` | 1 | `model_family_label` | `string` | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Family group label |
| `ExaCodeiumCommonPb_ModelFamilyMetadata` | 3 | `is_default_model_in_family` | `bool` | 0 (varint) | `18` | `(3 << 3) \| 0 = 24` | Default family flag |
| `ExaCodeiumCommonPb_ClientModelConfig` | 1 | `label` | `string` | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Model display name |
| `ExaCodeiumCommonPb_ClientModelConfig` | 3 | `credit_multiplier` | `float` | 5 (fixed32) | `1d` | `(3 << 3) \| 5 = 29` | Billing credit multiplier |
| `ExaCodeiumCommonPb_ClientModelConfig` | 4 | `disabled` | `bool` | 0 (varint) | `20` | `(4 << 3) \| 0 = 32` | Disabled model flag |
| `ExaCodeiumCommonPb_ClientModelConfig` | 5 | `supports_images` | `bool` | 0 (varint) | `28` | `(5 << 3) \| 0 = 40` | Multimodal flag |
| `ExaCodeiumCommonPb_ClientModelConfig` | 7 | `is_premium` | `bool` | 0 (varint) | `38` | `(7 << 3) \| 0 = 56` | Premium flag |
| `ExaCodeiumCommonPb_ClientModelConfig` | 9 | `is_beta` | `bool` | 0 (varint) | `48` | `(9 << 3) \| 0 = 72` | Beta model flag |
| `ExaCodeiumCommonPb_ClientModelConfig` | 10 | `provider` | `enum` | 0 (varint) | `50` | `(10 << 3) \| 0 = 80` | Model provider enum |
| `ExaCodeiumCommonPb_ClientModelConfig` | 11 | `is_recommended` | `bool` | 0 (varint) | `58` | `(11 << 3) \| 0 = 88` | Recommended model flag |
| `ExaCodeiumCommonPb_ClientModelConfig` | 15 | `is_new` | `bool` | 0 (varint) | `78` | `(15 << 3) \| 0 = 120` | New model flag |
| `ExaCodeiumCommonPb_ClientModelConfig` | 18 | `max_tokens` | `int32` | 0 (varint) | `9001` | `(18 << 3) \| 0 = 144` -> `0x90 0x01` | Synthetic context cap |
| `ExaCodeiumCommonPb_ClientModelConfig` | 19 | `promo_status` | Submessage | 2 (len-delim) | `9a01` | `(19 << 3) \| 2 = 154` -> `0x9A 0x01` | Promo status metadata |
| `ExaCodeiumCommonPb_ClientModelConfig` | 20 | `is_capacity_limited` | `bool` | 0 (varint) | `a001` | `(20 << 3) \| 0 = 160` -> `0xA0 0x01` | Capacity limit flag |
| `ExaCodeiumCommonPb_ClientModelConfig` | 22 | `model_uid` | `string` | 2 (len-delim) | `b201` | `(22 << 3) \| 2 = 178` -> `0xB2 0x01` | Target model UID (`"glm-5-2"`) |
| `ExaCodeiumCommonPb_ClientModelConfig` | 27 | `description` | `string` | 2 (len-delim) | `da01` | `(27 << 3) \| 2 = 218` -> `0xDA 0x01` | Model description |
| `ExaCodeiumCommonPb_ClientModelConfig` | 30 | `model_family_metadata` | Submessage | 2 (len-delim) | `f201` | `(30 << 3) \| 2 = 242` -> `0xF2 0x01` | Family grouping metadata |
| `GetChatMessageRequest` | 1 | `metadata` | Submessage | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | `Metadata` |
| `GetChatMessageRequest` | 2 | `prompt` | `string` | 2 (len-delim) | `12` | `(2 << 3) \| 2 = 18` | System instruction |
| `GetChatMessageRequest` | 3 | `chat_message_prompts`| Submessage | 2 (len-delim) | `1a` | `(3 << 3) \| 2 = 26` | Repeated `ChatMessagePrompt` |
| `GetChatMessageRequest` | 7 | `request_type` | `enum` | 0 (varint) | `38` | `(7 << 3) \| 0 = 56` | Value 5 (`CASCADE`) |
| `GetChatMessageRequest` | 8 | `configuration`| Submessage | 2 (len-delim) | `42` | `(8 << 3) \| 2 = 66` | `CompletionConfiguration` |
| `GetChatMessageRequest` | 10 | `tools` | Submessage | 2 (len-delim) | `52` | `(10 << 3) \| 2 = 82` | Repeated `ChatToolDefinition` |
| `GetChatMessageRequest` | 15 | `trajectory_reference` | Submessage | 2 (len-delim) | `7a` | `(15 << 3) \| 2 = 122` | Cortex trajectory reference |
| `GetChatMessageRequest` | 16 | `cascade_id` | `string` | 2 (len-delim) | `8201` | `(16 << 3) \| 2 = 130` -> `0x82 0x01` | Session cascade ID |
| `GetChatMessageRequest` | 20 | `planner_mode` | `enum` | 0 (varint) | `a001` | `(20 << 3) \| 0 = 160` -> `0xA0 0x01` | Planner mode (1) |
| `GetChatMessageRequest` | 21 | `chat_model_uid` | `string` | 2 (len-delim) | `aa01` | `(21 << 3) \| 2 = 170` -> `0xAA 0x01` | Upstream model UID |
| `GetChatMessageRequest` | 22 | `execution_id` | `string` | 2 (len-delim) | `b201` | `(22 << 3) \| 2 = 178` -> `0xB2 0x01` | Client execution ID |
| `GetChatMessageResponse` | 1 | `message_id` | `string` | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Response message ID |
| `GetChatMessageResponse` | 2 | `timestamp` | Submessage | 2 (len-delim) | `12` | `(2 << 3) \| 2 = 18` | Server response timestamp |
| `GetChatMessageResponse` | 3 | `delta_text` | `string` | 2 (len-delim) | `1a` | `(3 << 3) \| 2 = 26` | Streaming text delta |
| `GetChatMessageResponse` | 5 | `stop_reason` | `enum` | 0 (varint) | `28` | `(5 << 3) \| 0 = 40` | Stop reason (10 = FUNCTION_CALL) |
| `GetChatMessageResponse` | 6 | `delta_tool_calls`| Submessage | 2 (len-delim) | `32` | `(6 << 3) \| 2 = 50` | Repeated `ChatToolCall` |
| `GetChatMessageResponse` | 7 | `usage` | Submessage | 2 (len-delim) | `3a` | `(7 << 3) \| 2 = 58` | `ModelUsageStats` |
| `GetChatMessageResponse` | 9 | `delta_thinking` | `string` | 2 (len-delim) | `4a` | `(9 << 3) \| 2 = 74` | Reasoning delta |
| `GetChatMessageResponse` | 10 | `delta_signature`| `string` | 2 (len-delim) | `52` | `(10 << 3) \| 2 = 82` | Opaque verification token |
| `GetChatMessageResponse` | 11 | `thinking_redacted`| `bool` | 0 (varint) | `58` | `(11 << 3) \| 0 = 88` | Redacted reasoning flag |
| `GetChatMessageResponse` | 23 | `actual_model_uid`| `string` | 2 (len-delim) | `ba01` | `(23 << 3) \| 2 = 186` -> `0xBA 0x01`| Executing model UID |
| `GetCascadeModelConfigsRequest` | 1 | `metadata` | Submessage | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | `Metadata` |
| `GetCascadeModelConfigsResponse`| 1 | `client_model_configs`| Submessage | 2 (len-delim) | `0a` | `(1 << 3) \| 2 = 10` | Repeated `ClientModelConfig` |

### 2.4 Pinned Enum Definitions Matrix

| Enum Name | Numeric Code | Upstream Protobuf Identifier | Verified Semantics & Protocol Role |
|---|---|---|---|
| `ChatMessageRequestType` | 0 | `CHAT_MESSAGE_REQUEST_TYPE_UNSPECIFIED` | Unspecified default request type |
| `ChatMessageRequestType` | 5 | `CHAT_MESSAGE_REQUEST_TYPE_CASCADE` | Cascade chat generation request |
| `ExaCodeiumCommonPb_ChatMessageSource` | 0 | `CHAT_MESSAGE_SOURCE_UNSPECIFIED` | Unspecified message source |
| `ExaCodeiumCommonPb_ChatMessageSource` | 1 | `CHAT_MESSAGE_SOURCE_USER` | User input turn |
| `ExaCodeiumCommonPb_ChatMessageSource` | 2 | `CHAT_MESSAGE_SOURCE_SYSTEM` | System instructions / developer prompt |
| `ExaCodeiumCommonPb_ChatMessageSource` | 4 | `CHAT_MESSAGE_SOURCE_TOOL` | Tool execution output turn |
| `ExaCodeiumCommonPb_ConversationalPlannerMode` | 0 | `CONVERSATIONAL_PLANNER_MODE_UNSPECIFIED` | Unspecified planner mode |
| `ExaCodeiumCommonPb_ConversationalPlannerMode` | 1 | `CONVERSATIONAL_PLANNER_MODE_DEFAULT` | Default conversational planner |
| `ExaCortexPb_CortexTrajectoryType` | 0 | `CORTEX_TRAJECTORY_TYPE_UNSPECIFIED` | Unspecified trajectory type |
| `ExaCortexPb_CortexTrajectoryType` | 4 | `CORTEX_TRAJECTORY_TYPE_CASCADE` | Cascade trajectory tracking |
| `ExaCortexPb_CortexStepType` | 0 | `CORTEX_STEP_TYPE_UNSPECIFIED` | Unspecified step type |
| `ExaCortexPb_CortexStepType` | 14 | `CORTEX_STEP_TYPE_USER_INPUT` | User input step tracking |
| `ExaCodeiumCommonPb_StopReason` | 0 | `STOP_REASON_UNSPECIFIED` | Default intermediate streaming value |
| `ExaCodeiumCommonPb_StopReason` | 1 | `STOP_REASON_INCOMPLETE` | Output generation halted before completion |
| `ExaCodeiumCommonPb_StopReason` | 3 | `STOP_REASON_MAX_TOKENS` | Token generation cap reached |
| `ExaCodeiumCommonPb_StopReason` | 9 | `STOP_REASON_PARTIAL` | Truncated / partial content delivered |
| `ExaCodeiumCommonPb_StopReason` | 10 | `STOP_REASON_FUNCTION_CALL` | Tool invocation emitted by model |
| `ExaCodeiumCommonPb_StopReason` | 13 | `STOP_REASON_ERROR` | Upstream generation error |
| `ExaCodeiumCommonPb_ModelProvider` | 0 | `MODEL_PROVIDER_UNSPECIFIED` | Unspecified model provider |
| `ExaCodeiumCommonPb_ModelProvider` | 1 | `MODEL_PROVIDER_WINDSURF` | Codeium / Windsurf proprietary model |
| `ExaCodeiumCommonPb_ModelProvider` | 2 | `MODEL_PROVIDER_OPENAI` | OpenAI upstream provider |
| `ExaCodeiumCommonPb_ModelProvider` | 3 | `MODEL_PROVIDER_ANTHROPIC` | Anthropic upstream provider |

---

## 3. Lead Audit Defects & Implementation Verification

### 3.1 Defect 1: String vs. Submessage Heuristic Parse Success Bypass
- **Vulnerability:** In earlier test code, `audit_string_fields_recursive` guessed whether a field was a submessage by attempting to parse its payload. A string containing malformed UTF-8 bytes `[0x08, 0x80, 0x01]` was parsed successfully as a submessage (field 1 = varint 128) and bypassed UTF-8 validation.
- **Verification of Fix:**
  - `crates/gateway/tests/devin_wire_fixtures.rs` implements `ProtoSchema` and `audit_fields_schema_aware` (lines 80-238). Field types are looked up strictly from schema definitions.
  - When a field is declared `StringField`, wire type must be 2 and payload bytes must pass `std::str::from_utf8`.
  - Negative fixture `chat_utf8_proto_parseable_negative.json` contains framed response with `delta_text` set to `[0x08, 0x80, 0x01]`.
  - Test `proto_parseable_invalid_utf8_rejected_negative_case` explicitly proves:
    1. Bytes `[0x08, 0x80, 0x01]` syntactically parse as a protobuf message.
    2. Bytes `[0x08, 0x80, 0x01]` fail `std::str::from_utf8`.
    3. `audit_fields_schema_aware` rejects the frame with an error identifying `delta_text` (field 3).
  - Test `all_positive_fixtures_have_valid_utf8_strings` uses `audit_fields_schema_aware` across all positive fixtures, verifying zero invalid UTF-8 strings exist in any positive fixture.

### 3.2 Defect 2: Byte Occurrence Scanning in `tag_checks`
- **Vulnerability:** Previous test code checked `tag_checks` using `raw.windows(N).any(|w| w == target)`, ignoring declared frame index and field paths. This allowed mismatched lengths in one path to pass if the byte sequence appeared in an unrelated field.
- **Verification of Fix:**
  - `assert_tag_check_exact_path` (lines 350-480) parses the declared `path` string and traverses the exact structural path:
    - Connect envelope paths: `"frame.flag"`, `"frame.len_be"`, `"end_stream.flag"`, `"end_stream.len_be"`.
    - Message level: `"field1.tag"`, `"field1.len"`.
    - Submessage level: `"metadata.field3.tag"`, `"metadata.field3.len"`, `"metadata.field12.tag"`, `"request_type.field7.tag"`, `"configuration.field5.tag"`, `"chat_model_uid.field21.tag"`.
    - Array entries: `"entry1.field*"`, `"entry2.field*"`.
    - Framed stream: `"frame{N}.field{M}.*"`, `"usage.field{M}.*"`, `"final.field23.tag"`.
  - Test `tag_checks_length_semantics_normalized` passes across all 12 fixtures with structural path assertions.

### 3.3 Defect 3: Measured-Size Reporting Errors
- **Vulnerability:** Previous verifier reports claimed `chat_tools.json` was 232 B, `chat_response.json` was 369 B, and `chat_tools_ambiguous_negative.json` was 120 B.
- **Verification of Fix:** Every fixture was re-measured from raw bytes. All fixture headers, payloads, and total byte lengths are reconciled in Section 4.

---

## 4. Independent Golden Fixture Catalog & Exact Recalculated Metrics

All 12 golden fixture files exist identically in `tests/data/devin/` and `crates/gateway/tests/data/devin/` (`diff -r` confirms 0 differences).

| Fixture File Name | RPC Method | Classification | Total Wire Size | Framing Mode | Frame Count & Per-Frame Payload Lengths (Bytes) | Key Scenarios & Assertions Verified |
|---|---|---|---|---|---|---|
| `unary_request.json` | `GetCascadeModelConfigs` | `positive` | 15 B | Unframed `application/proto` | Unframed: [15] | First byte `0x0A` (unframed tag). Literal header `Basic dummy-token-dummy-token`. `Metadata.api_key = "dummy-token"`. |
| `unary_response.json` | `GetCascadeModelConfigs` | `positive` | 58 B | Unframed `application/proto` | Unframed: [58] | Repeated `ClientModelConfig`. Varint `max_tokens` (200000, 262000), `model_uid` (`glm-5-2`, `swe-1-7`), `supports_images: true`, `is_premium: true`. |
| `chat_request.json` | `GetChatMessage` | `positive` | 196 B | Framed `application/connect+proto` | 1 frame: [191] | Flag `0x00`, length 191 (`0x000000BF`). Metadata fields, system prompt, complete Korean prompt ("안녕하세요, 한 줄 소개를 작성해 주세요."), `chat_model_uid = "glm-5-2"`. (5B header + 191B payload = 196B). |
| `chat_response.json` | `GetChatMessage` | `positive` | 389 B | Framed `application/connect+proto` | 10 frames: [20, 16, 16, 32, 28, 13, 118, 30, 30, 36] | 9 data frames (`0x00`) + 1 EndStream (`0x02`). Valid complete UTF-8 strings per frame ("가", "나"), thinking, signature, redaction, interleaved tools with explicit IDs (`call_001`, `call_002`), usage snapshot, stop reason 10 (`FUNCTION_CALL`), `actual_model_uid`. (10*5=50B headers + 339B payloads = 389B). |
| `chat_endstream_error.json` | `GetChatMessage` | `positive` | 44 B | Framed `application/connect+proto` | 1 frame: [39] | Terminal EndStream frame flag `0x02`, length 39 (`0x00000027`). Code-only JSON `{"error":{"code":"resource_exhausted"}}`. (5B header + 39B payload = 44B). |
| `chat_usage.json` | `GetChatMessage` | `positive` | 76 B | Framed `application/connect+proto` | 2 frames: [30, 36] | Isolated `ModelUsageStats`: input 12, output 34, cache write 7, cache read 5; total = 46. EndStream JSON `{"error":null,"hasSyncPoints":false}` (36 B payload). (2*5=10B headers + 66B payloads = 76B). |
| `chat_reasoning.json` | `GetChatMessage` | `positive` | 154 B | Framed `application/connect+proto` | 5 frames: [32, 28, 13, 20, 36] | Reasoning sequence: `delta_thinking` -> `delta_signature` -> `thinking_redacted: true` -> `delta_text` terminating reasoning. (5*5=25B headers + 129B payloads = 154B). |
| `chat_tools.json` | `GetChatMessage` | `positive` | 247 B | Framed `application/connect+proto` | 6 frames: [47, 42, 32, 30, 30, 36] | Interleaved tools (`call_001`, `call_002`) with explicit IDs in continuation frames 2 and 3; stop reason 10, actual model UID. (6*5=30B headers + 217B payloads = 247B). |
| `chat_tools_ambiguous_negative.json` | `GetChatMessage` | `negative` | 126 B | Framed `application/connect+proto` | 3 frames: [47, 42, 22] | Rejection fixture: 2 active calls with incoming ID-less delta; decoders must reject as `ambiguous_id_less_delta_with_multiple_active_calls`. (3*5=15B headers + 111B payloads = 126B). |
| `chat_utf8_boundary.json` | `GetChatMessage` | `positive` | 83 B | Framed `application/connect+proto` | 3 frames: [16, 16, 36] | Complete Korean UTF-8 per frame (Frame 0: `"가"`, Frame 1: `"나"`). Connect transport byte stream reassembly verified across arbitrary chunk splits (1B, 2B, 3B, 5B, 7B, 11B, 13B, 16B, 23B). (3*5=15B headers + 68B payloads = 83B). |
| `chat_utf8_malformed_negative.json` | `GetChatMessage` | `negative` | 22 B | Framed `application/connect+proto` | 1 frame: [17] | Rejection fixture: `delta_text` contains incomplete codepoint bytes `0xeab080eb`; decoders must reject as `invalid_utf8_in_protobuf_string`. (5B header + 17B payload = 22B). |
| `chat_utf8_proto_parseable_negative.json` | `GetChatMessage` | `negative` | 21 B | Framed `application/connect+proto` | 1 frame: [16] | Rejection fixture: `delta_text` contains bytes `[0x08, 0x80, 0x01]` (parses as submessage field 1 = varint 128, but fails UTF-8). Proves schema-aware traversal rejects it. (5B header + 16B payload = 21B). |

---

## 5. Fixture Independence & Test Harness Architecture

The test harness in `crates/gateway/tests/devin_wire_fixtures.rs` adheres to strict testing independence:
1. **Self-Contained Parsing:** Implements its own minimal LEB128 varint decoder (`read_varint`), raw wire field parser (`parse_wire_fields`), Connect envelope framer (`split_connect_frames`), and streaming chunk buffer (`StreamAssembler`).
2. **Zero Production Code Dependencies:** Does not import from `prost`, `prost_types`, `mahoquot-gateway`, `mahoquot-providers`, or `mahoquot-registry`. Only uses `std` and `serde_json`.
3. **Deterministic Execution:** No network calls, fixed sleeps, background threads, or status polling loops. All tests execute deterministically in `< 0.01s`.

---

## 6. Credential Hygiene & Live Claims Boundary

### 6.1 Credential Hygiene Audit
- A programmatic regex audit across all fixture JSON files, test source files, schemas, and contract documentation confirmed **zero real session tokens, Bearer secrets, or API keys**.
- All authentication values strictly use:
  - Token string: `"dummy-token"`
  - Authorization header: `"Basic dummy-token-dummy-token"`
  - Protobuf metadata: `Metadata.api_key = "dummy-token"`

### 6.2 Boundaries of Live Server Claims
- Documentation (`docs/devin-wire-contract.md`, `.omo/evidence/devin/p0-producer.md`, `crates/gateway/src/compat/devin.proto`) explicitly defines the verified boundary:
  - **Verified:** Parity with pinned upstream commit `ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4`.
  - **UNVERIFIED (experimental):** Direct live compatibility with `https://server.codeium.com`.
  - **UNVERIFIED (unknown):** Account token quotas, credit multipliers, rate limits, and actual billing rates.
  - **UNVERIFIED:** Context window limits (upstream README cites 128k/16k while code defaults to 200k/262k).
- No unsupported claims of live execution or end-to-end inference are made.

---

## 7. Command Execution Evidence & Actual Output

### 7.1 LSP Diagnostics Check
- **Target:** `crates/gateway/tests/devin_wire_fixtures.rs`
- **Tool:** `lsp_diagnostics`
- **Result:** **0 errors, 0 warnings**.

### 7.2 Cargo Test Execution
- **Working Directory:** `/Users/indo/code/project/mahoquot-proxy`
- **Command:**
  ```bash
  cargo test -p mahoquot-gateway --test devin_wire_fixtures
  ```
- **Exit Code:** `0`
- **Captured Output:**
  ```text
      Finished `test` profile [unoptimized + debuginfo] target(s) in 0.44s
       Running tests/devin_wire_fixtures.rs (target/debug/deps/devin_wire_fixtures-e7267d5fae0f7367)

  running 15 tests
  test terminal_code_only_error ... ok
  test proto_parseable_invalid_utf8_rejected_negative_case ... ok
  test ambiguous_tool_calls_rejected_negative_fixture ... ok
  test malformed_utf8_rejected_negative_fixture ... ok
  test literal_basic_dummy_token_header ... ok
  test reasoning_signature_and_redaction ... ok
  test unary_unframed_request_and_response ... ok
  test interleaved_tool_frames_across_frames ... ok
  test usage_and_cache_snapshot ... ok
  test chat_framed_request ... ok
  test korean_utf8_complete_strings_and_arbitrary_transport_splits ... ok
  test chat_framed_response_stream ... ok
  test actual_model_uid_contract ... ok
  test all_positive_fixtures_have_valid_utf8_strings ... ok
  test tag_checks_length_semantics_normalized ... ok

  test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
  ```

All 15 independent tests passed with exit code 0.

---

## 8. Explicit Identification of Missing Cases / Scope Gaps

The following upstream protocol features are declared in schema/code but unexercised in dedicated golden fixtures:
1. **Request Tool Definitions:** `GetChatMessageRequest` field 10 (`repeated ExaChatPb_ChatToolDefinition tools`) is omitted in `chat_request.json`; tool invocations are tested only in response frames.
2. **Multi-Turn Conversation History:** `chat_request.json` contains only a single user turn (`source = USER = 1`). Historical assistant turns (`source = SYSTEM = 2`), prior tool calls (`ChatMessagePrompt.tool_calls`, field 6), and tool results (`source = TOOL = 4`, `tool_call_id = 7`, `tool_result_is_error = 9`) lack request fixtures.
3. **Multimodal Image Attachments:** `ExaCodeiumCommonPb_ImageData` (`base64_data = 1`, `mime_type = 2`) is defined in schema and referenced in `ChatMessagePrompt.images` (field 10), but no fixture contains image payloads.
4. **Connect Compressed Frames:** Framing flag `0x01` (compressed data frame) is not covered; all fixtures use uncompressed data (`0x00`) or EndStream (`0x02`).
5. **Connect EndStream Errors with Detailed Messages:** `chat_endstream_error.json` tests code-only errors (`{"error":{"code":"resource_exhausted"}}`). Upstream Connect errors with `message` strings or `details` arrays are not represented in fixtures.
6. **Alternative Stop Reasons:** Only stop reason 10 (`STOP_REASON_FUNCTION_CALL`) is exercised in `chat_response.json` and `chat_tools.json`. Stop reasons 1 (`INCOMPLETE`), 3 (`MAX_TOKENS`), 9 (`PARTIAL`), and 13 (`ERROR`) lack dedicated fixtures.
7. **Extended Model Configuration Metadata:** `unary_response.json` tests basic model fields (`label`, `supports_images`, `is_premium`, `max_tokens`, `model_uid`). Extended upstream metadata (`credit_multiplier` field 3, `promo_status` field 19, `model_family_metadata` field 30, `is_capacity_limited` field 20) are omitted.
8. **Trajectory and Execution Tracking:** `trajectory_reference` (field 15), `cascade_id` (field 16), `planner_mode` (field 20), and `execution_id` (field 22) in `GetChatMessageRequest` are omitted in `chat_request.json`.

---

## 9. Verification Conclusion & Lead Handoff

Plan P0 artifacts and contracts satisfy all requirements:
1. Pinned schema `crates/gateway/src/compat/devin.proto` matches upstream commit `ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4` byte-for-byte in schema content.
2. UTF-8 string invariants are upheld across all positive fixtures; malformed UTF-8 is isolated in negative fixtures asserting fatal rejection.
3. Heuristic string parsing has been replaced by schema-aware field validation (`audit_fields_schema_aware`).
4. Path-directed tag assertions (`assert_tag_check_exact_path`) eliminate arbitrary byte scanning.
5. All 12 fixture sizes are recalculated and verified against raw wire bytes.
6. `cargo test -p mahoquot-gateway --test devin_wire_fixtures` passes with exit code `0` (15/15 tests passing).
7. Credentials are dummy-only; live compatibility remains designated as experimental.
8. Contract documentation accurately reflects field numbers, types, enum names, and token validation rules.

**Intermediate Verification Status:** PASS.  
**Lead Handoff:** Report submitted for final lead review.

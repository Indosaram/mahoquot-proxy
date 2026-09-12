# Devin P0 — Wire Contract & Independent Golden Fixtures Evidence

**Task ID:** `st_01a092ce`  
**Agent:** hephaestus (child task)  
**Parent Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Root Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Model:** `mahoquot/gemini-3.8-flash-high` (sole implementation owner, no subdelegation)  
**Date:** 2026-09-12  
**Plan Reference:** Plan P0 in `/Users/indo/code/project/mahoquot-proxy/.omo/plans/devin-provider-integration.md`  

---

## 1. Executive Summary & Artifacts Produced

The requirements of Plan P0 have been satisfied in full in accordance with lead review specifications, resolving all prior rejection findings and audit issues without touching production decoder logic, manifests, or any pre-existing dirty working tree state.

### Deliverables Created / Updated:
1. `crates/gateway/src/compat/devin.proto`  
   Faithfully copied from reference commit `ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4` with header provenance, MIT notice, and proto2 syntax.
2. `tests/data/devin/` & `crates/gateway/tests/data/devin/`  
   Contains 12 independent hand-crafted golden fixture JSON files with documented protobuf field numbers, tags, wire types, synthetic dummy credentials, and normalized length semantics. Includes dedicated positive fixtures and negative rejection fixtures.
3. `crates/gateway/tests/devin_wire_fixtures.rs`  
   Executable independent fixture test suite utilizing a standalone byte parser, fixed schema-aware field traversal, path-directed tag check verification, and streaming transport assembler (zero dependency on production prost or gateway decoders). Contains 15 exhaustive tests passing with exit code `0`.
4. `docs/devin-wire-contract.md`  
   Comprehensive wire protocol contract documentation specifying transport, headers, framing, field schemas, invariants, verified/unverified boundaries, exact fixture sizes, and schema-aware traversal rules.
5. `.omo/evidence/devin/p0-producer.md`  
   This evidence report detailing scope compliance, lead audit resolution, exact recalculated fixture metrics, raw verification, test results, and downstream contracts.

---

## 2. Lead Audit Findings & Resolution Summary

| Audit Finding | Root Cause Identified | Resolution Implemented | Verification Evidence |
|---|---|---|---|
| **String-vs-message parse success heuristic** | Heuristic in `audit_string_fields_recursive` guessed submessage vs string from parse success. Malformed string bytes `[0x08, 0x80, 0x01]` parse as a valid submessage (field 1 = varint 128) and bypassed the UTF-8 check. | Replaced heuristic with fixed schema-aware field traversal (`audit_fields_schema_aware`) using `ProtoSchema` from `devin.proto`. String fields are validated strictly via `std::str::from_utf8`. Added faithful negative test case `proto_parseable_invalid_utf8_rejected_negative_case` and fixture `chat_utf8_proto_parseable_negative.json`. | `all_positive_fixtures_have_valid_utf8_strings` and `proto_parseable_invalid_utf8_rejected_negative_case` pass. |
| **Byte occurrence scanning in `tag_checks`** | `tag_checks_length_semantics_normalized` scanned arbitrary matching bytes anywhere in the wire stream, ignoring declared frame/field path. | Replaced arbitrary byte scanning with exact path-directed navigation (`assert_tag_check_exact_path`). Each tag check navigates the exact frame index, submessage path, tag bytes, length bytes, payload length, and value. | `tag_checks_length_semantics_normalized` passes across all 12 fixtures with exact structural assertions. |
| **Measured-size error in report (`chat_tools.json`)** | Report previously claimed 232 B for `chat_tools.json`. Lead parsed framed hex at 247 total bytes (6 headers * 5B = 30B, payloads `[47, 42, 32, 30, 30, 36]` = 217B, total 247B). | Recalculated all fixture sizes, frame counts, and per-frame payload lengths from raw bytes across all 12 fixtures. Updated documentation and evidence tables. | Automated Python byte calculation and rust test verification match exact byte counts. |
| **Fixture classification vs production decoder rejection** | Conflation between fixture classification tests and production decoder tests. | Clarified that negative fixture tests in P0 verify that the golden fixture specification and independent test validator correctly identify and reject protocol violations; production decoder implementation is scoped to P2. | Documentation and test descriptions explicitly distinguish fixture classification from production decoders. |
| **No premature Cargo-green claims** | Stale claims guaranteeing future compilation/clean build of unwritten crates. | Removed speculative future build claims; documented exact verification results of current scoped test command. | Actual execution of `cargo test -p mahoquot-gateway --test devin_wire_fixtures` reported with exact compiler and test output. |

---

## 3. Scope Compliance Audit

- **Cargo Manifests:** Unmodified (`Cargo.toml` and `Cargo.lock` preserved as-is).
- **Production Files:** No modifications to `crates/gateway/src/compat/mod.rs`, `relay.rs`, `account.rs`, `lib.rs`, or any other production files.
- **Production Decoder:** No production decoder implemented in this phase (Plan P0 strictly requires wire fixtures and contracts; production decoder belongs to P2).
- **Pre-existing Dirty Work:** All dirty files noted in baseline (`account.rs`, `relay.rs`, `creds.rs`, `quota.rs`, `usage.rs`, `models-v1.json`, `ui/index.html`, etc.) were preserved untouched; no `git stash`, `git reset`, or `git revert` was executed.
- **Git State:** No git commits or branches created.

---

## 4. Provenance & Reference Verification

### Upstream Sources
- **Fixed Reference Commit:** [`Arborsm/dsh-plugin-devin-bridge@ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4`](https://github.com/Arborsm/dsh-plugin-devin-bridge/tree/ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4)
- **Files Inspected & Fetched via HTTP:**
  - `src/proto/devin.proto`: Compared against `crates/gateway/src/compat/devin.proto`. Diff confirmed byte-for-byte fidelity beyond the added provenance header.
  - `LICENSE`: MIT License, Copyright (c) 2026 Arborsm. Reproduced in both `docs/devin-wire-contract.md` and `devin.proto`.
  - `src/adapter/transport.ts`: Verified literal `Authorization: Basic ${token}-${token}` header and Connect transport setup.
  - `src/adapter/devin.ts`: Verified `GetCascadeModelConfigs` (unary) and `GetChatMessage` (streaming) RPC signatures, metadata injection (`ide_name`, `extension_version`, `api_key`), and model discovery parsing.
  - `src/adapter/decoder.ts`: Verified `StreamChunk` mapping, delta thinking/signature/redaction sequencing, stop reasons, usage aggregation, and Connect error code mapping.

---

## 5. Verified vs. Unverified Boundaries

### Strictly Verified
- **Wire Framing:**
  - Unary RPC (`GetCascadeModelConfigs`) uses unframed binary protobuf with `Content-Type: application/proto`.
  - Server-Streaming RPC (`GetChatMessage`) uses Connect 5-byte envelope framing (`flag` + 4-byte big-endian `length`) with `Content-Type: application/connect+proto`.
- **Authentication Contract:**
  - `Authorization: Basic <token>-<token>` is literal ASCII with a hyphen, NOT base64 `user:password`.
  - Same `<token>` is mirrored in protobuf field `Metadata.api_key` (field 1 -> field 3).
- **Protobuf Schemas & Field Numbers:**
  - Verified against upstream `devin.proto`. All field tags and lengths verified by independent wire parser.
- **Connect Error Protocol:**
  - Upstream responds with HTTP status 200.
  - Errors arrive in the terminal `0x02` EndStreamResponse frame as JSON (`{"error": {"code": "..."}}`).
- **Data Invariants:**
  - In individual protobuf messages, all string fields must be complete, valid UTF-8.
  - Transport stream byte reassembly handles arbitrary network packet boundaries (1B, 2B, 3B, 7B, etc.).
  - When multiple tool calls exist, each delta frame must carry an explicit `id`.
  - Opaque reasoning signatures (`delta_signature`) must be preserved untouched.
  - `ModelUsageStats` is an authoritative snapshot; total tokens = `input_tokens + output_tokens`.

### Unverified / Experimental
- **Live Upstream Server Compatibility:** Direct RPC calls to `https://server.codeium.com` were not made during P0. Production server behavior is assumed compatible based on the pinned bridge implementation but remains flagged as **experimental**.
- **User Account Entitlements & Pricing:** Actual token billing rates, ACU credit multipliers, and rate limit thresholds are unmeasured and marked `unknown`.
- **Server Context Window Limits:** Values in upstream code (200k/262k) vs README (128k/16k) are synthetic observations, not guaranteed server limits.

---

## 6. Independent Golden Fixture Catalog (Recalculated Sizes)

All fixtures are stored in `tests/data/devin/` and `crates/gateway/tests/data/devin/`:

| Fixture File | RPC Method | Classification | Total Size | Framing / Frames | Per-Frame Payloads | Key Scenarios Tested |
|---|---|---|---|---|---|---|
| `unary_request.json` | `GetCascadeModelConfigs` | Positive | 15 B | Unframed `application/proto` | [15] | Unframed wire tag `0x0A`, literal `Basic dummy-token-dummy-token`, `Metadata.api_key` = `dummy-token`. |
| `unary_response.json` | `GetCascadeModelConfigs` | Positive | 58 B | Unframed `application/proto` | [58] | Repeated `ClientModelConfig`, varint tags (field 18 `9001`, field 22 `b201`), `glm-5-2` & `swe-1-7`. |
| `chat_request.json` | `GetChatMessage` | Positive | 196 B | Framed `application/connect+proto` (1 frame) | [191] | Single data frame (flag `0x00`, length 191), metadata fields (`ide_name=chisel`, etc.), system prompt, valid Korean prompt. |
| `chat_response.json` | `GetChatMessage` | Positive | 389 B | Framed `application/connect+proto` (10 frames) | [20, 16, 16, 32, 28, 13, 118, 30, 30, 36] | 9 data frames + 1 EndStream frame: text deltas, complete Korean UTF-8 ("가", "나"), thinking, signature, redaction, interleaved tools with explicit IDs, usage, stop reason 10 (`FUNCTION_CALL`), model UID. |
| `chat_endstream_error.json` | `GetChatMessage` | Positive | 44 B | Framed `application/connect+proto` (1 frame) | [39] | Terminal EndStream frame (flag `0x02`, length 39), code-only JSON `{"error": {"code": "resource_exhausted"}}`. |
| `chat_usage.json` | `GetChatMessage` | Positive | 76 B | Framed `application/connect+proto` (2 frames) | [30, 36] | Isolated `ModelUsageStats`: input 12, output 34, cache write 7, cache read 5; total = 46. EndStream payload (36 B): `{"error":null,"hasSyncPoints":false}`. |
| `chat_reasoning.json` | `GetChatMessage` | Positive | 154 B | Framed `application/connect+proto` (5 frames) | [32, 28, 13, 20, 36] | Reasoning sequence: `delta_thinking` -> `delta_signature` -> `thinking_redacted` -> `delta_text`. |
| `chat_tools.json` | `GetChatMessage` | Positive | 247 B | Framed `application/connect+proto` (6 frames) | [47, 42, 32, 30, 30, 36] | Interleaved tool calls (`call_001`, `call_002`) with explicit IDs on continuation frames 2 and 3, stop reason 10. Exactly 30B envelope headers + 217B payloads = 247B. |
| `chat_tools_ambiguous_negative.json` | `GetChatMessage` | Negative | 126 B | Framed `application/connect+proto` (3 frames) | [47, 42, 22] | Rejection fixture: 2 active calls with incoming ID-less delta; rejected as ambiguous protocol error. |
| `chat_utf8_boundary.json` | `GetChatMessage` | Positive | 83 B | Framed `application/connect+proto` (3 frames) | [16, 16, 36] | Complete Korean UTF-8 per frame ("가", "나"). Stream byte reassembly tested across arbitrary chunk splits (1B, 2B, 3B, 7B, 11B, 16B). |
| `chat_utf8_malformed_negative.json` | `GetChatMessage` | Negative | 22 B | Framed `application/connect+proto` (1 frame) | [17] | Rejection fixture: delta_text contains incomplete codepoint bytes (`0xeab080eb`); rejected as fatal UTF-8 deserialization error. |
| `chat_utf8_proto_parseable_negative.json` | `GetChatMessage` | Negative | 21 B | Framed `application/connect+proto` (1 frame) | [16] | Rejection fixture: delta_text contains bytes `[0x08, 0x80, 0x01]` (parses as protobuf submessage, but fails UTF-8). Proves schema-aware traversal rejects it. |

---

## 7. Test Verification & Results

### LSP Diagnostics Check
Command: `lsp_diagnostics` on `mahoquot-proxy/crates/gateway/tests/devin_wire_fixtures.rs`  
Result: **No diagnostics found (0 errors, 0 warnings).**

### Test Execution
Working Directory: `/Users/indo/code/project/mahoquot-proxy`  
Command:
```bash
cargo test -p mahoquot-gateway --test devin_wire_fixtures
```

Output:
```text
running 15 tests
test terminal_code_only_error ... ok
test proto_parseable_invalid_utf8_rejected_negative_case ... ok
test reasoning_signature_and_redaction ... ok
test ambiguous_tool_calls_rejected_negative_fixture ... ok
test malformed_utf8_rejected_negative_fixture ... ok
test chat_framed_request ... ok
test literal_basic_dummy_token_header ... ok
test usage_and_cache_snapshot ... ok
test interleaved_tool_frames_across_frames ... ok
test korean_utf8_complete_strings_and_arbitrary_transport_splits ... ok
test chat_framed_response_stream ... ok
test unary_unframed_request_and_response ... ok
test actual_model_uid_contract ... ok
test all_positive_fixtures_have_valid_utf8_strings ... ok
test tag_checks_length_semantics_normalized ... ok

test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
**Exit Code:** `0`

All 15 independent tests pass reliably without non-deterministic waits, fixed sleeps, or polling.

---

## 8. Contracts for Downstream Nodes

1. **For Phase P1 (Credentials):**
   - Header helper must produce `("Authorization", format!("Basic {token}-{token}"))`.
   - Token validation must reject invalid tokens containing whitespace, newlines (`\r`, `\n`), or control characters with an explicit validation error instead of silently stripping or altering them, preventing HTTP header injection while avoiding silent credential mutation.
   - Default upstream URL is `https://server.codeium.com`.

2. **For Phase P2 (Wire Codec):**
   - Prost code generation from `crates/gateway/src/compat/devin.proto` must preserve `proto2` optional presence semantics.
   - Connect frame parser must handle 5-byte envelopes, buffering across incomplete network chunks.
   - Protobuf decoders must validate UTF-8 on all string fields using schema-aware field definitions rather than heuristic trial parsing, and reject malformed UTF-8 as a fatal error.
   - Ambiguous ID-less tool deltas with multiple active calls must be rejected as protocol errors.

3. **For Phase P3 (Chat Relay):**
   - Must detect HTTP 200 responses that carry EndStreamResponse errors (flag `0x02`).
   - Must support code-only error JSON objects without failing JSON deserialization.
   - Must treat `ModelUsageStats` as point-in-time authoritative snapshots, not cumulative deltas.
   - Must preserve opaque reasoning signatures (`delta_signature`) verbatim.

4. **For Phase P4 (Model Discovery):**
   - Calls `POST /exa.api_server_pb.ApiServerService/GetCascadeModelConfigs` with unframed protobuf body.
   - Maps model configs to catalog using `model_uid` (e.g. `glm-5-2`), caching with 5-minute TTL.

# Devin Upstream Mock Harness Evidence

Task `st_01a09151`, node "hephaestus", model `mahoquot/gemini-3.8-flash-high`.
Scope strictly honored:
- `scripts/devin-mock.mjs`
- `scripts/devin-mock.test.mjs`
- `.omo/evidence/devin/mock-harness.md` (this report)

No Rust production/test files, P0 fixture directory, Cargo manifests, UI, or existing scripts were edited. All pre-existing dirty workspace changes were preserved untouched.

---

## 1. Overview and Architecture

`scripts/devin-mock.mjs` is an independent, development-only upstream mock server for Devin's Connect RPC services:
1. **Unary Endpoint**: `POST /exa.api_server_pb.ApiServerService/GetCascadeModelConfigs` (`application/proto`)
2. **Streaming Endpoint**: `POST /exa.api_server_pb.ApiServerService/GetChatMessage` (`application/connect+proto`)
3. **Control API**: Loopback endpoints under `/__control/*` for scenario configuration, poll-free event synchronization, and sanitized capture inspection.

### Builtin-Only Implementation (Zero NPM Dependencies)
- Implemented entirely with Bun 1.4 builtins (`Bun.serve`, `ReadableStream`, `TextEncoder`, `TextDecoder`, `DataView`, `Uint8Array`).
- Pure JavaScript independent protobuf wire parser and serializer (`encodeVarint`, `decodeVarint`, `parseFields`, `encodeDataFrame`, `encodeEndStreamFrame`, `parseConnectFrames`).
- Does **not** import or depend on the production Rust encoder, prost generated code, or any external npm package.

### Protocol and Validation Rules
- **Authorization**: Literal `Authorization: Basic <token>-<token>` header check. Rejects standard base64 Basic, Bearer, or mismatched token pairs.
- **Protobuf Wire Metadata**: Independently extracts field 1 `metadata` and field 3 `api_key`. Validates that `metadata.api_key` strictly equals `<token>`. Rejects on mismatch with HTTP 401.
- **Exact Content-Type**: Unary demands `application/proto` (415 on failure); streaming demands `application/connect+proto` (415 on failure).
- **Unprefixed Model UID**: Rejects incoming `chat_model_uid` (field 21) if prefixed with `devin/` (HTTP 400). Upstream wire model UID must be unprefixed (e.g. `glm-5-2`, `swe-1-7`).
- **Token Redaction & Sanitization**: Captures sanitize authorization headers to `Basic [REDACTED]-[REDACTED]` and body metadata to `api_key: "[REDACTED]"`. Synthetic tokens (`devin-dummy-account-a`, `dummy-token`) never leak in `/status` or capture responses.

---

## 2. CLI Usage, Startup, and Shutdown

### Startup Command
```bash
bun scripts/devin-mock.mjs [--port <port>] [--host <host>] [--scenario <scenario>] [--scenario-options '<json>']
```
- Default port: `0` (binds to an ephemeral OS loopback port)
- Default host: `127.0.0.1`
- Default scenario: `default`
- Default scenario-options: `{}`

### Readiness Contract
The process writes **exactly one line** of JSON to `stdout` upon listening:
```json
{"status":"ready","port":54321,"host":"127.0.0.1","url":"http://127.0.0.1:54321"}
```
Test drivers and test runners read this line to acquire the dynamic port without polling or fixed sleeps.

### Clean Process Shutdown
The runner registers handlers for `SIGINT` and `SIGTERM`:
- Closes the active `Bun.serve` listener cleanly (`server.stop(true)`).
- Exits with status `0`.

---

## 3. Control API Specification

All control endpoints operate on loopback under `/__control/`:

### 1. Inspect State & Sanitized Captures
- `GET /__control/state`
- Returns:
  ```json
  {
    "scenario": "default",
    "scenario_options": {},
    "request_count": 2,
    "connection_close_count": 0,
    "captures": [
      {
        "id": "req_...",
        "method": "POST",
        "path": "/exa.api_server_pb.ApiServerService/GetChatMessage",
        "headers": {
          "content-type": "application/connect+proto",
          "authorization": "Basic [REDACTED]-[REDACTED]",
          "connect-protocol-version": "1"
        },
        "sanitized_body": {
          "api_key": "[REDACTED]",
          "chat_model_uid": "glm-5-2",
          "prompt": "Hello",
          "message_count": 1,
          "has_tools": false,
          "has_tool_results": false
        },
        "client_closed_early": false,
        "timestamp": 1726000000000
      }
    ],
    "events": [...]
  }
  ```

### 2. Set Scenario and Explicit Options
- `POST /__control/scenario`
- Body:
  ```json
  {
    "scenario": "disjoint-catalogs",
    "options": {
      "account_a_token": "devin-session-token$fixture-a",
      "account_b_token": "devin-session-token$fixture-b"
    }
  }
  ```
- Response (raw options echo omitted to guarantee zero token leakage):
  ```json
  {
    "ok": true,
    "scenario": "disjoint-catalogs"
  }
  ```
  Note: Only canonical `account_a_token` and `account_b_token` options are accepted. When inspecting `GET /__control/state`, public `scenario_options` automatically redacts token values (`"[REDACTED]"`) while preserving private routing configuration in the mock runner.

### 3. Reset State
- `POST /__control/reset`
- Resets captures, counters, events, scenario options, and active gates back to initial defaults.

### 4. Release Stream Gate
- `POST /__control/gate/release`
- Unblocks paused streaming gates (used for gated chunk continuation in transport cancellation scenarios).

### 5. Event Waiter (Bounded, No Polling)
- `POST /__control/wait-event`
- Body: `{"type": "client_closed", "timeout_ms": 5000}`
- Awaits the event using an in-memory event bus or returns immediately if already recorded in current run. Times out with HTTP 408 if deadline is exceeded.

---

## 4. Supported Scenarios

| Scenario Name | Endpoint | Description & Wire Characteristics |
|---|---|---|
| `disjoint-catalogs` | Unary | Synthetic per-account discovery. Disjoint catalogs are selected by account credentials:<br>• Account A (`glm-5-2` only): `devin-dummy-account-a`, `devin-session-token$fixture-a`, or configured `options.account_a_token`.<br>• Account B (`swe-1-7` only): `devin-dummy-account-b`, `devin-session-token$fixture-b`, or configured `options.account_b_token`.<br>• Default fallback `dummy-token` returns both models; unknown tokens return empty catalog. |
| `normal` / `reasoning` | Streaming | Full server stream sequence: thinking delta (`delta_thinking`), opaque signature (`delta_signature`), redaction flag (`thinking_redacted: true`), text delta (`delta_text`), followed by clean `EndStreamResponse` (JSON `{"error": null}`). Payload lengths `[32, 28, 13, 31, 14]`, total body `143` bytes. |
| `usage` | Streaming | Delivers text delta followed by authoritative `ModelUsageStats` frame (field 7: input 12, output 34, cache write 7, cache read 5, model `glm-5-2`). |
| `tools` | Streaming | Turn 1 (no tool results in history) emits tool call deltas (`get_weather`) with `STOP_REASON_FUNCTION_CALL` (10). Turn 2 (request contains tool result) emits final answer text and `stop_reason: 0`. |
| `output-limit` | Streaming | Delivers partial text delta with `STOP_REASON_MAX_TOKENS` (3) in data frame, testing client truncated/incomplete stop reason handling. |
| `code-only-error` | Streaming | HTTP status 200 OK. Terminal Connect `EndStreamResponse` frame (flag `0x02`) with 39-byte JSON `{"error":{"code":"resource_exhausted"}}` containing only the code string and no message. |
| `late-error` | Streaming | Emits initial text data frame, followed by terminal Connect error envelope `{"error": {"code": "unavailable", "message": "server disconnected unexpectedly"}}`. |
| `transport-cancellation` | Streaming | Deliberate chunk splitting: sends initial data chunk, pauses at a controllable async gate. Detects post-first-byte client abort signal (`req.signal.abort` and `ReadableStream.cancel`), records `client_closed` event, and marks capture `client_closed_early = true`. |

---

## 5. Wire Contract Details and Measured Literal Bytes

All wire byte measurements were captured directly from runtime buffers:

### 5.1 Normal Streaming Response Frames
The normal response stream delivers 5 Connect envelope frames totaling **143 bytes**:
- **Total framed bytes**: `143` (5 frames, each with 5-byte `[flag(1), len(4, BE)]` header)
- **Frame headers**: `5 * 5 = 25` header bytes
- **Frame payload lengths**: `[32, 28, 13, 31, 14]` (sum: `118` bytes; `118 + 25 = 143` total bytes)
  1. **Frame 0 (flag `0x00`, length 32)**: Protobuf field 9 (`delta_thinking` = `"Planning the reply."`), message ID `"resp-0001"`. Frame size: `5 + 32 = 37` bytes.
  2. **Frame 1 (flag `0x00`, length 28)**: Protobuf field 10 (`delta_signature` = `"sig-opaque-0001"`). Frame size: `5 + 28 = 33` bytes.
  3. **Frame 2 (flag `0x00`, length 13)**: Protobuf field 11 (`thinking_redacted` = `1`). Frame size: `5 + 13 = 18` bytes.
  4. **Frame 3 (flag `0x00`, length 31)**: Protobuf field 3 (`delta_text` = `"Hello, I am Devin."`). Frame size: `5 + 31 = 36` bytes.
  5. **Frame 4 (flag `0x02`, length 14)**: Connect EndStreamResponse JSON `{"error":null}` (14 ASCII characters). Frame size: `5 + 14 = 19` bytes.

### 5.2 Terminal Code-Only Error Payload
In `code-only-error` scenario:
- **HTTP status**: `200 OK` (Connect protocol standard)
- **Frame flag**: `0x02` (`EndStreamResponse`)
- **Payload length**: `39` bytes
- **Total frame length**: `44` bytes (`5` header + `39` payload)
- **Literal payload string**:
  ```json
  {"error":{"code":"resource_exhausted"}}
  ```
  Omission of the `message` property tests that clients do not crash or invent empty error strings when upstream omits optional error messages.

### 5.3 Unary Account-A Wire Request
The unframed protobuf request body for Account A model discovery:
- **Token value**: `"devin-dummy-account-a"` (length: 21 characters = 21 bytes, hex `0x15`)
- **Total request wire length**: `25` bytes
- **Exact literal hex**:
  ```text
  0a171a15646576696e2d64756d6d792d6163636f756e742d61
  ```
- **Byte breakdown**:
  - `0a`: Field 1 tag (`field_number = 1`, `wire_type = 2` length-delimited) -> `(1 << 3) | 2 = 0x0a`
  - `17`: Field 1 length (23 bytes, covering subfield tag `0x1a`, subfield length `0x15`, and 21 token bytes)
  - `1a`: Field 3 tag (`field_number = 3`, `wire_type = 2` length-delimited) -> `(3 << 3) | 2 = 0x1a`
  - `15`: Field 3 length (21 bytes = length of `"devin-dummy-account-a"`)
  - `646576696e2d64756d6d792d6163636f756e742d61`: ASCII bytes for `"devin-dummy-account-a"`

---

## 6. Verification and Test Evidence

### Red Phase 1: Disjoint Fixture Tokens & Custom Options
Before adding disjoint fixture token support and custom scenario options to `scripts/devin-mock.mjs`:
```text
$ bun test scripts/devin-mock.test.mjs
bun test v1.4.0 (34cbb9a40)

scripts/devin-mock.test.mjs:
(pass) Devin Upstream Mock Harness > CLI and process lifecycle > starts as child process, prints single JSON readiness line with port, and exits on SIGTERM [42.32ms]
...
(pass) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > returns account B catalog (swe-1-7 only) for account B credentials [0.24ms]
340 |       expect(configs.length).toBe(1);
                                   ^
error: expect(received).toBe(expected)

Expected: 1
Received: 0

      at <anonymous> (.../scripts/devin-mock.test.mjs:340:30)
(fail) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > returns account A catalog for devin-session-token$fixture-a [2.62ms]
363 |       expect(configs.length).toBe(1);
                                   ^
error: expect(received).toBe(expected)

Expected: 1
Received: 0

      at <anonymous> (.../scripts/devin-mock.test.mjs:363:30)
(fail) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > returns account B catalog for devin-session-token$fixture-b [3.08ms]
398 |       expect(configsA.length).toBe(1);
                                    ^
error: expect(received).toBe(expected)

Expected: 1
Received: 0

      at <anonymous> (.../scripts/devin-mock.test.mjs:398:31)
(fail) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > supports explicit scenario options for custom disjoint account tokens [1.00ms]
...
3 tests failed:
(fail) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > returns account A catalog for devin-session-token$fixture-a [2.62ms]
(fail) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > returns account B catalog for devin-session-token$fixture-b [3.08ms]
(fail) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > supports explicit scenario options for custom disjoint account tokens [1.00ms]

 22 pass
 3 fail
 112 expect() calls
Ran 25 tests across 1 file. [1.81s]
Exit code: 1
```

### Red Phase 2: Token Leakage in Control Responses (Lead Reproduced Regression)
Before removing the raw options echo from `POST /__control/scenario` and adding token field redaction to `GET /__control/state`:
```text
$ bun test scripts/devin-mock.test.mjs
bun test v1.4.0 (34cbb9a40)

scripts/devin-mock.test.mjs:
(pass) Devin Upstream Mock Harness > CLI and process lifecycle > starts as child process, prints single JSON readiness line with port, and exits on SIGTERM [14.94ms]
...
(pass) Devin Upstream Mock Harness > Control API & Token Redaction > redacts secret tokens from headers and parsed protobuf bodies in captures [0.31ms]
885 |       expect(postText.includes(privateTokenA)).toBe(false);
                                                     ^
error: expect(received).toBe(expected)

Expected: false
Received: true

      at <anonymous> (.../scripts/devin-mock.test.mjs:885:48)
(fail) Devin Upstream Mock Harness > Control API & Token Redaction > does not leak configured scenario tokens in POST /__control/scenario or GET /__control/state [0.92ms]
(pass) Devin Upstream Mock Harness > Control API & Token Redaction > resets state, counts, and scenario on POST /__control/reset [0.18ms]

1 tests failed:
(fail) Devin Upstream Mock Harness > Control API & Token Redaction > does not leak configured scenario tokens in POST /__control/scenario or GET /__control/state [0.92ms]

 25 pass
 1 fail
 120 expect() calls
Ran 26 tests across 1 file. [1479.00ms]
Exit code: 1
```

### Green Phase (All Tests Passing)
After removing raw `options` echo from `POST /__control/scenario`, redacting token fields in `GET /__control/state`, and enforcing canonical `account_a_token` / `account_b_token` fields:
```text
$ bun test scripts/devin-mock.test.mjs
bun test v1.4.0 (34cbb9a40)

scripts/devin-mock.test.mjs:
(pass) Devin Upstream Mock Harness > CLI and process lifecycle > starts as child process, prints single JSON readiness line with port, and exits on SIGTERM [19.96ms]
(pass) Devin Upstream Mock Harness > Independent Wire and Connect Framing > encodes and decodes varints correctly [0.09ms]
(pass) Devin Upstream Mock Harness > Independent Wire and Connect Framing > parses protobuf fields from raw bytes independently [0.50ms]
(pass) Devin Upstream Mock Harness > Independent Wire and Connect Framing > encodes and parses Connect envelope frames (data & end_stream) [0.07ms]
(pass) Devin Upstream Mock Harness > Authentication and wire field validation > rejects request missing Authorization header with HTTP 401 [0.17ms]
(pass) Devin Upstream Mock Harness > Authentication and wire field validation > rejects request with malformed Authorization (not Basic <token>-<token>) [0.12ms]
(pass) Devin Upstream Mock Harness > Authentication and wire field validation > rejects request where Authorization header token does not match protobuf metadata.api_key [0.18ms]
(pass) Devin Upstream Mock Harness > Authentication and wire field validation > rejects request with wrong Content-Type with HTTP 415 [0.11ms]
(pass) Devin Upstream Mock Harness > Authentication and wire field validation > rejects GetChatMessage when Content-Type is not application/connect+proto [0.28ms]
(pass) Devin Upstream Mock Harness > Authentication and wire field validation > rejects GetChatMessage when chat_model_uid has devin/ prefix [0.30ms]
(pass) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > returns account A catalog (glm-5-2 only) for account A credentials [0.30ms]
(pass) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > returns account B catalog (swe-1-7 only) for account B credentials [0.18ms]
(pass) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > returns account A catalog for devin-session-token$fixture-a [0.15ms]
(pass) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > returns account B catalog for devin-session-token$fixture-b [0.18ms]
(pass) Devin Upstream Mock Harness > Scenario 1: Disjoint Per-Account Catalogs > supports explicit scenario options for custom disjoint account tokens [0.46ms]
(pass) Devin Upstream Mock Harness > Scenario 2: Normal Text, Reasoning, Signature, Redacted > streams thinking delta, signature, redacted flag, text delta, and end stream [0.39ms]
(pass) Devin Upstream Mock Harness > Scenario 3: Usage and Cache Snapshots > streams usage stats containing input, output, cache read/write tokens and model_uid [0.35ms]
(pass) Devin Upstream Mock Harness > Scenario 4: Tools and Tool-Result Final Turn > emits tool call deltas and STOP_REASON_FUNCTION_CALL on turn 1 (no tool result) [0.27ms]
(pass) Devin Upstream Mock Harness > Scenario 4: Tools and Tool-Result Final Turn > emits final turn text on turn 2 (when tool result is present in history) [0.27ms]
(pass) Devin Upstream Mock Harness > Scenario 5: Output Limit > returns partial text with STOP_REASON_MAX_TOKENS (3) [0.14ms]
(pass) Devin Upstream Mock Harness > Scenario 6: Terminal Code-Only Error > returns HTTP 200 with flag 0x02 EndStreamResponse containing code-only error [0.15ms]
(pass) Devin Upstream Mock Harness > Scenario 7: Late Terminal Error > streams initial data frame, then terminates with EndStreamResponse error [0.12ms]
(pass) Devin Upstream Mock Harness > Scenario 8: Transport Cancellation & Deliberate Chunks > splits chunks deliberately and records connection-close signal when client cancels post-first-byte [0.71ms]
(pass) Devin Upstream Mock Harness > Control API & Token Redaction > redacts secret tokens from headers and parsed protobuf bodies in captures [0.33ms]
(pass) Devin Upstream Mock Harness > Control API & Token Redaction > does not leak configured scenario tokens in POST /__control/scenario or GET /__control/state [0.87ms]
(pass) Devin Upstream Mock Harness > Control API & Token Redaction > resets state, counts, and scenario on POST /__control/reset [0.26ms]

 26 pass
 0 fail
 133 expect() calls
Ran 26 tests across 1 file. [675.00ms]
Exit code: 0
```

### Test Discipline Improvements
1. **Child Process Cleanup**: Spawned test child processes are guaranteed termination in `finally` blocks (`proc.kill(15)` and `await proc.exited`).
2. **Timer Leaks Prevented**: Timeout timers are cleared with `clearTimeout(timer)` immediately upon race resolution or in `finally`.
3. **Cancellation Race Prevention**: In Scenario 8, the test initiates the `/__control/wait-event` subscription promise *before* invoking `abortController.abort()` and `reader.cancel()`, ensuring zero missed disconnect signals.

---

## 7. Verified Mock Control Workflow

The mock harness is ready to serve as the upstream peer for gateway and integration acceptance tests. The verified interaction loop is:

### 1. Launch Mock Server
```bash
# Start mock server on ephemeral port
bun scripts/devin-mock.mjs --port 0
```
Read the single stdout JSON line:
```json
{"status":"ready","port":54321,"host":"127.0.0.1","url":"http://127.0.0.1:54321"}
```

### 2. Configure Scenario via Loopback Control
```bash
# Configure disjoint catalogs with explicit account tokens
curl -s -X POST "http://127.0.0.1:54321/__control/scenario" \
  -H "Content-Type: application/json" \
  -d '{
    "scenario": "disjoint-catalogs",
    "options": {
      "account_a_token": "devin-session-token$fixture-a",
      "account_b_token": "devin-session-token$fixture-b"
    }
  }'
# Returns: {"ok":true,"scenario":"disjoint-catalogs"} (raw options echo omitted to avoid token leakage)
```

### 3. Inspect Captures and Verifications
```bash
# Fetch sanitized request captures (tokens redacted)
curl -s "http://127.0.0.1:54321/__control/state" | jq .
```

### 4. Reset Mock for Next Scenario
```bash
# Reset state between test suites
curl -s -X POST "http://127.0.0.1:54321/__control/reset"
```

### 5. Terminate Mock
Send `SIGTERM` or `SIGINT` to the mock PID:
```bash
kill -TERM <MOCK_PID>
```
The mock closes all active listeners and exits cleanly with exit code 0.

### Gateway Integration Status: PENDING LEAD INTEGRATION
The gateway credential import and RPC routing are defined in the plan (`.omo/plans/devin-provider-integration.md`):
- Management import will use the planned `POST /v0/management/devin/import-cli` or auth-files upload endpoints.
- Integration tests against this mock will be run by the gateway implementation owner. No unverified gateway commands or non-existent endpoints are asserted here.

# Devin Provider E2E Execution Surface & Runner Verification

**Task ID**: `st_01a093ff`  
**Execution Node**: hephaestus  
**Model**: `mahoquot/gemini-3.8-flash-high`  
**Timestamp**: 2026-09-12T05:15:00Z  
**Target Surface**: `scripts/devin-e2e/runner.ts`, `scripts/devin-e2e/runner.test.ts`  
**Deliverable Path**: `/Users/indo/code/project/mahoquot-proxy/.omo/evidence/devin/e2e-runner.md`  
**Status**: **PASS (Self-check exit code 0; 10/10 runner tests pass; lead owns final run)**  

---

## 1. Executive Summary & Verification Boundary

This document records the delivery and verification of the minimal trustworthy real gateway + independent local Devin mock E2E execution surface for the lead's final HTTP and browser QA.

In strict compliance with prompt directives:
1. **Owned Scope**: Owned strictly `scripts/devin-e2e/*` and `.omo/evidence/devin/e2e-runner.md`. Zero product files, zero frontend files, and zero git commits were touched.
2. **Reused Existing Runner**: Reused the 1700-line `scripts/devin-e2e/runner.ts`. No new framework was invented.
3. **Faithful Failing Tests First**: Implemented `scripts/devin-e2e/runner.test.ts` to reproduce demonstrable defects (double stream reader lock, while-loop sleep polling, missing two-account and cancellation commands, missing Maho CLI QA commands). Confirmed 5 faithful test failures before applying fixes; all 10 tests now pass cleanly.
4. **Isolated Real Gateway Execution**: Started actual binary `target/debug/mahoquot-gateway` with dummy management credentials (`devin-dummy-mgmt-key`), temporary isolated directory tree under `/var/folders/.../T/`, kernel-allocated ephemeral loopback ports, and independent mock wire assertions against `scripts/devin-mock.mjs`.
5. **Event-Driven Readiness & Cleanup**: Process readiness and teardown are governed strictly by exact events (`proc.exited`, tracing stdout `"listening"` signal, TCP bind check) with bounded timeouts. Zero `while` polling and zero sleep delays.
6. **Honest In-Flight State**: Four inference surfaces are honestly audited as `PENDING` (OpenAI Chat HTTP 401 upstream pending P3 relay completion; Responses/Messages/Gemini HTTP 503 pending P5 adapters) without pretending success.
7. **GUI Automation Discipline**: Non-destructive `maho tab list` confirmed the Maho browser daemon is not running (Connection Refused on socket). No browser substitutes or kill/reset actions were taken. Concrete commands with explicit `--tab <id>` were generated for lead desktop/mobile screenshot QA.

---

## 2. Demonstrable Runner Defects Fixed with Faithful Failing Tests

Prior to modifying `runner.ts`, `scripts/devin-e2e/runner.test.ts` was authored to assert the defects:

```text
$ bun test scripts/devin-e2e/runner.test.ts
(fail) stopExternalStack must not use sleep polling in a while loop
(fail) generateLeadQACommands must provide commands for two-account scope (Account A and Account B)
(fail) generateLeadQACommands must provide commands for transport cancellation and wire checks
(fail) generateLeadQACommands must include explicit maho CLI commands with --tab <id> and tab listing
(fail) runner.ts must not call proc.stdout.getReader() multiple times on the same process
```

### Defect Analysis & Resolutions:

1. **Defect 1: Sleep Polling in `stopExternalStack`**:
   - *Problem*: `stopExternalStack` contained `while (Date.now() - start < 3000) { ... await new Promise((r) => setTimeout(r, 100)); }`. This violated the hard ban on sleep delays and polling patterns.
   - *Fix*: Replaced polling loop with clean SIGTERM dispatch, a bounded 3-second `.unref()` fallback timer for SIGKILL, and exact event-driven port release checks via `isPortAvailable`.

2. **Defect 2: Double-Locking ReadableStream on Gateway Stdout**:
   - *Problem*: In `startGatewayProcess`, `const outReader = proc.stdout.getReader();` was called during the readiness phase, and then called a second time in a background streaming loop without releasing the lock. In Bun, this throws `TypeError: ReadableStream is locked to a reader`, resulting in silent background logging failures and empty `gateway.stdout.log` files.
   - *Fix*: Replaced dual readers with a single, continuous `pumpStream` reader per stream (`stdout` and `stderr`) that writes chunks immediately to disk, flushes via `writer.flush()`, checks for `"listening"` and the ephemeral port, and resolves a `listeningPromise`.

3. **Defect 3: Early EOF Race in Gateway Readiness Check**:
   - *Problem*: The readiness check previously raced `readOut()` and `readErr()` with `Promise.race`. If `stdout` reached EOF before `stderr` emitted `"listening"`, `readOut()` resolved `undefined`, causing the race to complete with `isListening = false` and prematurely killing the process.
   - *Fix*: Bound readiness to `listeningPromise` raced against `proc.exited` and a 15s bounded timeout.

4. **Defect 4: Missing Two-Account Scope & Cancellation Commands**:
   - *Problem*: `generateLeadQACommands` provided commands for only a single account and lacked any instructions for two-account scope (`devin-alpha`, `devin-beta`) or transport cancellation.
   - *Fix*: Expanded `generateLeadQACommands` to provide complete copy-pasteable commands for two-account disjoint catalog verification (`disjoint-catalogs` scenario) and post-first-byte transport cancellation (`transport-cancellation` scenario).

5. **Defect 5: Missing Maho Browser QA with Explicit `tab_id`**:
   - *Problem*: The runner lacked concrete Maho CLI commands enforcing the explicit `--tab <id>` rule for desktop and mobile screenshot QA.
   - *Fix*: Added section 5 in `generateLeadQACommands` with non-destructive tab discovery, explicit `--tab "$TAB_ID"` flags, and desktop/mobile capture commands.

---

## 3. Test Suite Verification (`bun test`)

All 10 unit and integration tests in `scripts/devin-e2e/runner.test.ts` pass cleanly:

```text
$ cd mahoquot-proxy && bun test scripts/devin-e2e/runner.test.ts
bun test v1.4.0 (34cbb9a40)

scripts/devin-e2e/runner.test.ts:
(pass) Devin E2E Runner Defects & Invariants > Defect 1: No sleep or polling in runner code > stopExternalStack must not use sleep polling in a while loop [0.14ms]
(pass) Devin E2E Runner Defects & Invariants > Defect 2: Two-account scope commands in lead QA output > generateLeadQACommands must provide commands for two-account scope (Account A and Account B) [0.03ms]
(pass) Devin E2E Runner Defects & Invariants > Defect 3: Transport cancellation commands in lead QA output > generateLeadQACommands must provide commands for transport cancellation and wire checks [0.01ms]
(pass) Devin E2E Runner Defects & Invariants > Defect 4: Maho CLI browser QA commands with explicit tab_id > generateLeadQACommands must include explicit maho CLI commands with --tab <id> and tab listing [0.01ms]
(pass) Devin E2E Runner Defects & Invariants > Defect 5: Gateway logging must not double-lock stdout ReadableStream > runner.ts must not call proc.stdout.getReader() multiple times on the same process [0.11ms]
(pass) Devin E2E Runner Defects & Invariants > Network and Ephemeral Ports > getFreePort allocates free port and releases it immediately [3.64ms]
(pass) Devin E2E Runner Defects & Invariants > Network and Ephemeral Ports > verifyTcpConnect resolves false on closed port without hanging [4.32ms]
(pass) Devin E2E Runner Defects & Invariants > Isolated Artifacts & Config > createIsolatedArtifacts creates private directory hierarchy and valid credentials.toml [0.99ms]
(pass) Devin E2E Runner Defects & Invariants > Safe Static Console Server > startConsoleServer serves index.html read-only and stops cleanly [3.95ms]
(pass) Devin E2E Runner Defects & Invariants > Full E2EStack Process Lifecycle > spawns isolated gateway + mock + console, verifies ephemeral ports, and cleanly stops with zero leaks [51.59ms]

 10 pass
 0 fail
 39 expect() calls
Ran 10 tests across 1 file. [1.52s]
```

Upstream mock harness tests remain 100% green (26 tests, 133 assertions):

```text
$ cd mahoquot-proxy && bun test scripts/devin-mock.test.mjs
 26 pass
 0 fail
 133 expect() calls
Ran 26 tests across 1 file. [7.71s]
```

---

## 4. Live Gateway Smoke Self-Check Results

Executing `bun scripts/devin-e2e/runner.ts self-check`:

```text
$ cd mahoquot-proxy && bun scripts/devin-e2e/runner.ts self-check
[SELF-CHECK] Initializing isolated Devin E2E stack...
[SELF-CHECK] Stack ready. Running smoke test sequence...

=== DEVIN E2E SMOKE SELF-CHECK RESULTS ===

[PASS] 01_gateway_health: Gateway /healthz liveness probe
       HTTP 200 (Expected: 200)
[PASS] 02_auth_missing: Management API rejects unauthenticated request
       HTTP 401 (Expected: 401)
[PASS] 03_auth_wrong: Management API rejects invalid bearer key
       HTTP 401 (Expected: 401)
[PASS] 04_auth_valid: Management API accepts configured API key
       HTTP 200 (Expected: 200)
[PASS] 05_devin_import_cli: Import Devin CLI credentials into isolated gateway auth directory
       HTTP 200 (Expected: 200)
[PASS] 06_devin_models_refresh: Trigger Devin model discovery RPC against local mock upstream
       HTTP 200 (Expected: 200)
[PASS] 07_v1_models_list: Inspect active /v1/models catalog for discovered Devin models
       HTTP 200 (Expected: 200)
[PASS] 07a_mock_disjoint_scope: Mock upstream enforces disjoint model catalog wire separation between Account A and Account B
       HTTP 200 (Expected: 200)
[PASS] 07b_mock_transport_cancellation: Mock upstream captures post-first-byte client transport cancellation and asserts wire close event
[PENDING] 08a_inference_chat: OpenAI Chat surface (/v1/chat/completions)
       HTTP 401 (Expected: 200 (pending P3 relay completion))
       Note: Relay protocol pending P3/P5 worker completion; gateway rejects with expected diagnostic
[PENDING] 08b_inference_responses: Codex Responses surface (/v1/responses)
       HTTP 503 (Expected: 200 (pending P5 surface completion))
       Note: Responses translation pending P5 worker completion
[PENDING] 08c_inference_messages: Anthropic Messages surface (/v1/messages)
       HTTP 503 (Expected: 200 (pending P5 surface completion))
       Note: Messages translation pending P5 worker completion
[PENDING] 08d_inference_gemini: Gemini GenerateContent surface (/v1beta/...)
       HTTP 503 (Expected: 200 (pending P5 surface completion))
       Note: Gemini translation pending P5 worker completion
[PASS] 09_account_disable: Disable Devin account and verify models are removed from routing pool
       HTTP 200 (Expected: 200)
[PASS] 10_account_delete: Delete Devin account credential and verify atomic removal from auth dir
       HTTP 200 (Expected: 200)
[PASS] 11_console_static_server: Static console server serves built index.html safely read-only
       HTTP 200 (Expected: 200)

=== CLEANUP RECEIPT ===
  Killed PIDs: 5146, 5143
  Released Ports: 53634, 53632, 53633
  Artifacts Cleaned: true

Exit Code: 0
```

---

## 5. Maho CLI Browser QA Inspection & Environment Status

In accordance with GUI automation instructions:
1. **Tool Inspection**: `maho --help` and `maho tab --help` were inspected. Maho supports active tab listing (`maho tab list`), navigation (`maho tab navigate`), interaction (`maho click`, `maho type`), and desktop input (`maho desktop screenshot`).
2. **Non-Destructive Tab List**:
   ```bash
   $ maho tab list --json
   Error: Browser not running. Start Maho.app or run `maho headless --launch`.
   (socket: /Users/indo/Library/Application Support/Maho/maho.sock)
   (underlying: socket connect failed: Connection refused (os error 61))
   ```
3. **Real Environment Blocker**: The browser daemon is currently stopped. No substitute browsers (Playwright/Puppeteer/Selenium) were spawned, and no user browser state was modified or reset.
4. **Documented QA Commands**: Explicit commands utilizing `--tab "$TAB_ID"` are provided below for the lead's final browser assessment once the browser daemon is started.

---

## 6. Lead QA Execution Surface Commands

The following commands are generated by `bun scripts/devin-e2e/runner.ts commands` and directly copy-pasteable by the lead:

```bash
# ==============================================================================
# Mahoquot Proxy Devin E2E QA Commands for Lead Assessment
# Gateway Target: http://127.0.0.1:18801
# Management Key: devin-dummy-mgmt-key
# Console Target: http://127.0.0.1:18802
# Mock Target:    http://127.0.0.1:18803
# ==============================================================================

# 1. Management Auth Checks
# 1a. Missing Auth (Expect HTTP 401):
curl -s -i "http://127.0.0.1:18801/v0/management/auth-files"

# 1b. Wrong Auth (Expect HTTP 401):
curl -s -i -H "Authorization: Bearer invalid-token" "http://127.0.0.1:18801/v0/management/auth-files"

# 1c. Valid Auth (Expect HTTP 200):
curl -s -i -H "Authorization: Bearer devin-dummy-mgmt-key" "http://127.0.0.1:18801/v0/management/auth-files"

# 2. Devin Single Account Lifecycle: HTTP import -> models -> 4 surfaces -> delete
# 2a. Import Devin CLI credentials (Expect HTTP 200):
curl -s -i -X POST "http://127.0.0.1:18801/v0/management/devin/import-cli" \
  -H "Authorization: Bearer devin-dummy-mgmt-key" \
  -H "Content-Type: application/json" \
  -d '{"identity": "devin-lead-qa", "label": "Devin Lead QA Account"}'

# 2b. Model Discovery Refresh via Mock Upstream (Expect HTTP 200 with model list):
curl -s -i -X POST "http://127.0.0.1:18801/v0/management/devin/models/refresh" \
  -H "Authorization: Bearer devin-dummy-mgmt-key"

# 2c. Public Model Catalog List (Expect HTTP 200 including devin/glm-5-2):
curl -s -i "http://127.0.0.1:18801/v1/models" \
  -H "Authorization: Bearer devin-dummy-mgmt-key"

# 2d. Four Inference Surfaces (Honest Pending Assessment):
# Surface A: OpenAI Chat Surface (/v1/chat/completions):
curl -s -i -X POST "http://127.0.0.1:18801/v1/chat/completions" \
  -H "Authorization: Bearer devin-dummy-mgmt-key" \
  -H "Content-Type: application/json" \
  -d '{"model": "devin/glm-5-2", "messages": [{"role": "user", "content": "Hello Devin"}]}'

# Surface B: Codex Responses Surface (/v1/responses - Pending P5 responses adapter):
curl -s -i -X POST "http://127.0.0.1:18801/v1/responses" \
  -H "Authorization: Bearer devin-dummy-mgmt-key" \
  -H "Content-Type: application/json" \
  -d '{"model": "devin/glm-5-2", "input": [{"role": "user", "content": "Hello Devin"}]}'

# Surface C: Anthropic Messages Surface (/v1/messages - Pending P5 messages adapter):
curl -s -i -X POST "http://127.0.0.1:18801/v1/messages" \
  -H "Authorization: Bearer devin-dummy-mgmt-key" \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{"model": "devin/glm-5-2", "max_tokens": 100, "messages": [{"role": "user", "content": "Hello Devin"}]}'

# Surface D: Gemini GenerateContent Surface (/v1beta/models/... - Pending P5 gemini adapter):
curl -s -i -X POST "http://127.0.0.1:18801/v1beta/models/devin/glm-5-2:generateContent" \
  -H "Authorization: Bearer devin-dummy-mgmt-key" \
  -H "Content-Type: application/json" \
  -d '{"contents": [{"parts": [{"text": "Hello Devin"}]}]}'

# 2e. Account Lifecycle: Disable (Expect HTTP 200 with disabled: true):
curl -s -i -X PATCH "http://127.0.0.1:18801/v0/management/auth-files/status" \
  -H "Authorization: Bearer devin-dummy-mgmt-key" \
  -H "Content-Type: application/json" \
  -d '{"name": "devin-devin-lead-qa.json", "disabled": true}'

# 2f. Account Lifecycle: Delete (Expect HTTP 200 with status: ok):
curl -s -i -X DELETE "http://127.0.0.1:18801/v0/management/auth-files?name=devin-devin-lead-qa.json" \
  -H "Authorization: Bearer devin-dummy-mgmt-key"

# 3. Two-Account Scope & Disjoint Catalog Verification
# 3a. Configure Mock for disjoint-catalogs scenario (Account A has glm-5-2, Account B has swe-1-7):
curl -s -i -X POST "http://127.0.0.1:18803/__control/scenario" \
  -H "Content-Type: application/json" \
  -d '{"scenario": "disjoint-catalogs", "options": {"account_a_token": "devin-dummy-account-a", "account_b_token": "devin-dummy-account-b"}}'

# 3b. Import Account Alpha (devin-alpha):
curl -s -i -X POST "http://127.0.0.1:18801/v0/management/devin/import-cli" \
  -H "Authorization: Bearer devin-dummy-mgmt-key" \
  -H "Content-Type: application/json" \
  -d '{"identity": "devin-alpha", "label": "Devin Alpha Account"}'

# 3c. Import Account Beta (devin-beta):
curl -s -i -X POST "http://127.0.0.1:18801/v0/management/devin/import-cli" \
  -H "Authorization: Bearer devin-dummy-mgmt-key" \
  -H "Content-Type: application/json" \
  -d '{"identity": "devin-beta", "label": "Devin Beta Account"}'

# 3d. Trigger Discovery Refresh across both accounts:
curl -s -i -X POST "http://127.0.0.1:18801/v0/management/devin/models/refresh" \
  -H "Authorization: Bearer devin-dummy-mgmt-key"

# 3e. Inspect Discovered Models Per Account:
curl -s -i "http://127.0.0.1:18801/v0/management/devin/models/status" \
  -H "Authorization: Bearer devin-dummy-mgmt-key"

# 3f. Clean up Two Accounts:
curl -s -i -X DELETE "http://127.0.0.1:18801/v0/management/auth-files?name=devin-devin-alpha.json" -H "Authorization: Bearer devin-dummy-mgmt-key"
curl -s -i -X DELETE "http://127.0.0.1:18801/v0/management/auth-files?name=devin-devin-beta.json" -H "Authorization: Bearer devin-dummy-mgmt-key"

# 4. Transport Cancellation & Wire Assertion Checks
# 4a. Set Mock to transport-cancellation scenario (streams first chunk and holds at gate):
curl -s -i -X POST "http://127.0.0.1:18803/__control/scenario" \
  -H "Content-Type: application/json" \
  -d '{"scenario": "transport-cancellation"}'

# 4b. Trigger streaming chat request and deliberately abort client transport post-first-byte (timeout 1s):
curl -s -N -m 1 -X POST "http://127.0.0.1:18801/v1/chat/completions" \
  -H "Authorization: Bearer devin-dummy-mgmt-key" \
  -H "Content-Type: application/json" \
  -d '{"model": "devin/glm-5-2", "messages": [{"role": "user", "content": "Cancel me"}], "stream": true}' || true

# 4c. Verify Mock Wire Closed Event & Connection Close Assertion:
curl -s -i "http://127.0.0.1:18803/__control/state"

# 5. GUI Automation & Maho CLI Browser QA (Strict tab_id discipline)
# 5a. Non-destructive browser availability check:
maho tab list --json
# If browser not running (socket refused), report status cleanly. Do not use substitutes or kill user browser.

# 5b. When browser is available, open console in new tab:
# maho open "http://127.0.0.1:18802/"
# TARGET_TAB=$(maho tab list --json | jq -r '.[0].id // empty')

# 5c. Desktop screenshot QA with explicit tab_id:
# maho desktop screenshot --tab "$TARGET_TAB" --output /tmp/devin-console-desktop.png

# 5d. Mobile viewport screenshot QA with explicit tab_id:
# maho desktop screenshot --tab "$TARGET_TAB" --mobile --output /tmp/devin-console-mobile.png

# 5e. Clean Tab Close:
# maho tab close --tab "$TARGET_TAB"

# 6. Browser Developer Console LocalStorage Setup
# Open http://127.0.0.1:18802/ in browser.
# Run in Developer Console (F12):
#   localStorage.setItem('mahoquot.base', 'http://127.0.0.1:18801');
#   localStorage.setItem('mahoquot.key', 'devin-dummy-mgmt-key');
#   location.reload();
# ==============================================================================
```

---

## 7. Artifact Audit & Owned Scope Conformance

1. **Touched Files**:
   - `scripts/devin-e2e/runner.ts` (Modified existing runner: eliminated double reader lock, removed while-loop sleep polling, added two-account/cancellation QA commands and wire checks)
   - `scripts/devin-e2e/runner.test.ts` (Added test suite: faithful failing tests for defects, all 10 passing)
   - `.omo/evidence/devin/e2e-runner.md` (Added evidence document)
2. **Product / Frontend Files**: Zero modified.
3. **Git Commits**: Zero created.
4. **Model Constraint**: Executed strictly using assigned `mahoquot/gemini-3.8-flash-high`.

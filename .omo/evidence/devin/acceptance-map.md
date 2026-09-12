# Devin Provider Integration Acceptance Map

**Document Path**: `/Users/indo/code/project/mahoquot-proxy/.omo/evidence/devin/acceptance-map.md`  
**Plan Reference**: `/Users/indo/code/project/mahoquot-proxy/.omo/plans/devin-provider-integration.md`  
**Date**: 2026-09-12  
**Target Model**: `mahoquot/gemini-3.8-flash-high` (literal `:high` is unsupported by the API; model identifier is `mahoquot/gemini-3.8-flash-high`; old GLM instructions in notepad are stale history and explicitly superseded)  
**Assigned Task ID**: `st_01a09404`  
**Parent Session**: `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Root Session**: `01a090bb-807b-7869-b40a-dbc89de2af7a`  

---

## 1. Executive Summary & Verification Boundary

This document establishes the canonical acceptance mapping for the Devin LLM provider integration into `mahoquot-proxy` and `quotio-rs`. It connects each requirement from phases P0 through P7, all 14 mandatory regression cases, all 8 final acceptance conditions, all 7 initial uncertainties, the plan's 20 core architectural contracts (including reference-parity registration), and the complete 12-command verification matrix to concrete implementation artifacts, validation commands, and real-surface scenarios.

### Lead Acceptance & Current Operational Status
- **P0 (Wire Criteria & Independent Contract Fixtures)**: **ACCEPTED BY LEAD**. Pinned protobuf schema matches upstream commit `ced035d` 100% byte-for-byte. Verified via `cargo test -p mahoquot-gateway --test devin_wire_fixtures` (15 passed, exit 0, LSP 0 clean).
- **P1 (Account Credentials & Registry Policy)**: **ACCEPTED BY LEAD**. Verified via `cargo test -p mahoquot-providers --test devin_credentials` (41 passed, exit 0), `cargo test -p mahoquot-gateway --test devin_credentials` (16 passed, exit 0), `cargo test -p mahoquot-registry` (55 passed across 7 test suites, exit 0), and `cargo test -p mahoquot-gateway --lib` (322 passed, exit 0).
- **P2 (Wire Codec & Protobuf Transformation)**: **ACCEPTED BY LEAD**. Verified via `cargo test -p mahoquot-gateway --test devin_wire` (40 passed, exit 0), `cargo test -p mahoquot-gateway --test devin_wire_fixtures` (15 passed, exit 0), and `cargo test -p mahoquot-gateway --test provider_finish_contracts` (6 passed, exit 0).
- **Lead Gateway Test Suite Rerun**: The lead re-executed the primary gateway integration test suite with **exit code 0**:
  ```bash
  cargo test -p mahoquot-gateway --test devin_relay --test devin_catalog --test devin_wire --test devin_responses_adapter --test provider_finish_contracts
  ```
  Results: `devin_relay` (19 passed), `devin_catalog` (20 passed), `devin_wire` (40 passed), `devin_responses_adapter` (21 passed), `provider_finish_contracts` (6 passed).
- **Reference-Parity Contract Registration**: **REGISTERED & VERIFIED**. Parity registered in `mahoquot-proxy/crates/gateway/tests/data/reference-parity.json` (`auth-devin-cli`, `auth-devin-token`, adapter `devin`, provider `devin`), `quotio-rs/docs/reference-parity.md` (authoritative documentation), and enforced by `mahoquot-proxy/crates/gateway/tests/t20_reference_parity.rs` (1 passed, exit 0). The proxy documentation stub `mahoquot-proxy/docs/reference-parity.md` is preserved untouched.
- **P3 (Chat Relay & Transport Dispatch)**: **VERIFIED VIA INTEGRATION SUITE**. Relay wire contract, streaming/non-streaming dispatch, HTTP 200 Connect error mapping, usage extraction, and downstream commitment boundaries verified by `devin_relay` (19 passed, exit 0). Multi-surface and real gateway E2E runner pending.
- **P4 (Model Discovery & Dynamic Routing)**: **P4 FIXES RUNNING — DO NOT CALL COMPLETED**. `devin_catalog` suite passed 20 tests (exit 0) under lead rerun. Active fix lane in progress to finalize account pool and runtime state snapshot integration.
- **P5 (Remaining Client API Surfaces & Multi-Turn Tools)**: **NOT INTEGRATED YET — DO NOT CALL COMPLETED**. Responses adapter tested via `devin_responses_adapter` (21 passed, exit 0); Anthropic Messages and Gemini multi-turn tool surface integration pending.
- **P6 (Console Management, UI & Observability)**: **P6 API METADATA RUNNING — DO NOT CALL COMPLETED**. Frontend filename identity fix accepted; scoped model preservation repair running; backend model discovery metadata integration running. Direct frontend display defect under active repair.
- **Auxiliary Preparation Worker**: **RUNNER PREP RUNNING — DO NOT CALL COMPLETED**. Task owns scripts under `scripts/devin-e2e/` and `e2e-runner.md`.
- **P7 (Release Gate & Verification Synthesis)**: **PENDING**. Full workspace build, clippy, formatting audit, real gateway E2E against local mock upstream, and release evidence synthesis.

### Critical Verification Boundaries
1. **Local Mock Harness vs. Live Devin Verification**:
   - `scripts/devin-mock.mjs` is an independent local fixture harness (zero npm dependencies, Bun 1.4 builtins).
   - Local mock literal measurements (normal response 143 B with payload lengths `[32, 28, 13, 31, 14]`; code-only error 44 B with payload 39 B; late-error 121 B with payloads `[34, 77]`; 26 tests passed) are **LOCAL MOCK measurements only**.
   - These are **NOT** live Devin responses or live credentials. Live server verification has **NEVER** occurred. Upstream Devin live status remains strictly **EXPERIMENTAL**.
2. **Browser QA Environment Blocker (Not a Product Failure)**:
   - Real browser desktop/mobile verification remains **PENDING**.
   - Default Maho socket connection was refused. Official launch command `maho headless --launch --keep-alive --url about:blank --extract title --json` exited 1: browser socket did not appear within 15s (monitor `mon_ME2A9C1P4WQCHHX2` / `bash24`).
   - Read-only `lsof` located a running Maho process (PID 23979) utilizing `/Users/indo/Library/Application Support/Chromium/maho.sock`; explicit socket queries also refused.
   - Non-destructive discipline was preserved: no browser kill, reset, or restart was executed, and no fake screenshots were generated. Real browser desktop/mobile verification remains pending while implementation lanes proceed.
3. **Real Gateway E2E Pending**:
   - Full gateway lifecycle (`import -> models -> inference -> delete`) on real gateway endpoints against the mock upstream remains **PENDING**. Standalone mock tests alone do not satisfy this requirement.

---

## 2. Baseline Audit: Known Baseline Failures vs. New Devin Claims

The pre-implementation baseline recorded in `.omo/evidence/devin/baseline.md` is audited against current repository execution evidence:

| Component | Test / Inspection Command | Recorded Baseline State | Current Resolution & Status | Relation to Devin Integration |
|---|---|---|---|---|
| **Rust Gateway Lib Initializers** | `cargo test -p mahoquot-gateway --lib` | `error[E0063]: missing field email in initializer of account::GenericAccount` across 5 locations (exit code 101). | **RESOLVED**: Gateway library compiles cleanly and **322 tests pass** (exit code 0). | Pre-existing baseline compiler blocker resolved during P1 verification. Unblocks gateway library testing. |
| **Console Frontend Lint** | `bun run lint` (in `monitor-ui/frontend`) | 18 errors across 133 checked files (exit code 1). | **BASELINE FAILURE (LEAD VERIFIED COUNT & EXIT ONLY)**: Exit code 1, **20 errors across 135 files** (monitor `mon_BDH4FM7PA07M16HN` / `bash_23`). Lead verified count 20 / 135 / exit 1 only; previous 18+2 baseline attribution and claim of no new Devin errors are previous worker reports, not newly re-proven by this lead run. Provenance distinctions preserved. | Pre-existing lint errors remain untouched per zero-blanket-reformat rule. |
| **Console Model Alignment Test** | `bun run test -- src/__tests__/task15-runtime-model-alignment.test.tsx` | Failed with `expected 83, got 84` due to hardcoded catalog count. | **RESOLVED**: Full frontend suite passes (45 test files, 401 tests pass cleanly). | Downstream catalog count assertion accommodated; all tests pass. |
| **Frontend Unit & Integration Suite** | `bun run typecheck && bun run test` | Baseline: 43 test files, 372 tests passed. | **PASSED (LEAD VERIFIED)**: Exit code 0, **45 files, 401 tests passed in 11.64s** (monitor `mon_KJ6F182NJCMZ9RQ6` / `bash_21`). `schemas.ts` physical-path LSP: clean / 0 diagnostics. Pre-existing App `act(...)` console warnings remain. | Unit and integration regressions pass completely. (Note: NOT browser or real gateway E2E). |
| **Frontend Isolated Build** | `bun run build -- --outDir /tmp/devin-console-lead-final-build` | Baseline had unbuilt changes in frontend source. | **PASSED (LEAD VERIFIED)**: Exit code 0, 3.77s; output singlefile `index.html` 875.29 kB (gzip 304.45 kB). Monitor `mon_JYE27NT628E84A9G` / `bash_22`. | Verified isolated bundle generation. Shipped proxy bundle sync (`bun run sync:proxy`) intentionally deferred. |
| **Workspace Formatting** | `cargo fmt --all --check` | Pre-existing formatting diffs across unrelated files in working tree. | **BASELINE FAILURE PRESERVED**: Exit code 1 due to pre-existing formatting diffs in `t25_account_management.rs`, `t27_scoped_key_enforcement.rs`, `t3_failover.rs`, `zcode.rs`, etc. | Zero blanket formatting applied; no unrelated production files touched. |
| **Devin Providers Crate** | `cargo test -p mahoquot-providers` | New provider implementation. | **PASSED (ACCEPTED BY LEAD)**: 116 tests pass cleanly across 7 suites (including 41 tests in `devin_credentials.rs`). | P1 credentials invariant fully verified. |
| **Devin Registry Crate** | `cargo test -p mahoquot-registry` | New provider policy and contribution models. | **PASSED (ACCEPTED BY LEAD)**: 55 tests pass cleanly across 7 suites (including 9 tests in `devin_tests.rs`). | P1 registry invariant fully verified. |
| **Devin Wire Fixtures** | `cargo test -p mahoquot-gateway --test devin_wire_fixtures` | New independent fixture suite. | **PASSED (ACCEPTED BY LEAD)**: 15 tests pass cleanly, exit 0, LSP 0. | P0 contract invariant fully verified. |
| **Devin Wire Codec** | `cargo test -p mahoquot-gateway --test devin_wire` | New Connect envelope codec. | **PASSED (ACCEPTED BY LEAD / RERUN)**: 40 tests pass cleanly, exit 0. | P2 wire codec invariant fully verified. |
| **Devin Relay Suite** | `cargo test -p mahoquot-gateway --test devin_relay` | New Chat relay transport suite. | **PASSED (LEAD RERUN)**: 19 tests pass cleanly, exit 0. | P3 Chat relay transport verified. |
| **Devin Catalog Suite** | `cargo test -p mahoquot-gateway --test devin_catalog` | New dynamic discovery & catalog suite. | **PASSED (LEAD RERUN)**: 20 tests pass cleanly, exit 0. | P4 dynamic discovery logic verified; pool integration fixes running. |
| **Devin Responses Adapter Suite** | `cargo test -p mahoquot-gateway --test devin_responses_adapter` | New Responses surface adapter suite. | **PASSED (LEAD RERUN)**: 21 tests pass cleanly, exit 0. | Surface translation logic verified; P5 not integrated yet. |
| **Provider Finish Contracts Suite** | `cargo test -p mahoquot-gateway --test provider_finish_contracts` | Renderer finish contract suite. | **PASSED (LEAD RERUN)**: 6 tests pass cleanly, exit 0. | Multi-turn and finish contract regressions verified. |
| **Reference Parity Suite** | `cargo test -p mahoquot-gateway --test t20_reference_parity` | Parity manifest and locator suite. | **PASSED (VERIFIED)**: 1 test passes cleanly, exit 0. | Devin authentication, adapter, and test owner registered. |
| **Devin Upstream Mock** | `bun test scripts/devin-mock.test.mjs` | New independent local mock harness. | **PASSED (ACCEPTED BY LEAD)**: 26 tests pass cleanly (133 assertions), exit 0. | Local mock contract verified. Strictly local mock measurements. |

---

## 3. Required Command Matrix: Complete Plan §7 Audit

All 12 commands defined in Plan §7 are tracked below with their exact current status:

```bash
# mahoquot-proxy commands
1. cargo fmt --all --check
   Status: BASELINE FAILURE (exit 1). Pre-existing formatting diffs across unrelated files. Preserved untouched per prompt block.
2. cargo test -p mahoquot-registry
   Status: PASSED (exit 0). 55 tests pass across 7 suites (including 9 in devin_tests.rs).
3. cargo test -p mahoquot-providers
   Status: PASSED (exit 0). 116 tests pass across 7 suites (including 41 in devin_credentials.rs).
4. cargo test -p mahoquot-gateway
   Status: PARTIAL / TARGETED SUITES PASSED (LEAD RERUN EXIT 0). Whole-crate command pending completion of concurrent lanes. Named subsets passed individually:
   - devin_relay: 19 passed, exit 0 (lead rerun)
   - devin_catalog: 20 passed, exit 0 (lead rerun)
   - devin_wire: 40 passed, exit 0 (lead rerun)
   - devin_responses_adapter: 21 passed, exit 0 (lead rerun)
   - provider_finish_contracts: 6 passed, exit 0 (lead rerun)
   - devin_wire_fixtures: 15 passed, exit 0 (P0 accepted)
   - devin_credentials: 16 passed, exit 0 (P1 accepted)
   - t20_reference_parity: 1 passed, exit 0 (parity registration verified)
   - mahoquot-gateway --lib: 322 passed, exit 0
5. cargo clippy --workspace --all-targets -- -D warnings
   Status: PENDING. Scheduled for final release gate P7.
6. cargo build --workspace
   Status: PENDING. Full workspace build scheduled for P7.

# quotio-rs monitor-ui frontend commands
7. bun run typecheck
   Status: PASSED (exit 0). tsc --noEmit reports 0 diagnostics.
8. bun run lint
   Status: BASELINE FAILURE (LEAD VERIFIED COUNT & EXIT ONLY). Monitor mon_BDH4FM7PA07M16HN / bash_23, exit 1, 20 errors across 135 files. Lead verified count 20 / 135 / exit 1 only; previous 18+2 baseline attribution and claim of no new Devin errors are previous worker reports, not newly re-proven by this lead run. Provenance distinctions preserved.
9. bun run test
   Status: PASSED (exit 0). 45 test files, 401 tests pass in 11.64s (including 19 in provider-catalog.test.ts). Pre-existing App act(...) console warnings remain.
10. bun run build
    Status: PASSED (exit 0). Isolated build to /tmp/devin-console-lead-final-build produces singlefile index.html (875.29 kB, gzip 304.45 kB).
11. bun run test:e2e
    Status: PENDING. Requires active running gateway and live browser environment. Preparation worker st_01a0938a owns harness setup; runner prep running.
12. bun run sync:proxy
    Status: PENDING (INTENTIONALLY DEFERRED). Deferred to protect mahoquot-proxy/ui/index.html until final lead acceptance.
```

---

## 4. Plan Architectural Contracts Mapping (Plan §5 & §6)

Every concrete implementation contract required by Plan §5 and §6 is mapped below to concrete source code, tests, evidence, and current operational status:

| # | Contract Domain (Plan §5) | Implementation Source Artifact | Validation Test Suite / Scenario | Supporting Evidence | Operational Status |
|---|---|---|---|---|:---:|
| **C01** | **Provider ID**<br>`devin()` provider identifier without aliasing `codeium`/`windsurf` | `crates/registry/src/lib.rs:58`<br>`ProviderId::devin()` | `crates/registry/tests/devin_tests.rs`<br>`test_devin_provider_id_and_canonical_slug` | `.omo/evidence/devin/p1-registry.md` | **ACCEPTED BY LEAD** (P1) |
| **C02** | **Model SST Policy**<br>`Discovered` provider policy and binding in catalog SST | `crates/registry/catalog/models-v1.json`<br>`"devin": "discovered"` | `crates/registry/tests/devin_tests.rs`<br>`models_v1_json_contains_devin_discovered_policy` | `.omo/evidence/devin/p1-registry.md` | **ACCEPTED BY LEAD** (P1) |
| **C03** | **Credentials Schema & Parsing**<br>`DevinAccount`, TOML CLI parser, literal header, secret redaction | `crates/providers/src/devin.rs`<br>`crates/providers/src/lib.rs` | `crates/providers/tests/devin_credentials.rs`<br>41 contract tests | `.omo/evidence/devin/p1-credentials.md`<br>`.omo/evidence/devin/p1-verification.md` | **ACCEPTED BY LEAD** (P1) |
| **C04** | **Account Pool Integration**<br>`ProviderKind::Devin`, `ProviderAccount::Devin` variants and loading | `crates/gateway/src/account.rs`<br>`crates/gateway/src/state.rs` | `crates/gateway/tests/devin_credentials.rs`<br>16 pool and credential tests | `.omo/evidence/devin/p1-accounts.md`<br>`.omo/evidence/devin/p1-verification.md` | **ACCEPTED (P1) / P4 FIXES RUNNING** |
| **C05** | **Management Credential APIs**<br>Manual upload validation and host CLI import (`POST /v0/management/devin/import-cli`) | `crates/gateway/src/management/creds.rs`<br>`devin_import_cli` | `crates/gateway/tests/devin_credentials.rs`<br>`test_cli_import_resolved_on_proxy_host_atomic_and_unchanged_source` | `.omo/evidence/devin/p1-credentials.md`<br>`.omo/evidence/devin/p1-verification.md` | **ACCEPTED BY LEAD** (P1) |
| **C06** | **Model Runtime Synthesis**<br>Merging discovered contributions with per-account allowlists | `crates/gateway/src/runtime_state.rs`<br>`registry_with_account_contributions` | `crates/gateway/tests/devin_catalog.rs`<br>20 tests passed in lead rerun | `.omo/evidence/devin/p6-models-verification.md` | **P4 FIXES RUNNING — NOT COMPLETED** |
| **C07** | **Management Model Registry**<br>Distinguishing signed global catalog from per-account Devin discovery | `crates/gateway/src/management/registry.rs`<br>`crates/gateway/src/models_route.rs` | `crates/gateway/tests/devin_catalog.rs`<br>20 tests passed in lead rerun | `.omo/evidence/devin/p6-models-verification.md` | **P4 FIXES RUNNING — NOT COMPLETED** |
| **C08** | **Request Planning**<br>Devin RPC target resolution and protobuf request planning | `crates/gateway/src/relay.rs`<br>`resolve_target` | `crates/gateway/tests/devin_relay.rs`<br>19 tests passed in lead rerun | `.omo/evidence/devin/p3-relay.md`<br>`.omo/evidence/devin/p3-verification.md` | **VERIFIED VIA LEAD RERUN (EXIT 0)** |
| **C09** | **Upstream Wire Transport**<br>`application/connect+proto`, HTTP/1.1 POST to `server.codeium.com` | `crates/gateway/src/relay.rs`<br>`send_upstream`, binary stream gate | `crates/gateway/tests/devin_relay.rs`<br>19 tests passed in lead rerun | `.omo/evidence/devin/p3-relay.md`<br>`.omo/evidence/devin/p3-verification.md` | **VERIFIED VIA LEAD RERUN (EXIT 0)** |
| **C10** | **Input Normalization**<br>Preserving token limits, stream flags, and role structure | `crates/gateway/src/relay.rs`<br>`build_plan` | `crates/gateway/tests/devin_relay.rs`<br>19 tests passed in lead rerun | `.omo/evidence/devin/p3-relay.md`<br>`.omo/evidence/devin/p3-verification.md` | **VERIFIED VIA LEAD RERUN (EXIT 0)** |
| **C11** | **Routing & Scoped Keys**<br>Devin binding selection, account pinning, scoped key isolation | `crates/gateway/src/relay.rs`<br>`resolve_route`, `member_provider_id` | `crates/gateway/tests/devin_catalog.rs`<br>20 tests passed in lead rerun | `.omo/evidence/devin/p6-models-verification.md` | **P4 FIXES RUNNING — NOT COMPLETED** |
| **C12** | **URL Construction**<br>Devin base URL combined with Connect RPC paths | `crates/gateway/src/url.rs`<br>`build_provider_url` | `crates/gateway/tests/devin_relay.rs`<br>19 tests passed in lead rerun | `.omo/evidence/devin/p3-relay.md` | **VERIFIED VIA LEAD RERUN (EXIT 0)** |
| **C13** | **Protobuf Schema & Types**<br>Exact field tags and proto2 presence from upstream reference | `crates/gateway/src/compat/devin_proto.rs`<br>`crates/gateway/src/compat/devin.proto` | `crates/gateway/tests/devin_wire_fixtures.rs`<br>15 independent fixture tests | `.omo/evidence/devin/p0-verification.md` | **ACCEPTED BY LEAD** (P0) |
| **C14** | **Connect Decoder & Codec**<br>Envelope framing, 16 MiB guard, stop reasons, tools, usage | `crates/gateway/src/compat/devin.rs`<br>`DevinDecoder`, `build_chat_request` | `crates/gateway/tests/devin_wire.rs`<br>40 tests passed in lead rerun | `.omo/evidence/devin/p2-verification.md` | **ACCEPTED BY LEAD** (P2) |
| **C15** | **Protocol Pipeline Dispatch**<br>`Protocol::Devin` variant, parser integration, streaming body | `crates/gateway/src/compat/mod.rs`<br>`Protocol::Devin`, `StreamingBodyParams` | `crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/devin_relay.rs` | `.omo/evidence/devin/p2-codec.md`<br>`.omo/evidence/devin/p3-relay.md` | **VERIFIED VIA CODEC & RELAY SUITES** |
| **C16** | **Common Events Mapping**<br>`CodexEvent::ReasoningSignature`, `ReasoningRedacted`, stable tool deltas | `crates/gateway/src/compat/events.rs`<br>`crates/gateway/src/compat/render.rs` | `crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/provider_finish_contracts.rs` | `.omo/evidence/devin/p2-codec.md`<br>`.omo/evidence/devin/p2-verification.md` | **ACCEPTED BY LEAD** (P2) |
| **C17** | **Client Surface Adapters**<br>Chat, Responses, Anthropic Messages, Gemini translation | `crates/gateway/src/compat/responses.rs`<br>`crates/gateway/src/compat/claude.rs`<br>`crates/gateway/src/compat/gemini.rs` | `crates/gateway/tests/devin_responses_adapter.rs`<br>21 tests passed in lead rerun | `.omo/evidence/devin/p5-responses-adapter.md`<br>`.omo/evidence/devin/p5-adapters-verification.md` | **P5 NOT INTEGRATED YET — NOT COMPLETED** |
| **C18** | **Terminal Success & Error Handling**<br>Connect HTTP 200 error envelope mapping, no false success | `crates/gateway/src/relay.rs`<br>`finish_success`, `DevinOutcome` | `crates/gateway/tests/devin_relay.rs`<br>`crates/gateway/tests/provider_finish_contracts.rs` | `.omo/evidence/devin/p3-relay.md` | **VERIFIED VIA CODEC & RELAY SUITES** |
| **C19** | **Usage & Quota Accounting**<br>Authoritative usage snapshot (field 7), cache tokens separated, quota unknown | `crates/gateway/src/usage.rs`<br>`crates/gateway/src/telemetry.rs`<br>`crates/gateway/src/request_history.rs` | `crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/devin_relay.rs` | `.omo/evidence/devin/p2-verification.md`<br>`.omo/evidence/devin/p3-relay.md` | **VERIFIED VIA CODEC & RELAY SUITES** |
| **C20** | **Parity Contracts & Registration**<br>Authoritative registration in reference parity docs, data, and tests | `quotio-rs/docs/reference-parity.md`<br>`crates/gateway/tests/data/reference-parity.json`<br>`crates/gateway/tests/t20_reference_parity.rs` | `cargo test -p mahoquot-gateway --test t20_reference_parity`<br>1 passed, exit 0 | `.omo/evidence/devin/acceptance-map.md` | **REGISTERED & VERIFIED (EXIT 0)** |

---

## 5. Detailed Scenario Matrix: All 14 Mandatory Regression Cases

| # | Regression Case Name & Invariant | Core Invariant Under Test | Implementation Artifact(s) | Concrete Validation Command / Scenario | Acceptance Status |
|---|---|---|---|---|:---:|
| **1** | **CLI Credentials Resolution & Immutability** | `DEVIN_CREDENTIALS_PATH` env > `XDG_DATA_HOME` > `~/.local/share/devin/credentials.toml`. Rejects missing/corrupt/empty TOML. Token replacement preserves identity slug. Original host file bytes NEVER modified or removed. | `crates/providers/src/devin.rs`<br>`crates/providers/tests/devin_credentials.rs` | `cargo test -p mahoquot-providers --test devin_credentials`<br>41 tests assert path precedence, corrupt syntax, and file byte-immutability. | **ACCEPTED BY LEAD** (P1) |
| **2** | **Authentication Header & Metadata Wire Parity** | `Authorization: Basic <token>-<token>` literal ASCII (NOT base64 `user:pass`). Same token mirrored in protobuf `Metadata.api_key = 3`. Content-Type strictly `application/connect+proto` (streaming) or `application/proto` (unary); no duplicates or overwrites. | `crates/gateway/src/compat/devin.rs`<br>`crates/providers/src/devin.rs`<br>`crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/devin_wire_fixtures.rs`<br>`crates/gateway/tests/devin_relay.rs` | `cargo test -p mahoquot-gateway --test devin_wire -- metadata_keeps_api_key_and_literal_header_contract`<br>`cargo test -p mahoquot-gateway --test devin_wire_fixtures -- literal_basic_dummy_token_header`<br>`cargo test -p mahoquot-providers --test devin_credentials -- auth_header_is_literal_basic_token_token_not_base64`<br>`cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_wire_contract_assertions` | **ACCEPTED BY LEAD (P1/P2) & VERIFIED IN RELAY RERUN** (Live server unverified) |
| **3** | **Connect Envelope Fragmentation & Multibyte UTF-8** | Connect 5-byte header split across 1..4 byte boundaries; payload split across arbitrary chunk boundaries; multiple frames combined in single chunk; Korean 3-byte UTF-8 split mid-character across TRANSPORT CHUNKS with complete valid strings per individual protobuf frame (malformed cross-frame string splitting rejected as invalid protobuf); empty deltas; unknown protobuf field tags ignored. | `crates/gateway/src/compat/devin.rs`<br>`crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/devin_wire_fixtures.rs` | `cargo test -p mahoquot-gateway --test devin_wire -- korean_text_survives_every_chunk_split`<br>`cargo test -p mahoquot-gateway --test devin_wire_fixtures -- korean_utf8_complete_strings_and_arbitrary_transport_splits`<br>`cargo test -p mahoquot-gateway --test devin_wire_fixtures -- malformed_utf8_rejected_negative_fixture` | **ACCEPTED BY LEAD** (P0 & P2) |
| **4** | **Connect Protocol Error Envelopes & Malformed Frames** | Frame >16 MiB rejected immediately (`MAX_FRAME_SIZE`); truncated EOF rejected; invalid protobuf rejected; code-only EndStreamResponse error (`{"error":{"code":"resource_exhausted"}}`) parsed cleanly without requiring `message`; duplicate terminal rejected. | `crates/gateway/src/compat/devin.rs`<br>`crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/devin_wire_fixtures.rs` | `cargo test -p mahoquot-gateway --test devin_wire -- oversize_frame_is_rejected_without_buffering`<br>`cargo test -p mahoquot-gateway --test devin_wire -- code_only_terminal_error_fails_with_code_preserved`<br>`cargo test -p mahoquot-gateway --test devin_wire_fixtures -- terminal_code_only_error` | **ACCEPTED BY LEAD** (P0 & P2) |
| **5** | **Reasoning, Signatures, Redaction & Interleaved Tools** | `delta_thinking` mapped to `ReasoningDelta`; `delta_signature` preserved opaque; `thinking_redacted: true` mapped to redacted event; interleaved tool calls (`call_001`, `call_002`) keep stable index; malformed JSON args rejected (never fixed to `{}`). | `crates/gateway/src/compat/devin.rs`<br>`crates/gateway/src/compat/events.rs`<br>`crates/gateway/src/compat/render.rs`<br>`crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/provider_finish_contracts.rs` | `cargo test -p mahoquot-gateway --test devin_wire -- interleaved_tool_indices_are_stable_across_text_and_reasoning`<br>`cargo test -p mahoquot-gateway --test devin_wire -- stream_decodes_thinking_signature_redaction_text_tool_and_usage`<br>`cargo test -p mahoquot-gateway --test provider_finish_contracts` (6 passed in lead rerun) | **ACCEPTED BY LEAD (P2) & VERIFIED IN RERUN** |
| **6** | **Stream vs Non-Stream Parity & Usage Accounting** | Non-stream and stream produce identical final text, tool arguments, and usage totals. Authoritative snapshot from `ModelUsageStats` (field 7): `total = input + output`; cache tokens NOT double-counted in total. Missing usage handled safely. | `crates/gateway/src/compat/devin.rs`<br>`crates/gateway/src/relay.rs`<br>`crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/devin_relay.rs` | `cargo test -p mahoquot-gateway --test devin_wire -- usage_is_none_when_absent_and_final_snapshot_wins_without_double_count`<br>`cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_streaming_success`<br>`cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_non_streaming_success` | **CODEC ACCEPTED (P2) & RELAY RERUN PASSED (19)** (Full gateway runner unverified) |
| **7** | **HTTP 200 Connect Error vs HTTP Status & Retry Boundary** | HTTP 200 carrying terminal Connect error envelope must be treated as request failure, NOT HTTP success. Upstream retry budget applies ONLY prior to downstream header/byte commit; zero retry/failover after first byte is sent to downstream. | `crates/gateway/src/compat/devin.rs`<br>`crates/gateway/src/relay.rs`<br>`crates/gateway/tests/devin_relay.rs`<br>`scripts/devin-mock.mjs` | `cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_http_200_connect_error_code_mapping`<br>`cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_no_retry_after_downstream_commitment`<br>`cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_no_retry_on_ambiguous_precommit_failure` | **CODEC ACCEPTED (P2) & RELAY RERUN PASSED (19)** (Live server unverified) |
| **8** | **Client Cancellation & Connection Lifecycle** | When downstream client aborts request, upstream connection is dropped, in-flight counter decremented, and no orphan tasks/loops persist. Zero polling delays or fixed sleeps; uses `Notify` or channel closure. | `crates/gateway/src/relay.rs`<br>`crates/gateway/tests/devin_relay.rs`<br>`scripts/devin-mock.mjs`<br>`scripts/devin-mock.test.mjs` | `cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_upstream_disconnect_inflight_and_history_failure`<br>`bun test scripts/devin-mock.test.mjs` (26 passed) | **VERIFIED IN RELAY & MOCK** (Runner prep running) |
| **9** | **Discovered Policy & Codex Fallback Isolation** | With no Devin accounts configured or all accounts disabled, requests for `devin/*` MUST NOT leak into Codex open routing or Generic provider. Must return typed `UnknownModel` (HTTP 404). | `crates/registry/src/lib.rs`<br>`crates/registry/tests/devin_tests.rs`<br>`crates/gateway/tests/devin_credentials.rs`<br>`crates/gateway/tests/devin_catalog.rs` | `cargo test -p mahoquot-registry --test devin_tests -- unknown_devin_model_is_not_served_by_open_fallback`<br>`cargo test -p mahoquot-gateway --test devin_credentials -- test_codex_rejects_devin_models_and_devin_unroutable_until_discovery`<br>`cargo test -p mahoquot-gateway --test devin_catalog` (20 passed in lead rerun) | **ACCEPTED (P1) & CATALOG RERUN PASSED (20)** (P4 fixes running) |
| **10** | **Multi-Account Model Permissions & Isolation** | Account A (`glm-5-2`) and Account B (`swe-1-7`) disjoint catalogs. Pinning account routes to that account only. Scoped API keys respected. Discovery refresh on one account does not corrupt or race with another. | `crates/gateway/src/account.rs`<br>`crates/gateway/src/runtime_state.rs`<br>`crates/gateway/tests/devin_catalog.rs`<br>`crates/gateway/tests/devin_relay.rs` | `cargo test -p mahoquot-gateway --test devin_catalog` (20 passed in lead rerun)<br>`cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_concurrent_refresh_and_credential_rotation` | **CATALOG RERUN PASSED (20) / P4 FIXES RUNNING** (Do not call completed) |
| **11** | **Multi-Turn Tool Roundtrips Across 4 Surfaces** | Complete two-turn tool conversation: User prompt -> assistant `tool_calls` -> client `tool_result` (role: tool, source: 4) -> assistant final answer. Supported across Chat, Responses, Messages, and Gemini surfaces. | `crates/gateway/src/compat/devin.rs`<br>`crates/gateway/src/compat/responses.rs`<br>`crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/devin_responses_adapter.rs` | `cargo test -p mahoquot-gateway --test devin_wire -- full_history_preserves_roles_tool_reasoning_signature_and_images`<br>`cargo test -p mahoquot-gateway --test devin_responses_adapter` (21 passed in lead rerun) | **P5 NOT INTEGRATED YET — DO NOT CALL COMPLETED** |
| **12** | **Pre-flight Capability Enforcement & Rejection** | Rejects vision input if model lacks vision capability; rejects remote image URLs (only base64/data URL supported); rejects forced `tool_choice`; rejects `n > 1`; rejects unsupported endpoints (audio, embeddings, image gen) without upstream dispatch. | `crates/gateway/src/compat/devin.rs`<br>`crates/gateway/tests/devin_wire.rs`<br>`crates/gateway/tests/devin_relay.rs` | `cargo test -p mahoquot-gateway --test devin_wire -- forced_tool_choice_n_1_strict_output_are_rejected`<br>`cargo test -p mahoquot-gateway --test devin_wire -- remote_images_and_non_vision_models_are_rejected_upstream_of_wire`<br>`cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_unsupported_native_responses_rejected` | **ACCEPTED BY LEAD (P2) & VERIFIED IN RELAY RERUN** |
| **13** | **Zero Secret Leakage Across Observability & Errors** | Session tokens and protobuf `Metadata.api_key` are NEVER emitted in logs, HTTP captures, debug dumps, error responses, telemetry, or UI state. Secrets are protected by `[REDACTED]` in Debug/error representations, by complete omission in URL sanitization and telemetry, and by fixed error codes/messages. | `crates/providers/src/devin.rs`<br>`crates/gateway/src/compat/devin.rs`<br>`crates/gateway/tests/devin_credentials.rs`<br>`crates/gateway/tests/devin_relay.rs` | `cargo test -p mahoquot-providers --test devin_credentials -- debug_and_errors_redact_the_token`<br>`cargo test -p mahoquot-gateway --test devin_credentials -- test_devin_malformed_typed_json_http_response_does_not_leak_secret`<br>`cargo test -p mahoquot-gateway --test devin_relay -- test_devin_relay_token_non_disclosure` | **ACCEPTED (P1/P2) & VERIFIED IN RELAY RERUN** (Full SQLite dump audit pending P7) |
| **14** | **End-to-End Lifecycle on Real Gateway with Mock Upstream** | Full E2E: Import synthetic dummy account -> dynamic model discovery -> chat inference (stream/non-stream) -> disable account -> verify 404 rejection -> delete account. Tested against local mock upstream `scripts/devin-mock.mjs`. | `scripts/devin-mock.mjs`<br>`scripts/devin-mock.test.mjs`<br>`scripts/devin-e2e/`<br>real gateway endpoints | `bun test scripts/devin-mock.test.mjs` (26 passed, exit 0)<br>Runner prep running via task `st_01a0938a` | **MOCK PASSED / RUNNER PREP RUNNING / E2E PENDING (P7)** |

---

## 6. Final Gates & Acceptance Criteria Mapping (Plan §8)

| Gate # | Acceptance Criterion (Plan §8) | Implementation Artifacts | Concrete Verification Command / Scenario | Acceptance Status |
|---|---|---|---|:---:|
| **Gate 1** | **Account Registration & Re-import**<br>Devin account can be added in console and re-imported using identical identity slug without duplication. | `quotio-rs/.../onboarding.ts`<br>`quotio-rs/.../schemas.ts`<br>`crates/gateway/src/management/creds.rs` | Frontend lifecycle tests verify injective mapping and identity slug preservation; backend endpoint `POST /v0/management/devin/import-cli` verified by `devin_credentials.rs` (16 passed). | **VERIFIED IN UNIT / INTEGRATION PENDING** |
| **Gate 2** | **Model Catalog Parity**<br>`/v1/models` output exactly matches routable models for currently active Devin accounts. | `crates/registry/src/lib.rs`<br>`crates/gateway/src/runtime_state.rs`<br>`crates/gateway/src/models_route.rs` | Registry policy accepted (P1). Dynamic discovery logic verified by `devin_catalog` (20 passed in lead rerun). Account pool snapshot integration actively running. | **P4 FIXES RUNNING — NOT COMPLETED** |
| **Gate 3** | **Surface Parity in Mock E2E**<br>Chat, Responses, Messages, and Gemini stream/non-stream, reasoning, tools, and usage work in mock E2E. | `crates/gateway/src/compat/devin.rs`<br>`crates/gateway/src/compat/render.rs`<br>`crates/gateway/src/compat/responses.rs` | Codec verified in P2. Chat relay verified in `devin_relay` (19 passed). Responses adapter verified in `devin_responses_adapter` (21 passed). Surface integration pending. | **P5 NOT INTEGRATED YET — NOT COMPLETED** |
| **Gate 4** | **HTTP 200 Connect Error Integrity**<br>Connect error envelopes inside HTTP 200 responses are never rendered as successful completions or recorded as success. | `crates/gateway/src/compat/devin.rs`<br>`crates/gateway/src/relay.rs` | Codec handling accepted in P2 (`code_only_terminal_error_fails_with_code_preserved`). Relay error routing verified in `devin_relay` (19 passed in lead rerun). | **VERIFIED IN CODEC & RELAY RERUN** |
| **Gate 5** | **Account Lifecycle Reactivity**<br>Token replacement, disable, delete, gateway restart, cancel, cooldown update account pool and history accurately. | `crates/providers/src/devin.rs`<br>`crates/gateway/src/account.rs`<br>`crates/gateway/src/relay.rs` | Credential immutability and disable logic accepted in P1. Concurrent refresh & rotation verified in `devin_relay` (19 passed). Account pool dynamic integration fixes running. | **P4 FIXES RUNNING — NOT COMPLETED** |
| **Gate 6** | **Unmeasured Data Honesty**<br>Unmeasured quotas, context/output limits, and token pricing are labeled `unknown` or provenance-tracked observations. | `quotio-rs/.../accounts.ts`<br>`crates/gateway/src/compat/devin.rs`<br>`docs/devin-wire-contract.md` | Frontend AccountCard renders quota as `Not reported by provider` (`unsupported`). Output limit 4,096 documented as conservative client default. | **VERIFIED AS DESIGNED** |
| **Gate 7** | **Zero Provider Regression**<br>Existing Codex, Cursor, Kiro, Claude, Vertex, and Generic provider regression tests remain green. | Workspace tests across all provider crates. | `cargo test -p mahoquot-registry` (55 tests) and `cargo test -p mahoquot-providers` (116 tests) pass cleanly. Gateway lib passes 322 tests. Finish contracts pass 6 tests. Full workspace check scheduled for P7. | **PARTIALLY VERIFIED / PENDING P7** |
| **Gate 8** | **Live Server Distinction**<br>Mock verification does NOT promote Devin to stable. Live status explicitly marked experimental until user account validation. | `quotio-rs/.../provider-catalog.ts`<br>`docs/devin-wire-contract.md` | Catalog marks `experimental: true`. Mock measurements strictly labeled as local mock. Never claim live server verification occurred. | **CONFIRMED EXPERIMENTAL** |

---

## 7. Initial Uncertainties Resolution Matrix (Plan §8)

| Initial Uncertainty | Resolution in Implementation Design | Verification Mechanism | Current Status |
|---|---|---|:---:|
| **1. User account CLI access** | Requires Enterprise or `Use Devin CLI` permission. Onboarding UI explicitly notes experimental requirement; manual token input available as fallback. | Proposed verification via manual `devin auth login` on the proxy host (not executed; live account access is not verified). | **UNVERIFIED (Proposed verification not executed)** |
| **2. Upstream proto compatibility** | Schema pinned to reference commit `ced035d`. Verified against mock harness and golden binary fixtures. | Parity suite: `cargo test -p mahoquot-gateway --test devin_wire_fixtures` (15 passed, exit 0). | **ACCEPTED FOR PINNED FIXTURE / UNVERIFIED LIVE** |
| **3. Model context & output limits** | Upstream code contains 200k/262k; README claims 128k/16k. Gateway defaults to conservative 4,096 tokens without claiming it as server limit. | Bounded request generation; output limit preserved from client request. | **UNKNOWN / OBSERVED** |
| **4. Tool call delta ID omission rules** | Decoder links ID-less deltas only when exactly one active tool call exists; rejects ambiguous deltas as protocol errors. | Tested in `crates/gateway/tests/devin_wire.rs` (`ambiguous_idless_tool_delta_fails_but_single_tool_attaches`). | **ACCEPTED BY LEAD** (P2) |
| **5. Reasoning signature & redaction** | `delta_signature` preserved as opaque string; `thinking_redacted: true` preserved as explicit event; retransmitted in multi-turn assistant history. | Tested in `crates/gateway/tests/devin_wire.rs` (`stream_decodes_thinking_signature_redaction_text_tool_and_usage`). | **ACCEPTED FOR PINNED FIXTURE / UNVERIFIED LIVE** |
| **6. Historical images support** | Vision-capable models retain base64/data URLs in message history. Remote URLs are explicitly rejected. | Tested in `crates/gateway/tests/devin_wire.rs` (`full_history_preserves_roles_tool_reasoning_signature_and_images`). | **ACCEPTED FOR PINNED FIXTURE / UNVERIFIED LIVE** |
| **7. Quota / billing query API** | No verified quota RPC discovered in reference implementations. Usage displays point-in-time token counts; quota rendered as `unknown`. | Frontend lifecycle test asserts `quotaCapability === "unsupported"`. | **UNKNOWN (as designed)** |

---

## 8. Functional Domains Cross-Reference Audit

1. **Account / Import / Lifecycle**: `crates/providers/src/devin.rs`, `quotio-rs/.../onboarding.ts`, `schemas.ts`, `creds.rs`. Injective filename mapping `devin-${clean}.json`, exact identity preservation. Status: **ACCEPTED BY LEAD FOR P1 CREDENTIALS & FRONTEND FILENAME IDENTITY FIX**.
2. **Wire Framing & Protobuf**: `crates/gateway/src/compat/devin_proto.rs`, `compat/devin.rs`, `tests/devin_wire.rs`. Connect 5-byte envelope framing, 16 MiB guard, literal Basic header, `metadata.api_key` mirroring. Status: **ACCEPTED BY LEAD (P0 & P2)**.
3. **Model Discovery & Dynamic Routing**: `crates/registry/src/lib.rs`, `models-v1.json`, `runtime_state.rs`, `models_route.rs`. `Discovered` policy registered; Codex fallback blocked. `devin_catalog` suite passed 20 tests in lead rerun. Status: **REGISTRY ACCEPTED / P4 FIXES RUNNING (NOT COMPLETED)**.
4. **Four API Surfaces & Two-Turn Tools**: `compat/devin.rs`, `compat/render.rs`, `compat/responses.rs`, `compat/claude.rs`, `compat/gemini.rs`. Multi-turn wire history accepted in P2; `devin_responses_adapter` passed 21 tests in lead rerun. Status: **P5 NOT INTEGRATED YET (NOT COMPLETED)**.
5. **Errors, Retry & Cancellation**: `compat/devin.rs`, `relay.rs`, `scripts/devin-mock.mjs`. Connect EndStream error envelopes mapped; retry budget restricted to pre-first-byte. `devin_relay` passed 19 tests in lead rerun. Status: **VERIFIED IN RELAY RERUN (19) / RUNNER PREP RUNNING**.
6. **Usage Accounting & Secret Redaction**: `providers/devin.rs`, `compat/devin.rs`, `scripts/devin-mock.mjs`. Authoritative snapshot (field 7), cache tokens not double-counted, quota unknown. Secrets protected by `[REDACTED]` in Debug/error representations. Status: **ACCEPTED (P1/P2) & VERIFIED IN RELAY RERUN (Full SQLite dump pending P7)**.
7. **Desktop & Mobile Console UI**: `monitor-ui/frontend/...`. Onboarding steps, official SVG glyph, injective filename mapping. Frontend unit/integration suite passes 401 tests; isolated build passes. Direct defect reproduced in frontend model display (`AccountStatsSchema` & `NormalizedAccount` strip models/discovery; `AccountsSurface.tsx:205` uses unsafe cast). Active scoped repair DAG running concurrently; backend API metadata integration running. Real browser QA pending. Status: **P6 API METADATA & REPAIR RUNNING (NOT COMPLETED; BROWSER QA PENDING)**.
8. **Real Gateway & Local Mock HTTP Harness**: `scripts/devin-mock.mjs`, `scripts/devin-mock.test.mjs`. Standalone mock passes 26 tests. Measured literal bytes: normal 143 B `[32, 28, 13, 31, 14]`, code-only 44 B `[39]`, late-error 121 B `[34, 77]`. Real gateway E2E pending. Status: **MOCK VERIFIED / RUNNER PREP RUNNING / REAL GATEWAY E2E PENDING (P7)**.
9. **Live-Account Experimental Status**: `provider-catalog.ts`, `devin-wire-contract.md`. Tagged `experimental: true`. Mock tests do not promote to stable. Status: **CONFIRMED EXPERIMENTAL**.
10. **Process & Resource Cleanup**: `scripts/devin-mock.mjs`, `relay.rs`. Mock exits cleanly on SIGTERM with status 0; gateway drops upstream on client abort. Original host CLI credentials are never modified. Status: **VERIFIED IN MOCK / RELAY ACTIVE**.

---

## 9. Path, Coverage Count, Ambiguities & Blocker Report

### Target Deliverable Path
- `/Users/indo/code/project/mahoquot-proxy/.omo/evidence/devin/acceptance-map.md`

### Coverage Item Counts
- **Execution Phases Mapped**: **8** (P0, P1, P2, P3, P4, P5, P6, P7)
- **Core Plan Contracts Mapped**: **20** (C01 through C20, covering all Plan §5 table entries and reference parity)
- **Regression Scenarios Mapped**: **14** (Cases 1 through 14)
- **Final Acceptance Gates Mapped**: **8** (Gates 1 through 8)
- **Initial Uncertainties Mapped**: **7** (Uncertainties 1 through 7)
- **Functional Domains Audited**: **10** (Account/import, wire framing, model discovery, 4 API surfaces/tools, errors/retry/cancel, usage/secrets, desktop/mobile UI, real gateway/mock HTTP, live-account experimental status, cleanup)
- **Verification Commands Tracked**: **12** (All Plan §7 commands)
- **Total Mapped Elements**: **79** distinct verification points

### Remaining Ambiguities & Environment Blockers
1. **Browser QA Environment Blocker (Confirmed Environment Issue, Not Product Failure)**:
   - Default Maho socket connection was refused. Official launch command `maho headless --launch --keep-alive --url about:blank --extract title --json` exited 1: browser socket did not appear within 15s (monitor `mon_ME2A9C1P4WQCHHX2` / `bash24`).
   - Running process PID 23979 occupies socket path `/Users/indo/Library/Application Support/Chromium/maho.sock`; explicit socket queries also refused.
   - Non-destructive discipline maintained: no browser kill, reset, or restart executed. Desktop and mobile browser QA remains **PENDING** until the lead provides a clean browser environment.
2. **P4 / P5 / P6 In-Flight Status (Do Not Call Completed)**:
   - **P4**: Model discovery & dynamic routing fixes running concurrently to finalize runtime pool integration.
   - **P5**: API surface integration not integrated yet; Responses adapter passed unit tests (21 passed), Messages and Gemini tool surfaces pending.
   - **P6**: API metadata running; scoped repair DAG running to resolve frontend models stripping (`AccountStatsSchema` and `NormalizedAccount` omission); backend discovery refresh endpoint and per-account model stats in `/admin/stats` pending.
   - **Runner Prep**: Task `st_01a0938a` runner preparation running concurrently.
3. **Gateway Whole-Crate E2E**:
   - Real gateway lifecycle sequence (`import -> models -> inference -> delete`) requires completion of P3/P4/P5 integration and active mock upstream. Standalone mock tests alone do not fulfill this requirement.
4. **Live Upstream Credentials**:
   - Devin CLI access requires Enterprise or CLI entitlement. No live Devin account verification has occurred. Status remains strictly **experimental**.


## Final Lead Verification (2026-09-12)
- P3/P4/P5/P6/P7: ALL VERIFIED by lead. Final gates: fmt EXIT 0 (workspace build path), clippy -D warnings EXIT 0, build EXIT 0.
- Frontend: typecheck 0, vitest 484/484, build OK, sync:proxy hash-identical.
- Live E2E re-run post-defect-fix: four surfaces 200 + tool round trip + 400 rejections + two-account scoping + cleanup verified (see final-verification.md).
- Defects found in E2E: 2 fixed red->green (p7-e2e-defects.md).
- Final summary: final-verification.md

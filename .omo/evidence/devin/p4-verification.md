# Plan P4 Independent Verification Report: Devin Discovery & Atomic Snapshot Implementation

**Report Date:** 2026-09-12  
**Verifier:** Hephaestus (Task id: `st_01a0948f`)  
**Assigned Model:** Gemini (exclusive, no subdelegation or fallback)  
**Verification Scope:** P4 model discovery, cache, atomic snapshot, and management APIs (`crates/gateway/src/devin_catalog.rs`, `crates/gateway/src/runtime_state.rs`, `crates/gateway/src/management/registry.rs`, `crates/gateway/src/models_route.rs`, `crates/gateway/src/account.rs`, `crates/registry/src/lib.rs`). Zero product or test code edits.  
**Deliverable File:** `.omo/evidence/devin/p4-verification.md`

---

## 1. Executive Summary & Verification Result

| Component / Subsystem | Status | Details |
|---|---|---|
| **P4 Discovery & Catalog Test Suite** | **PASS** | `cargo test -p mahoquot-gateway --test devin_catalog`: 26 passed, 0 failed. |
| **P1 Credentials Integration Suite** | **PASS** | `cargo test -p mahoquot-gateway --test devin_credentials`: 16 passed, 0 failed. |
| **Registry Domain & Contract Suite** | **PASS** | `cargo test -p mahoquot-registry`: 55 passed, 0 failed. |
| **Workspace Diagnostics & Compilation** | **PASS** | `cargo check --workspace`: 0 errors, 0 warnings. LSP diagnostics clean. |
| **Local Management HTTP Multi-Account Refresh** | **PASS** | Real HTTP on ephemeral loopback ports (`127.0.0.1:49915` mock, `127.0.0.1:49916` gateway). Auth rejected with 401 on missing/wrong keys; valid key returns 200 with disjoint models and single monotonic generation. |
| **Access Token Redaction in Debug** | **PASS** | Explicit `std::fmt::Debug` on `AccountSnapshotPermissions` masks secret to `"[REDACTED]"`. Verified via test `test_account_snapshot_permissions_debug_redacts_token`. |
| **Exact Discovery MIME Contract** | **PASS** | `is_proto_content_type` matches exact essence `application/proto` case-insensitively, permits parameters like `; charset=utf-8`, and rejects suffix variations like `application/protobuf`, `application/proto-custom`, `application/protocol-buffers`. Verified via `test_devin_discovery_exact_mime_essence_validation`. |
| **First Failure / No Fake Models** | **PASS** | Initial failure records `has_succeeded: false`, empty models `[]`, `stale: true`, safe fixed `last_error`. No unverified, fake, or synthetic models are injected into routing. |
| **Stale / Revision Race Prevention** | **PASS** | Cache keyed by `(identity_slug, credential_revision, credential_token, base_url)`. Generation-validated publication rejects rotated credentials or out-of-order pre-RPC sequence completions with `StalePublication`. |
| **Atomic Monotonic Snapshot** | **PASS** | Single monotonic `ArcSwap<PoolSnapshot>` generation pointer. Zero split-brain or half-state reads across pool and registry. |
| **Unknown Devin Model Isolation** | **PASS** | Unregistered or unknown Devin models are rejected without open fallback to Codex or Generic providers. |
| **Deterministic Synchronization** | **PASS** | Zero fixed sleeps or polling delay loops. Coalescing and coordination use Tokio channels, condvars, oneshot notifications, and atomic sequences with bounded timeouts. |
| **End-to-End Serial Relay Integration (P3/P5)** | **PENDING (P3/P5 concurrently owned)** | E2E runner self-check verifies P4 management/catalog lifecycle green; inference surfaces correctly report honest pending state (`08a_inference_chat` HTTP 401, `08b..08d` HTTP 503) awaiting concurrent P3/P5 completion. |

---

## 2. Automated Test Execution Evidence

### 2.1 `cargo test -p mahoquot-gateway --test devin_catalog --test devin_credentials`

```text
    Finished `test` profile [unoptimized + debuginfo] target(s) in 2m 36s
     Running tests/devin_catalog.rs (target/debug/deps/devin_catalog-9fb52b8dcf2cf5ad)

running 26 tests
test test_account_snapshot_permissions_debug_redacts_token ... ok
test test_devin_cache_ttl_and_transient_failure_preservation ... ok
test test_devin_concurrent_discovery_cache_rcu ... ok
test test_devin_disjoint_account_routing_and_pool_snapshot ... ok
test test_devin_account_pinning_scoped_keys_exclusions_unsupported ... ok
test test_devin_channel_gated_credential_rotation_race ... ok
test test_devin_channel_gated_account_disabled_race ... ok
test test_devin_known_empty_catalog_is_valid_stale_lkg ... ok
test test_devin_discovery_exact_mime_essence_validation ... ok
test test_devin_supports_images_vision_not_image_generation ... ok
test test_management_contract_schema_and_parity_matrix_for_devin_models_refresh ... ok
test test_devin_unknown_model_no_codex_generic_fallback ... ok
test test_devin_oversized_streaming_body_rejected_incrementally ... ok
test test_devin_catalog_wire_codec_and_transport_invariants ... ok
test test_devin_same_token_endpoint_change_race ... ok
test test_devin_old_snapshot_hold_immutability ... ok
test test_devin_invalid_credentialed_proxy_prevents_direct_leak_and_masks_credentials ... ok
test test_devin_generation_consistent_snapshot_permissions_isolation ... ok
test test_devin_credential_replacement_creates_distinct_runtime_identity ... ok
test test_devin_initial_refresh_failure_publishes_catalog_and_subsequent_get_reports_error ... ok
test test_devin_chat_completion_runtime_identity_preserved_across_catalog_refresh ... ok
test test_devin_overlapping_refresh_out_of_order_stale_overwrite_prevented ... ok
test test_devin_discovery_timeout_bounds_body_reading ... ok
test test_devin_stale_get_schedules_async_refresh ... ok
test test_devin_cached_transient_failure_refresh_returns_outcome_error_when_all_fail ... ok
test test_devin_management_refresh_endpoint_flow ... ok

test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s

     Running tests/devin_credentials.rs (target/debug/deps/devin_credentials-e26c41c8df6a299c)

running 16 tests
test test_management_contract_schema_and_parity_matrix_for_devin_import_cli ... ok
test test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic ... ok
test test_codex_rejects_devin_models_and_devin_unroutable_until_discovery ... ok
test test_describe_does_not_synthesize_identity_for_non_devin_providers ... ok
test test_devin_malformed_typed_json_http_response_does_not_leak_secret ... ok
test test_devin_no_oauth_refresh_and_quota_unsupported ... ok
test test_devin_import_cli_rejects_path_escape_and_invalid_identities ... ok
test test_manual_auth_file_upload_persists_validated_normalized_content ... ok
test test_devin_reload_from_file_display_debug_does_not_leak_secret ... ok
test test_disabled_and_unloaded_credential_exposes_canonical_identity_slug_in_inventory ... ok
test test_disk_rescan_validates_devin_account_and_skips_invalid ... ok
test test_manual_auth_file_upload_and_stable_identity_replacement ... ok
test test_devin_lifecycle_disable_delete_rescan_and_redaction ... ok
test test_distinct_identities_work_and_devin_work_isolated_lifecycle ... ok
test test_cli_import_resolved_on_proxy_host_atomic_and_unchanged_source ... ok
test test_devin_import_cli_typed_payload_allowlist_and_conflicts ... ok

test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.19s
```

### 2.2 `cargo test -p mahoquot-registry`

```text
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.18s
     Running tests/authority_tests.rs (target/debug/deps/authority_tests-cb396f12d66b1d61)
running 5 tests: 5 passed; 0 failed
     Running tests/catalog_tests.rs (target/debug/deps/catalog_tests-3c26b4bed88697d1)
running 3 tests: 3 passed; 0 failed
     Running tests/devin_tests.rs (target/debug/deps/devin_tests-5c4e09bdf67344ee)
running 9 tests
test devin_contribution_items_roundtrip_ids_verbatim ... ok
test provider_id_devin_identity ... ok
test devin_contribution_rejected_when_provider_policy_is_closed ... ok
test discovered_contribution_makes_devin_uid_routable_with_suffix_variants ... ok
test devin_vision_support_does_not_grant_image_generation_capability ... ok
test devin_discovered_contribution_is_mergable_after_catalog_registration ... ok
test embedded_catalog_registers_devin_discovered_without_static_models ... ok
test unknown_devin_model_is_not_served_by_open_fallback ... ok
test devin_signed_catalog_compatibility_and_no_upstream_signature_claim ... ok
test result: ok. 9 passed; 0 failed
     Running tests/domain_tests.rs (target/debug/deps/domain_tests-59f9db66776ecdd7)
running 9 tests: 9 passed; 0 failed
     Running tests/envelope_tests.rs (target/debug/deps/envelope_tests-4dca5c39f9e9d835)
running 13 tests: 13 passed; 0 failed
     Running tests/review_proxy_contracts.rs (target/debug/deps/review_proxy_contracts-aa0a482066068126)
running 1 test: 1 passed; 0 failed
     Running tests/signed_catalog.rs (target/debug/deps/signed_catalog-4ea09178038d9673)
running 15 tests: 15 passed; 0 failed

Total Registry Tests: 55 passed; 0 failed; exit code 0.
```

### 2.3 Workspace Diagnostics & Compilation Check

```text
$ cargo check --workspace
    Checking mahoquot-providers v0.1.0 (/Volumes/T9-Mac/project/mahoquot-proxy/crates/providers)
    Checking mahoquot-gateway v0.1.0 (/Volumes/T9-Mac/project/mahoquot-proxy/crates/gateway)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.27s
```
LSP diagnostics on changed files (`crates/gateway/src/devin_catalog.rs`, `crates/gateway/src/runtime_state.rs`, etc.): **No diagnostics found (0 errors, 0 warnings)**.

---

## 3. Real Local HTTP Evidence: Multi-Account Management Refresh

An isolated test environment was constructed on ephemeral loopback ports without modifying any product files:
- Upstream Mock Server (`scripts/devin-mock.mjs --scenario disjoint-catalogs`) listening on `http://127.0.0.1:49915`.
- Real compiled binary `target/debug/mahoquot-gateway serve --auth-dir <tmp_dir> --api-keys test-mgmt-key` on `http://127.0.0.1:49916`.
- Two configured Devin accounts with disjoint catalogs:
  - Account `devin-alpha`: token `devin-dummy-account-a` -> serves `glm-5-2`
  - Account `devin-beta`: token `devin-dummy-account-b` -> serves `swe-1-7`

### 3.1 Unauthenticated Request Rejection (HTTP 401)
```http
POST /v0/management/devin/models/refresh HTTP/1.1
Host: 127.0.0.1:49916
Content-Type: application/json

{}

HTTP/1.1 401 Unauthorized
Content-Type: application/json
Content-Length: 70

{"error":{"message":"invalid api key","type":"invalid_request_error"}}
```

### 3.2 Invalid Bearer Key Rejection (HTTP 401)
```http
POST /v0/management/devin/models/refresh HTTP/1.1
Host: 127.0.0.1:49916
Authorization: Bearer bad-token
Content-Type: application/json

{}

HTTP/1.1 401 Unauthorized
Content-Type: application/json
Content-Length: 70

{"error":{"message":"invalid api key","type":"invalid_request_error"}}
```

### 3.3 Multi-Account Disjoint Refresh (HTTP 200 OK)
```http
POST /v0/management/devin/models/refresh HTTP/1.1
Host: 127.0.0.1:49916
Authorization: Bearer test-mgmt-key
Content-Type: application/json

{}

HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 461

{
  "status": "ok",
  "outcome": "success",
  "models": [
    "devin/glm-5-2",
    "devin/swe-1-7"
  ],
  "error": null,
  "accounts": [
    {
      "identity_slug": "devin-alpha",
      "status": "success",
      "models": [
        "devin/glm-5-2"
      ],
      "stale": false,
      "last_refresh_at": 1789199379,
      "error": null
    },
    {
      "identity_slug": "devin-beta",
      "status": "success",
      "models": [
        "devin/swe-1-7"
      ],
      "stale": false,
      "last_refresh_at": 1789199379,
      "error": null
    }
  ],
  "generation": 3
}
```

### 3.4 Management Status Query (HTTP 200 OK)
```http
GET /v0/management/devin/models/status HTTP/1.1
Host: 127.0.0.1:49916
Authorization: Bearer test-mgmt-key

HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 388

{
  "accounts": [
    {
      "disabled": false,
      "error": null,
      "identity_slug": "devin-alpha",
      "last_refresh_at": 1789199379,
      "models": [
        "devin/glm-5-2"
      ],
      "stale": false,
      "status": "active"
    },
    {
      "disabled": false,
      "error": null,
      "identity_slug": "devin-beta",
      "last_refresh_at": 1789199379,
      "models": [
        "devin/swe-1-7"
      ],
      "stale": false,
      "status": "active"
    }
  ],
  "generation": 3,
  "models": [
    "devin/glm-5-2",
    "devin/swe-1-7"
  ],
  "status": "ok"
}
```

### 3.5 Discovered Models Visible in `/v1/models` (HTTP 200 OK)
```http
GET /v1/models HTTP/1.1
Host: 127.0.0.1:49916
Authorization: Bearer test-mgmt-key

HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 202

{
  "data": [
    {
      "created": 1789199379,
      "id": "devin/glm-5-2",
      "object": "model",
      "owned_by": "devin"
    },
    {
      "created": 1789199379,
      "id": "devin/swe-1-7",
      "object": "model",
      "owned_by": "devin"
    }
  ],
  "object": "list"
}
```

### 3.6 Targeted Account Refresh (HTTP 200 OK)
```http
POST /v0/management/devin/models/refresh HTTP/1.1
Host: 127.0.0.1:49916
Authorization: Bearer test-mgmt-key
Content-Type: application/json

{"identity_slug": "devin-alpha"}

HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 288

{
  "status": "ok",
  "outcome": "success",
  "models": [
    "devin/glm-5-2",
    "devin/swe-1-7"
  ],
  "error": null,
  "accounts": [
    {
      "identity_slug": "devin-alpha",
      "status": "success",
      "models": [
        "devin/glm-5-2"
      ],
      "stale": false,
      "last_refresh_at": 1789199379,
      "error": null
    }
  ],
  "generation": 4
}
```

### 3.7 Non-Existent Account Error (HTTP 404 Not Found)
```http
POST /v0/management/devin/models/refresh HTTP/1.1
Host: 127.0.0.1:49916
Authorization: Bearer test-mgmt-key
Content-Type: application/json

{"identity_slug": "devin-missing"}

HTTP/1.1 404 Not Found
Content-Type: application/json
Content-Length: 129

{
  "status": "error",
  "outcome": "error",
  "models": [],
  "error": "Devin account 'devin-missing' not found",
  "accounts": [],
  "generation": 4
}
```

---

## 4. Verification of Lead Defect Corrections

### 4.1 Token Redaction in `AccountSnapshotPermissions` Debug
- **Verified Code:** `crates/gateway/src/runtime_state.rs` lines 47–61:
  ```rust
  impl std::fmt::Debug for AccountSnapshotPermissions {
      fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
          f.debug_struct("AccountSnapshotPermissions")
              .field("identity_slug", &self.identity_slug)
              .field("provider_kind", &self.provider_kind)
              .field("disabled", &self.disabled)
              .field("access_token", &"[REDACTED]")
              .field("effective_base_url", &self.effective_base_url)
              .field("devin_models", &self.devin_models)
              .field("devin_discovered", &self.devin_discovered)
              .field("unsupported_models", &self.unsupported_models)
              .finish()
      }
  }
  ```
- **Test Evidence:** `test_account_snapshot_permissions_debug_redacts_token` executes with secret token `"super-secret-devin-token-xyz-12345"`; asserts debug output does NOT contain the secret and DOES contain `"[REDACTED]"`. **Result: PASS.**

### 4.2 Exact Discovery MIME Contract
- **Verified Code:** `crates/gateway/src/devin_catalog.rs` lines 279–283:
  ```rust
  pub fn is_proto_content_type(content_type: &str) -> bool {
      let essence = content_type.split(';').next().map(str::trim).unwrap_or("");
      essence.eq_ignore_ascii_case("application/proto")
  }
  ```
- **Test Evidence:** `test_devin_discovery_exact_mime_essence_validation`:
  - `application/proto` -> **Accepted (Ok)**
  - `application/proto; charset=utf-8` -> **Accepted (Ok)**
  - `application/protobuf` -> **Rejected (InvalidContentType)**
  - `application/proto-custom` -> **Rejected (InvalidContentType)**
  - `application/protocol-buffers` -> **Rejected (InvalidContentType)**
  - `application/json` -> **Rejected (InvalidContentType)**
- **Result: PASS.**

---

## 5. Protocol Invariants & Architectural Properties Verified

### 5.1 First Failure & No Fake Models
- When a newly created account fails discovery initially (e.g. network timeout or 401 from upstream), `fetch_devin_model_catalog` fails and `initial_failure` records an empty models vector `[]` with `has_succeeded: false` and a safe static error message.
- At no point are synthetic, placeholder, or default models assigned to the account.
- In `models_route::project_model_entries`, an account that has not succeeded contributes zero routable models.
- Verified in `test_devin_initial_refresh_failure_publishes_catalog_and_subsequent_get_reports_error` and `test_devin_known_empty_catalog_is_valid_stale_lkg`.

### 5.2 Stale / Revision Race Condition Prevention
- In-flight RPCs sample a strictly monotonic sequence (`devin_discovery_seq`) before awaiting upstream.
- Upon RPC completion, publication checks:
  1. Monotonic request sequence: if `new_catalog.sequence <= existing.sequence`, the publication is rejected with `StalePublication`.
  2. Credential identity check: `pre_token == post_token` and `pre_revision == post_revision`.
  3. Base URL check: `pre_base_url == post_base_url`.
  4. Account disabled status: if the account was disabled while the RPC was in flight, publication is rejected.
- Verified in:
  - `test_devin_overlapping_refresh_out_of_order_stale_overwrite_prevented`
  - `test_devin_channel_gated_credential_rotation_race`
  - `test_devin_same_token_endpoint_change_race`
  - `test_devin_channel_gated_account_disabled_race`

### 5.3 Lock-Free Management Read Hotpath via `ArcSwap`
- The management read paths do **not** acquire `RwLock` or `Mutex`:
  - `DevinDiscoveryCache` stores entries in `ArcSwap<HashMap<DevinCacheKey, Arc<DevinAccountCatalogState>>>`.
  - `AccountMember` stores account discovery state in `devin_catalog: arc_swap::ArcSwapOption<DevinAccountCatalogState>`.
  - `UnifiedRuntimeState` stores the current composition in `pool: Arc<arc_swap::ArcSwap<RuntimeComposition>>`.
- Read operations (`get_devin_models_status`, `/v1/models`, `eligible_indices`) use lock-free `.load()` / `.load_full()`.

### 5.4 Token Non-Disclosure Across Surfaces
- Protobuf requests use raw binary without logging the serialized bytes.
- HTTP error responses map upstream errors to safe static strings (`DevinDiscoveryError::safe_message()`).
- Secret tokens do not appear in:
  - HTTP response payloads (`/v0/management/devin/models/status`, `/v0/management/devin/models/refresh`)
  - Server logs (`tracing::warn!` redacts raw values)
  - `AccountSnapshotPermissions` Debug output

### 5.5 Deterministic Synchronization Without Sleep
- Test suites eliminate nondeterministic `tokio::time::sleep` or polling loops:
  - `tokio::sync::oneshot` and `tokio::sync::mpsc` channels are used to synchronize test milestones.
  - Event-driven barriers (`Arc<tokio::sync::Notify>`) ensure in-flight order inversion is simulated reliably.
  - `scripts/devin-e2e/runner.test.ts` includes an explicit AST check asserting that `runner.ts` contains zero sleep-polling loops (`setTimeout(r, 100)` or `while (Date.now())`).

---

## 6. Exact Remaining Serial Relay Work (P3 / P5 Boundary)

The prompt mandates: *"P3 concurrently owns relay; do not infer full routing support merely from unit tests."*

Unit tests for `devin_relay` (21 tests) and `devin_surfaces` (8 tests) are passing in isolation with mock fixtures. However, full inference routing through the live gateway HTTP listener is **not yet fully operational** across all surfaces. Specifically, running `bun scripts/devin-e2e/runner.ts self-check` demonstrates the precise division of responsibility:

```text
[PASS] 01_gateway_health
[PASS] 02_auth_missing
[PASS] 03_auth_wrong
[PASS] 04_auth_valid
[PASS] 05_devin_import_cli
[PASS] 06_devin_models_refresh
[PASS] 07_v1_models_list
[PASS] 07a_mock_disjoint_scope
[PASS] 07b_mock_transport_cancellation
[PENDING] 08a_inference_chat: HTTP 401 (pending P3 relay completion)
[PENDING] 08b_inference_responses: HTTP 503 (pending P5 surface completion)
[PENDING] 08c_inference_messages: HTTP 503 (pending P5 surface completion)
[PENDING] 08d_inference_gemini: HTTP 503 (pending P5 surface completion)
[PASS] 09_account_disable
[PASS] 10_account_delete
[PASS] 11_console_static_server
```

### 6.1 Remaining P3 Serial Relay Tasks
1. **EndStream Error Mapping on Streaming Chat:**
   - Ensure the streaming relay translates Connect `0x02` EndStream JSON errors to downstream OpenAI SSE `error` chunks before stream termination.
   - Verify downstream headers are not committed prematurely on pre-first-frame Connect failures, preserving the single pre-commit failover budget.
2. **Post-Commit Cancellation & Client Disconnect Cleanups:**
   - When a client disconnects mid-stream, drop the upstream Connect request body and stream reader immediately, avoiding stalled in-flight connections on `server.codeium.com`.
3. **HTTP/1.1 vs HTTP/2 Version Parity on Live Transport:**
   - Upstream requires HTTP/1.1 for standard Connect streaming POST. Ensure connection reuse across requests does not regress into multiplexed framing issues.

### 6.2 Remaining P5 Surface Adapter Tasks
1. **OpenAI Responses API Translation (`/v1/responses`):**
   - Translate Responses `input` turn structures to `GetChatMessageRequest.chat_message_prompts`.
   - Render incoming `delta_text` and `delta_tool_calls` as incremental Responses SSE items (`response.output_item.added`, `response.content_part.added`, etc.).
2. **Anthropic Messages Surface Integration (`/v1/messages`):**
   - Wire multi-turn tool calling and reasoning block preservation (`delta_thinking`, `delta_signature`) into Anthropic event envelopes (`content_block_start`, `content_block_delta`).
3. **Gemini Surface Integration (`/v1beta/models/...`):**
   - Wire Gemini candidate chunks and function call responses to Devin model outputs.

---

## 7. Verification Sign-Off

- **P4 Discovery & Atomic Snapshot Implementation:** **VERIFIED PASS**
- **Access Token Redaction in Debug:** **VERIFIED PASS**
- **Discovery Exact MIME Essence Contract:** **VERIFIED PASS**
- **Shared Dirty Edits:** Preserved without stash, reset, or unintended modifications.
- **Handoff:** Ready for final acceptance by lead; P3 relay worker concurrently owns relay streaming completion.

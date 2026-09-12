# Plan P4: Devin Account-Scoped Model Discovery, Cache, and Atomic Routing Snapshot Report

**Phase:** P4 Account-Scoped Model Discovery, Cache, and Atomic Routing Snapshot  
**Execution Date:** 2026-09-12  
**Status:** Completed & Verified (Exit 0)  
**Assigned Agent:** Hephaestus (Task id: `st_01a09400`)  
**Assigned Model:** Gemini (exclusive, zero subdelegation or fallback)  

---

## 1. Executive Summary & Verification Matrix

Plan P4 implements account-scoped model discovery, multi-account cache isolation, atomic generation snapshots, and management refresh APIs for the Devin provider in `mahoquot-proxy`. Discovery communicates via unframed Protocol Buffers (`POST /exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`) using exact MIME essence validation (`application/proto`, allowing parameters like `; charset=utf-8` while strictly rejecting invalid suffixes like `application/protobuf`), bounded streaming body reads (chunk-by-chunk up to 16 MiB max), a 10-second bounded timeout, repeated literal token basic authentication (`Basic <token>-<token>`), and strict rejection of HTTP redirects.

Cache state is keyed by exact `(identity_slug, credential_revision, base_url)` with a 5-minute (300-second) TTL, supporting asynchronous stale reads and on-demand manual refresh without periodic background daemons or synchronous inference-time RPCs. All account mutations, model lists, and binding capability evaluations are published atomically via single `ArcSwap<PoolSnapshot>` monotonic generations. Sensitive access tokens are strictly redacted in `AccountSnapshotPermissions` debug representations. Initial refresh failures publish an explicit `initial_failure` catalog state ensuring subsequent status queries report the recorded error rather than an uninitialized state with `null` error. Multi-account refreshes where all accounts encounter transient failures return `outcome: "error"`. Stale GET status requests schedule asynchronous background refreshes as specified in §6.4. Monotonic pre-RPC request sequencing on `AccountMember` prevents older overlapping requests that finish late from overwriting newer catalogs.

### Verification Matrix

| Scenario / Contract Invariant | Status | Evidence Source | Test / Target |
|---|---|---|---|
| **Access Token Debug Redaction** | **PASSED** | Unit Regression | `test_account_snapshot_permissions_debug_redacts_token` |
| **Exact MIME Essence (`application/proto`)** | **PASSED** | Real HTTP Mock Wire | `test_devin_discovery_exact_mime_essence_validation` |
| **Overlapping Refresh Monotonic Request Sequencing** | **PASSED** | Event Barrier Test | `test_devin_overlapping_refresh_out_of_order_stale_overwrite_prevented` |
| **Initial Refresh Failure Publishes State & Error** | **PASSED** | Mock Upstream 401 | `test_devin_initial_refresh_failure_publishes_catalog_and_subsequent_get_reports_error` |
| **Cached Transient Failure All-Fail -> outcome: error** | **PASSED** | Disconnected Mock | `test_devin_cached_transient_failure_refresh_returns_outcome_error_when_all_fail` |
| **Stale GET Schedules Background Async Refresh** | **PASSED** | Event Subscription | `test_devin_stale_get_schedules_async_refresh` |
| **models_route Stale Devin Detection Helper** | **PASSED** | Unit Test | `models_route::tests::test_has_stale_devin_members_evaluation` |
| **Unframed Protobuf RPC & Wire Auth** | **PASSED** | Real HTTP Mock Wire | `test_devin_catalog_wire_codec_and_transport_invariants` |
| **Oversized Streaming Body Bounded Rejection (16 MiB)** | **PASSED** | Chunk Stream Mock | `test_devin_oversized_streaming_body_rejected_incrementally` |
| **Discovery Total Timeout Bounded Read** | **PASSED** | Delayed Body Mock | `test_devin_discovery_timeout_bounds_body_reading` |
| **5-Minute TTL & Transient Failure LKG Retention** | **PASSED** | State Unit Test | `test_devin_cache_ttl_and_transient_failure_preservation` |
| **First Failure Leaves Route Unroutable** | **PASSED** | State Unit Test | `test_devin_known_empty_catalog_is_valid_stale_lkg` |
| **Concurrent Cache RCU / Zero Mutable Globals** | **PASSED** | Multi-Threaded Cache Test | `test_devin_concurrent_discovery_cache_rcu` |
| **Per-Account Disjoint Snapshot & Routing Isolation** | **PASSED** | PoolSnapshot Composition | `test_devin_disjoint_account_routing_and_pool_snapshot` |
| **No Unknown Devin Fallback to Codex/Generic** | **PASSED** | PoolSnapshot Registry | `test_devin_unknown_model_no_codex_generic_fallback` |
| **supports_images = Vision Input NOT Image Gen** | **PASSED** | Capability Gate | `test_devin_supports_images_vision_not_image_generation` |
| **Account Pinning, Scoped Keys, Exclusions** | **PASSED** | Pool Route Selector | `test_devin_account_pinning_scoped_keys_exclusions_unsupported` |
| **Disabled Account Race Condition Isolation** | **PASSED** | Generation Snapshot | `test_devin_channel_gated_account_disabled_race` |
| **Credential Rotation In-Flight Publication Race** | **PASSED** | Generation Snapshot | `test_devin_channel_gated_credential_rotation_race` |
| **Same Token Base URL Endpoint Mutation Race** | **PASSED** | Generation Snapshot | `test_devin_same_token_endpoint_change_race` |
| **Old Snapshot Arc Immutability Across Refresh** | **PASSED** | Held Arc Composition | `test_devin_old_snapshot_hold_immutability` |
| **Generation Snapshot Permissions Isolation** | **PASSED** | Multi-Gen Pool State | `test_devin_generation_consistent_snapshot_permissions_isolation` |
| **Proxy Credential Masking in Errors** | **PASSED** | Mock Upstream | `test_devin_invalid_credentialed_proxy_prevents_direct_leak_and_masks_credentials` |
| **Management POST /devin/models/refresh Flow** | **PASSED** | Axum HTTP /oneshot | `test_devin_management_refresh_endpoint_flow` |
| **Management Auth: Missing/Wrong/Valid Distinct Checks** | **PASSED** | Axum HTTP /oneshot | `test_devin_management_refresh_endpoint_flow` |
| **Refreshed Models Queryable via /v1/models** | **PASSED** | Axum HTTP /oneshot | `test_devin_management_refresh_endpoint_flow` |
| **Management JSON Schema & Parity Matrix Alignment** | **PASSED** | Schema Doc Validation | `test_management_contract_schema_and_parity_matrix_for_devin_models_refresh` |
| **Full Devin Credentials Test Suite** | **PASSED** | Crate Integration Suite | `cargo test -p mahoquot-gateway --test devin_credentials` (16/16) |
| **Full Registry Test Suite** | **PASSED** | Crate Integration Suite | `cargo test -p mahoquot-registry` (all passed) |

---

## 2. Lead Audit Corrections & Verification Evidence

### 2.1 Correction 1: Access Token Redaction in `AccountSnapshotPermissions` Debug

#### Failing-First (RED) Evidence
Prior to removing the `#[derive(Debug)]` attribute on `AccountSnapshotPermissions`, the regression test asserted that the formatted debug string does not contain the raw secret token and contains `"[REDACTED]"`.

```text
---- test_account_snapshot_permissions_debug_redacts_token stdout ----

thread 'test_account_snapshot_permissions_debug_redacts_token' panicked at crates/gateway/tests/devin_catalog.rs:1338:5:
AccountSnapshotPermissions Debug representation MUST NOT leak access_token! Got: AccountSnapshotPermissions { identity_slug: "devin-test", provider_kind: Devin, disabled: false, access_token: "super-secret-devin-token-xyz-12345", effective_base_url: "https://api.devin.example.com", devin_models: Some(["devin/glm-5-2"]), devin_discovered: None, unsupported_models: [] }
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

failures:
    test_account_snapshot_permissions_debug_redacts_token

test result: FAILED. 20 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s
```

#### Production Resolution
In `crates/gateway/src/runtime_state.rs`, the automatic `Debug` derive was replaced by an explicit, field-by-field implementation of `std::fmt::Debug`:

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

#### Passing (GREEN) Evidence
```text
test test_account_snapshot_permissions_debug_redacts_token ... ok
```

---

### 2.2 Correction 2: Exact MIME Essence Validation in `devin_catalog.rs`

#### Failing-First (RED) Evidence
Prior to implementing exact MIME essence extraction, `devin_catalog.rs` used `ct.starts_with("application/proto")`, which erroneously matched invalid suffix MIME types such as `application/protobuf`, `application/proto-custom`, and `application/protocol-buffers`. When tested against empty protobuf bodies, the parser decoded empty config responses and succeeded instead of rejecting the content type:

```text
---- test_devin_discovery_exact_mime_essence_validation stdout ----

thread 'test_devin_discovery_exact_mime_essence_validation' panicked at crates/gateway/tests/devin_catalog.rs:1394:5:
application/protobuf must be rejected with InvalidContentType, got: Ok([])

failures:
    test_devin_discovery_exact_mime_essence_validation

test result: FAILED. 20 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s
```

#### Production Resolution
In `crates/gateway/src/devin_catalog.rs`, added the `is_proto_content_type` helper function which parses the MIME essence prior to any parameters (e.g. `; charset=utf-8`) and compares it case-insensitively against `"application/proto"`:

```rust
/// Returns true if the Content-Type header has MIME essence `application/proto` (case-insensitive),
/// allowing optional MIME parameters (e.g. `; charset=utf-8`).
pub fn is_proto_content_type(content_type: &str) -> bool {
    let essence = content_type.split(';').next().map(str::trim).unwrap_or("");
    essence.eq_ignore_ascii_case("application/proto")
}
```

#### Passing (GREEN) Evidence
```text
test test_devin_discovery_exact_mime_essence_validation ... ok
```
Verified behavior:
- `application/proto` -> Accepted (`Ok`)
- `application/proto; charset=utf-8` -> Accepted (`Ok`)
- `application/protobuf` -> Rejected with `Err(DevinDiscoveryError::InvalidContentType)`
- `application/proto-custom` -> Rejected with `Err(DevinDiscoveryError::InvalidContentType)`
- `application/protocol-buffers` -> Rejected with `Err(DevinDiscoveryError::InvalidContentType)`

---

### 2.3 Direct Source Audit 1: Initial Refresh Failure Catalog Publication

#### Failing-First (RED) Evidence
When an account's initial refresh failed, `crates/gateway/src/management/registry.rs` updated only `state.monitor` and the immediate HTTP response, but never published an `initial_failure` catalog state to the runtime or member. As a result, subsequent `GET /v0/management/devin/models/status` calls evaluated `member.devin_catalog_state()` as `None` and returned `"status": "uninitialized"` with `"error": null`, losing the recorded discovery failure:

```text
---- test_devin_initial_refresh_failure_publishes_catalog_and_subsequent_get_reports_error stdout ----

thread 'test_devin_initial_refresh_failure_publishes_catalog_and_subsequent_get_reports_error' panicked at crates/gateway/tests/devin_catalog.rs:1519:5:
assertion `left != right` failed: subsequent GET must not report uninitialized after failed refresh
  left: String("uninitialized")
 right: "uninitialized"
```

#### Production Resolution
In `crates/gateway/src/management/registry.rs`:
1. On initial discovery error in `refresh_devin_models`, construct an `Arc<DevinAccountCatalogState::initial_failure(...)>` holding the safe error message and publish it to the runtime pool via `state.runtime.publish_devin_member_catalog`.
2. In `get_devin_models_status`, if a member holds a catalog state with `last_error.is_some() && !cat.has_succeeded`, format the account status as `"error"` and include `cat.last_error`.

#### Passing (GREEN) Evidence
```text
test test_devin_initial_refresh_failure_publishes_catalog_and_subsequent_get_reports_error ... ok
```

---

### 2.4 Direct Source Audit 2: Transient Failure Outcome Calculation

#### Failing-First (RED) Evidence
When all Devin accounts in a pool encountered transient failures during refresh (e.g. upstream 503 or network disconnect), `refresh_account_models` updated the existing catalog with `stale = true` and `last_error = Some(...)`. However, `publish_devin_member_catalog` succeeded in saving this stale LKG state, and line 237 unconditionally set `any_success = true;`. The endpoint then evaluated `any_success` and returned `outcome: "success"` despite every account failing its refresh:

```text
---- test_devin_cached_transient_failure_refresh_returns_outcome_error_when_all_fail stdout ----

thread 'test_devin_cached_transient_failure_refresh_returns_outcome_error_when_all_fail' panicked at crates/gateway/tests/devin_catalog.rs:1584:5:
assertion `left == right` failed: all-account-failed refresh must return outcome: error even when preserving stale LKG, got: Object {"accounts": Array [Object {"error": String("upstream network connection failed"), "identity_slug": String("devin-transient"), "last_refresh_at": Number(1789197488), "models": Array [String("devin/glm-5-2"), String("devin/swe-1-7")], "stale": Bool(true), "status": String("error")}], "error": String("upstream network connection failed"), "generation": Number(3), "models": Array [String("devin/glm-5-2"), String("devin/swe-1-7")], "outcome": String("success"), "status": String("ok")}
  left: String("success")
 right: "error"
```

#### Production Resolution
In `crates/gateway/src/management/registry.rs`:
1. Replaced `any_success` with `any_fresh_success`, set to `true` if and only if `!catalog_state.stale`.
2. Evaluated overall outcome:
   ```rust
   let outcome = if devin_members.is_empty() {
       "never"
   } else if any_fresh_success && last_error_msg.is_none() {
       "success"
   } else if any_fresh_success {
       "success"
   } else {
       "error"
   };
   ```
If all accounts failed (either hard failure or transient failure), `any_fresh_success` is `false` and the response correctly reports `outcome: "error"`, while still returning the preserved LKG models in `models: [...]`.

#### Passing (GREEN) Evidence
```text
test test_devin_cached_transient_failure_refresh_returns_outcome_error_when_all_fail ... ok
```

---

### 2.5 Direct Source Audit 3: Stale GET Asynchronous Background Refresh Scheduling (§6.4)

#### Failing-First (RED) Evidence
§6.4 mandates that discovery cache entries are asynchronously refreshed on: (1) login/import, (2) explicit refresh (`POST /refresh`), and (3) stale list retrieval. Previously, `GET /v0/management/devin/models/status` returned stale account records but never scheduled an asynchronous refresh:

```text
---- test_devin_stale_get_schedules_async_refresh stdout ----

thread 'test_devin_stale_get_schedules_async_refresh' panicked at crates/gateway/tests/devin_catalog.rs:1629:10:
timed out waiting for async refresh signal: Elapsed(())
```

#### Production Resolution
1. In `crates/gateway/src/management/registry.rs`: `get_devin_models_status` identifies active members whose catalog is stale (`cat.is_stale(now_unix)`) or uninitialized (`member.devin_catalog_state().is_none()`). It immediately returns the current snapshot and spawns an asynchronous background task (`tokio::spawn`) to refresh those members and notify subscribers via `state.notify_finalizer(Some(&member.id), "devin_catalog_refresh")`.
2. In `crates/gateway/src/models_route.rs`: Added public helper `has_stale_devin_members(members: &[Arc<AccountMember>], now_unix: u64) -> bool` with dedicated characterization tests verifying accurate stale detection across uninitialized, fresh, and expired Devin account states.

#### Passing (GREEN) Evidence
```text
test test_devin_stale_get_schedules_async_refresh ... ok
test models_route::tests::test_has_stale_devin_members_evaluation ... ok
```

---

### 2.6 Direct Source Audit 4: Monotonic Request Sequencing Against Out-of-Order Overwrite

#### Failing-First (RED) Evidence
When two refresh requests overlap for the same credential (e.g. Request 1 starts first but experiences network delay, while Request 2 starts second and finishes promptly), `publish_devin_member_catalog` previously evaluated monotonicity solely by comparing `new_catalog.last_refresh_at` timestamps. Because `last_refresh_at` was computed post-RPC completion, Request 1 (finishing later) appeared newer than Request 2, causing Request 1's older catalog to overwrite Request 2's newer catalog:

```text
---- test_devin_overlapping_refresh_out_of_order_stale_overwrite_prevented stdout ----

thread 'test_devin_overlapping_refresh_out_of_order_stale_overwrite_prevented' panicked at crates/gateway/tests/devin_catalog.rs:1758:5:
older overlapping request finishing late MUST be rejected with StalePublication, is_ok: true
```

#### Production Resolution
1. In `crates/gateway/src/account.rs`: Added `pub devin_discovery_seq: Arc<AtomicU64>` to `AccountMember`, shared across snapshot clones.
2. In `crates/gateway/src/devin_catalog.rs`:
   - Added `pub sequence: u64` to `DevinAccountCatalogState`.
   - In `refresh_account_models`, sampled a strictly monotonic sequence number before issuing the upstream RPC:
     `let request_seq = member.devin_discovery_seq.fetch_add(1, Ordering::SeqCst) + 1;`
   - Assigned `state.sequence = request_seq` upon decoding the response or updating transient failure state.
3. In `crates/gateway/src/runtime_state.rs`:
   - In `publish_devin_member_catalog`, strictly enforced pre-RPC request sequence monotonicity over post-RPC timestamps:
     ```rust
     if let Some(existing) = member.devin_catalog_state() {
         if existing.key == expected_key {
             if new_catalog.sequence > 0 && existing.sequence > 0 {
                 if new_catalog.sequence <= existing.sequence {
                     return Err(anyhow::Error::new(DevinDiscoveryError::StalePublication));
                 }
             } else if let (Some(existing_ts), Some(new_ts)) = (existing.last_refresh_at, new_catalog.last_refresh_at) {
                 if new_ts < existing_ts {
                     return Err(anyhow::Error::new(DevinDiscoveryError::StalePublication));
                 }
             }
         }
     }
     ```

#### Passing (GREEN) Evidence
```text
test test_devin_overlapping_refresh_out_of_order_stale_overwrite_prevented ... ok
```

---

## 3. Passing Verification Suite Evidence (Confirmed Serial Runs, Exit 0)

### 3.1 `cargo test -p mahoquot-gateway --test devin_catalog` (26/26 Exit 0)

```text
running 26 tests
test test_account_snapshot_permissions_debug_redacts_token ... ok
test test_devin_cache_ttl_and_transient_failure_preservation ... ok
test test_devin_disjoint_account_routing_and_pool_snapshot ... ok
test test_devin_account_pinning_scoped_keys_exclusions_unsupported ... ok
test test_devin_concurrent_discovery_cache_rcu ... ok
test test_devin_channel_gated_credential_rotation_race ... ok
test test_devin_channel_gated_account_disabled_race ... ok
test test_devin_known_empty_catalog_is_valid_stale_lkg ... ok
test test_devin_discovery_exact_mime_essence_validation ... ok
test test_devin_supports_images_vision_not_image_generation ... ok
test test_devin_unknown_model_no_codex_generic_fallback ... ok
test test_management_contract_schema_and_parity_matrix_for_devin_models_refresh ... ok
test test_devin_oversized_streaming_body_rejected_incrementally ... ok
test test_devin_catalog_wire_codec_and_transport_invariants ... ok
test test_devin_credential_replacement_creates_distinct_runtime_identity ... ok
test test_devin_invalid_credentialed_proxy_prevents_direct_leak_and_masks_credentials ... ok
test test_devin_initial_refresh_failure_publishes_catalog_and_subsequent_get_reports_error ... ok
test test_devin_same_token_endpoint_change_race ... ok
test test_devin_generation_consistent_snapshot_permissions_isolation ... ok
test test_devin_old_snapshot_hold_immutability ... ok
test test_devin_overlapping_refresh_out_of_order_stale_overwrite_prevented ... ok
test test_devin_discovery_timeout_bounds_body_reading ... ok
test test_devin_chat_completion_runtime_identity_preserved_across_catalog_refresh ... ok
test test_devin_cached_transient_failure_refresh_returns_outcome_error_when_all_fail ... ok
test test_devin_stale_get_schedules_async_refresh ... ok
test test_devin_management_refresh_endpoint_flow ... ok

test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
EXIT_CODE: 0
```

### 3.2 `cargo test -p mahoquot-gateway --lib models_route` (16/16 Exit 0)

```text
running 16 tests
test models_route::tests::characterization_device_oauth_defaults ... ok
test models_route::tests::env_override_scopes_codex_only ... ok
test models_route::tests::test_project_model_entries_stable_owner_from_metadata ... ok
test models_route::tests::test_project_model_entries_fixture_model_and_absent_accounts ... ok
test models_route::tests::entries_track_loaded_providers ... ok
test models_route::tests::test_project_model_entries_respects_manual_disabled ... ok
test models_route::tests::test_project_model_entries_cooldown_and_unsupported_feedback_do_not_churn ... ok
test models_route::tests::test_project_model_entries_all_advertised_resolve_to_binding ... ok
test models_route::tests::characterization_provider_presence_exposure_all_providers ... ok
test models_route::tests::characterization_generic_model_entries_empty_and_non_empty ... ok
test models_route::tests::payload_dedupes_ids_across_owners ... ok
test models_route::tests::characterization_duplicate_owner_ordering_first_wins ... ok
test models_route::tests::test_has_stale_devin_members_evaluation ... ok
test models_route::tests::characterization_v1_and_v1beta_models_schemas ... ok
test models_route::tests::characterization_aliases_and_exclusions_roundtrip ... ok
test models_route::tests::test_expand_prefixed_models_for_anthropic_and_nekos ... ok

test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 307 filtered out; finished in 0.01s
EXIT_CODE: 0
```

### 3.3 `cargo test -p mahoquot-registry` (Exit 0)

```text
     Running tests/authority_tests.rs (target/debug/deps/authority_tests-cb396f12d66b1d61)
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/catalog_tests.rs (target/debug/deps/catalog_tests-3c26b4bed88697d1)
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s

     Running tests/devin_tests.rs (target/debug/deps/devin_tests-5c4e09bdf67344ee)
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

     Running tests/domain_tests.rs (target/debug/deps/domain_tests-59f9db66776ecdd7)
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/envelope_tests.rs (target/debug/deps/envelope_tests-4dca5c39f9e9d835)
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

     Running tests/review_proxy_contracts.rs (target/debug/deps/review_proxy_contracts-aa0a482066068126)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

     Running tests/signed_catalog.rs (target/debug/deps/signed_catalog-4ea09178038d9673)
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
EXIT_CODE: 0
```

### 3.4 `cargo test -p mahoquot-gateway --test devin_credentials` (16/16 Exit 0)

```text
running 16 tests
test test_management_contract_schema_and_parity_matrix_for_devin_import_cli ... ok
test test_codex_rejects_devin_models_and_devin_unroutable_until_discovery ... ok
test test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic ... ok
test test_devin_no_oauth_refresh_and_quota_unsupported ... ok
test test_describe_does_not_synthesize_identity_for_non_devin_providers ... ok
test test_devin_malformed_typed_json_http_response_does_not_leak_secret ... ok
test test_manual_auth_file_upload_and_stable_identity_replacement ... ok
test test_devin_reload_from_file_display_debug_does_not_leak_secret ... ok
test test_manual_auth_file_upload_persists_validated_normalized_content ... ok
test test_disabled_and_unloaded_credential_exposes_canonical_identity_slug_in_inventory ... ok
test test_disk_rescan_validates_devin_account_and_skips_invalid ... ok
test test_devin_lifecycle_disable_delete_rescan_and_redaction ... ok
test test_distinct_identities_work_and_devin_work_isolated_lifecycle ... ok
test test_cli_import_resolved_on_proxy_host_atomic_and_unchanged_source ... ok
test test_devin_import_cli_typed_payload_allowlist_and_conflicts ... ok
test test_devin_import_cli_rejects_path_escape_and_invalid_identities ... ok

test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.26s
EXIT_CODE: 0
```

---

## 4. Key Architectural Implementations in Scope

### 4.1 Discovery Client & Transport Protocol
- Endpoint: `POST /exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`
- Request Header Invariants:
  - `Content-Type: application/proto`
  - `Accept: application/proto`
  - `Authorization: Basic <token>-<token>` (literal repeated token, unhashed, un-base64-encoded)
  - `Connect-Protocol-Version: 1`
  - Redirects disabled (`reqwest::redirect::Policy::none()`)
- Request Body: `GetCascadeModelConfigsRequest` protobuf message with `metadata.api_key = <token>`.
- Incremental Bounded Read: Reads chunks incrementally up to `MAX_DISCOVERY_RESPONSE_BYTES = 16 * 1024 * 1024` (16 MiB). Immediately halts and returns `DevinDiscoveryError::BodyTooLarge` upon exceeding 16 MiB without reading remainder into memory.
- Total Timeout: Hard-bounded by 10 seconds (`DEVIN_CATALOG_TTL.min(Duration::from_secs(10))`).

### 4.2 Cache Invariants & Lock-Free Storage
- Zero mutable globals; cache is owned by `AppState.devin_cache: Arc<ArcSwap<HashMap<DevinCacheKey, Arc<DevinAccountCatalogState>>>>`.
- Read path is completely lock-free via `ArcSwap::load()`.
- Cache Key: `DevinCacheKey { identity_slug, credential_revision, base_url }`.
- TTL: 300 seconds (5 minutes).
- Stale Reads: Stale records are served immediately if cached, triggering async background refresh.
- Transient Failure: Retains previous valid models marked with `stale: true` and records sanitized `last_error` (e.g. `"upstream 503 Service Unavailable"`).
- Initial Failure: Publishes `initial_failure` state holding `models: []`, `stale: true`, `has_succeeded: false`, and the recorded safe error.
- Credential Races & Monotonic Request Sequencing: In-flight discoveries validate credential revision and pre-RPC monotonic sequence numbers prior to snapshot publication; rotated credentials reject outdated results, and older overlapping requests finishing late return `Err(DevinDiscoveryError::StalePublication)`.

### 4.3 Model Semantics & Capability Isolation
- Public Model IDs: Exact `devin/<model_uid>`. Suffix variants (e.g. `devin/glm-5-2-max`) are preserved verbatim and isolated from base models (`devin/glm-5-2`).
- Excluded Models: Models with `disabled: true` are discarded during protobuf decode.
- `supports_images`: Pinned exclusively to vision input capability (`member.devin_model_supports_vision(...)`); strictly does NOT grant `ModelCapability::Image` (image generation).
- Fallback Prevention: Unknown Devin models fail resolution with registry error; strictly prohibited from falling back to Codex or Generic open providers.
- Quotas & Prices: No fabricated billing, pricing, or output quota claims.

### 4.4 Machine Management API Contracts
- Endpoint: `POST /v0/management/devin/models/refresh`
  - Optional Query: `?identity_slug=<slug>` or `?identity=<slug>`
  - Optional JSON Body: `{"identity_slug": "<slug>"}`
  - Auth: Reuses standard management bearer auth; returns 401 for missing or incorrect tokens.
  - Response Shape:
    ```json
    {
      "status": "ok",
      "outcome": "success",
      "models": ["devin/glm-5-2", "devin/glm-5-2-max"],
      "error": null,
      "accounts": [
        {
          "identity_slug": "devin-a",
          "outcome": "success",
          "generation": 2,
          "models": ["devin/glm-5-2", "devin/glm-5-2-max"],
          "model_count": 2,
          "error": null
        }
      ],
      "generation": 2
    }
    ```
  - Schema: Typed schema registered in `docs/management-contract-v1.schema.json` under `$defs.devin-models-refresh` and route owner map. Documented in `docs/parity-matrix.md`.
  - Visibility: `/v1/models` immediately reflects refreshed models with `owned_by: "devin"`.

---

## 5. Precise P3 Relay Integration Interface & Boundary Handoff

### 5.1 Relay Interface Specification

The P4 implementation publishes an account eligibility and snapshot query interface designed for seamless consumption by the concurrent/follow-up relay call sites:

```rust
// In crates/gateway/src/runtime_state.rs:

impl PoolSnapshot {
    /// Resolves all active accounts eligible to route the given public or unprefixed Devin model.
    pub fn routable_accounts_for_model(&self, model: &str) -> Vec<Arc<AccountMember>>;

    /// Queries immutable snapshot-scoped permissions and metadata for the account.
    pub fn account_permissions(&self, id: &str) -> Option<&AccountSnapshotPermissions>;

    /// Returns the effective base URL captured at this snapshot generation.
    pub fn devin_effective_base_url(&self, id: &str) -> Option<&str>;

    /// Returns the non-secret credential revision string captured at this snapshot generation.
    pub fn devin_credential_revision(&self, id: &str) -> Option<&str>;

    /// Returns true if the account supports the given model UID and has vision input enabled.
    pub fn devin_model_supports_vision(&self, id: &str, model: &str) -> bool;

    /// Checks full eligibility matching requested, canonical, and upstream model IDs against
    /// unsupported models, disabled status, and discovered catalog models.
    pub fn supports_devin_model(&self, requested: &str, canonical: &str, upstream: &str) -> bool;
}

impl AccountSnapshotPermissions {
    pub fn supports_devin_model(&self, requested: &str, canonical: &str, upstream: &str) -> bool;
    pub fn devin_model_supports_vision(&self, model: &str) -> bool;
}
```

### 5.2 Ownership Boundary & Unresolved Integration Edits

1. **Hard Boundary Compliance**: Per task boundaries, `relay.rs`, `url.rs`, `compat/*`, `usage.rs`, `telemetry.rs`, `request_history.rs`, `cp_routes.rs`, and `routes.rs` were NOT authored or mutated by P4. Those components are owned concurrently by the API integration worker.
2. **Relay Call-Site Integration**: Relay routing call-site integration (invoking Devin upstream Connect chat endpoints via `handle_relay`) will be serialized after ownership handoff. P4 tests exercise discovery, cache, snapshot generation, account filtering, and management refresh APIs directly without claiming completion of end-to-end proxy inference routing.
3. **Pre-existing Working-Tree State**: Shared dirty edits in other files (such as ongoing work by the API integration worker) were preserved without resetting or stashing.

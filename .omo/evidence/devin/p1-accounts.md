# Devin P1 — Gateway Credential Registration, Host CLI Import & Account Lifecycle Evidence

**Task ID:** `st_01a09311`  
**Agent Node:** hephaestus  
**Parent Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Root Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Model:** `mahoquot/gemini-3.8-flash-high` (Registered Gemini model exclusively via task routing; separate reasoning setting not exposed by task API)  
**Date:** 2026-09-12  
**Plan Reference:** Plan P1 in `/Users/indo/code/project/mahoquot-proxy/.omo/plans/devin-provider-integration.md` §6.1  

---

## 1. Executive Summary & Scope Compliance Audit

Gateway credential registration, host CLI import, account lifecycle management, security hardening, and JSON parse value leak elimination for the Devin provider have been implemented in strict compliance with the task scope and architectural contracts.

### Scope Compliance
- **Modified files within authorized gateway scope:**
  - `crates/gateway/src/account.rs`:
    - Added `ProviderKind::Devin` and `ProviderAccount::Devin(mahoquot_providers::DevinAccount)`.
    - Added literal Basic upstream headers (`Basic <token>-<token>`).
    - Enforced non-refreshable OAuth policy (static session token, returns `Ok(false)` on refresh).
    - Codex negative space exclusion for `devin/*` and `devin-*`.
    - Unroutable model policy until discovery phase.
    - Safe disk loading with `DevinAccount::validate()` and safe diagnostics skip.
    - Fixed safe Devin parse error messages in `reload_from_file` and `load_account_members` to eliminate raw serde JSON value leaks (e.g. `disabled: "secret-sentinel"`).
    - Preserved non-Devin error diagnostics and behavior.
  - `crates/gateway/src/management/creds.rs`:
    - Added `"devin"` to `is_credential_document`.
    - Customized `describe` listing without secret leakage, isolating identity synthesis strictly to `kind == "devin"`.
    - Ensured manual upload persists the validated normalized `DevinAccount` object.
    - Fixed safe Devin parse error messages in `create_auth_file` and `validate_provider_credential` so serde errors never echo client secret values in HTTP responses.
    - Implemented hardened `POST /v0/management/devin/import-cli` with strict typed allowlist, early identity validation rejecting path escapes before host CLI read or filesystem touch, proxy-host path resolution, offloaded blocking I/O, and atomic persistence.
  - `crates/gateway/src/url.rs`: Added `ProviderKind::Devin` resolution to `upstream_override` or `DEVIN_DEFAULT_API_SERVER_URL` (`https://server.codeium.com`).
  - `crates/gateway/src/quota.rs`: Explicitly returned `Err(QuotaError::Unsupported)` for `ProviderKind::Devin` so quota remains unknown rather than a fake 0%, free, or unlimited.
  - `crates/gateway/src/models_route.rs`: Added minimal exhaustive match arm for `ProviderKind::Devin` in `member_matches_provider_binding`.
  - `crates/gateway/src/relay.rs`: Added minimal exhaustive match arms for `ProviderKind::Devin` in `member_provider_id`, `capture_usage`, and explicit unsupported relay error in `resolve_target` until discovery/relay phases.
  - `crates/gateway/src/warmup.rs`: Added exhaustive match arm `ProviderAccount::Devin(_) => None` under refresh-related gateway matches, preserving all existing dirty work.
  - `crates/gateway/tests/devin_credentials.rs`: Added 16 comprehensive integration tests covering manual upload, CLI import on proxy host, arbitrary path rejection, path escape rejection in temp sandboxes, typed payload allowlist and conflict validation, disk rescan validation/skipping, stable identity replacement, lifecycle disable/delete/rescan, secret redaction, routing isolation, non-Devin describe isolation, quota unsupported contracts, typed JSON parse leak elimination across HTTP responses, reload error Display/Debug, loader diagnostics, and machine schema & route-registration-owner map validation (with pure-prose assertions removed).
  - `docs/management-contract-v1.schema.json`: Documented the `devin-import-cli` route contract under `$defs['devin-import-cli']`, and added exact `"POST /v0/management/devin/import-cli": "crates/gateway/src/management/creds.rs"` mapping to the existing `x-route-registration-owners` map.
  - `docs/parity-matrix.md`: Documented `POST /v0/management/devin/import-cli` in Section 6 (Mahoquot Extension Routes) while preserving the historical 114-endpoint inventory.
- **Untouched files outside scope:**
  - `docs/baseline/cliproxy-endpoints.txt`: Confirmed as historical 114-endpoint inventory; kept strictly unmodified.
  - `crates/gateway/src/management/contracts.rs`: Confirmed ENOENT; no synthetic runtime module or introspection handler invented.
  - `compat/*`: Preserved all pre-existing changes untouched; no P2/compat edits made.
  - `registry/*`: Preserved untouched.
  - `providers/*`: Preserved untouched.
  - `runtime_state.rs`: Preserved untouched.
  - Model discovery, UI, and Chat transport were NOT written in this phase.
  - All pre-existing dirty working tree modifications (e.g. cline generic provider, model-registry http files, index.html) were preserved untouched; no git stash, reset, or commits performed.

---

## 2. Lead Audit & Scoped Correction Verification Evidence

### Failing (RED) Evidence — JSON Parse Value Leak & Route Contract
Prior to applying production fixes for the Final Scoped Correction, the 4 dedicated regression tests failed:

```
$ cd /Users/indo/code/project/mahoquot-proxy && cargo test -p mahoquot-gateway --test devin_credentials
   Compiling mahoquot-gateway v0.1.0 (/Volumes/T9-Mac/project/mahoquot-proxy/crates/gateway)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 5.24s
     Running tests/devin_credentials.rs (target/debug/deps/devin_credentials-e26c41c8df6a299c)

running 16 tests
test test_management_contract_schema_and_parity_matrix_for_devin_import_cli ... FAILED
test test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic ... FAILED
test test_codex_rejects_devin_models_and_devin_unroutable_until_discovery ... ok
test test_devin_no_oauth_refresh_and_quota_unsupported ... ok
test test_devin_malformed_typed_json_http_response_does_not_leak_secret ... FAILED
test test_describe_does_not_synthesize_identity_for_non_devin_providers ... ok
test test_devin_reload_from_file_display_debug_does_not_leak_secret ... FAILED
test test_disabled_and_unloaded_credential_exposes_canonical_identity_slug_in_inventory ... ok
test test_manual_auth_file_upload_persists_validated_normalized_content ... ok
test test_devin_import_cli_rejects_path_escape_and_invalid_identities ... ok
test test_disk_rescan_validates_devin_account_and_skips_invalid ... ok
test test_manual_auth_file_upload_and_stable_identity_replacement ... ok
test test_devin_lifecycle_disable_delete_rescan_and_redaction ... ok
test test_cli_import_resolved_on_proxy_host_atomic_and_unchanged_source ... ok
test test_devin_import_cli_typed_payload_allowlist_and_conflicts ... ok
test test_distinct_identities_work_and_devin_work_isolated_lifecycle ... ok

failures:

---- test_management_contract_schema_and_parity_matrix_for_devin_import_cli stdout ----
thread 'test_management_contract_schema_and_parity_matrix_for_devin_import_cli' (15313134) panicked at crates/gateway/tests/devin_credentials.rs:1294:5:
schema must define $defs['devin-import-cli']

---- test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic stdout ----
thread 'test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic' (15313127) panicked at crates/gateway/tests/devin_credentials.rs:1279:5:
loader diagnostic must NOT embed secret sentinel: 2026-09-12T00:50:20.243131Z WARN mahoquot_gateway::account: skipping credential that does not match its provider schema path="/var/folders/.../devin-loader-leak.json" provider="devin" error=invalid type: string "loader-secret-sentinel-888", expected a boolean

---- test_devin_malformed_typed_json_http_response_does_not_leak_secret stdout ----
thread 'test_devin_malformed_typed_json_http_response_does_not_leak_secret' (15313128) panicked at crates/gateway/tests/devin_credentials.rs:1175:5:
HTTP error response must NOT embed secret sentinel from serde type error: {"error":"invalid devin credential format: invalid type: string \"secret-sentinel-value-12345\", expected a boolean"}

---- test_devin_reload_from_file_display_debug_does_not_leak_secret stdout ----
thread 'test_devin_reload_from_file_display_debug_does_not_leak_secret' (15313130) panicked at crates/gateway/tests/devin_credentials.rs:1216:5:
LoadError Display must NOT contain secret sentinel: parse /var/folders/.../devin-reload-leak.json: invalid type: string "reload-secret-sentinel-999", expected a boolean

failures:
    test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic
    test_devin_malformed_typed_json_http_response_does_not_leak_secret
    test_devin_reload_from_file_display_debug_does_not_leak_secret
    test_management_contract_schema_and_parity_matrix_for_devin_import_cli

test result: FAILED. 12 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s
Exit code: 101
```

### Passing (GREEN) Evidence
After implementing the fixed safe error handling in `creds.rs` and `account.rs`, and updating `docs/management-contract-v1.schema.json` and `docs/parity-matrix.md`, all 16 tests passed cleanly:

```
$ cd /Users/indo/code/project/mahoquot-proxy && cargo test -p mahoquot-gateway --test devin_credentials
   Compiling mahoquot-gateway v0.1.0 (/Volumes/T9-Mac/project/mahoquot-proxy/crates/gateway)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 45.23s
     Running tests/devin_credentials.rs (target/debug/deps/devin_credentials-e26c41c8df6a299c)

running 16 tests
test test_management_contract_schema_and_parity_matrix_for_devin_import_cli ... ok
test test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic ... ok
test test_codex_rejects_devin_models_and_devin_unroutable_until_discovery ... ok
test test_disk_rescan_validates_devin_account_and_skips_invalid ... ok
test test_describe_does_not_synthesize_identity_for_non_devin_providers ... ok
test test_devin_malformed_typed_json_http_response_does_not_leak_secret ... ok
test test_devin_no_oauth_refresh_and_quota_unsupported ... ok
test test_manual_auth_file_upload_persists_validated_normalized_content ... ok
test test_disabled_and_unloaded_credential_exposes_canonical_identity_slug_in_inventory ... ok
test test_devin_reload_from_file_display_debug_does_not_leak_secret ... ok
test test_manual_auth_file_upload_and_stable_identity_replacement ... ok
test test_devin_import_cli_typed_payload_allowlist_and_conflicts ... ok
test test_devin_lifecycle_disable_delete_rescan_and_redaction ... ok
test test_distinct_identities_work_and_devin_work_isolated_lifecycle ... ok
test test_devin_import_cli_rejects_path_escape_and_invalid_identities ... ok
test test_cli_import_resolved_on_proxy_host_atomic_and_unchanged_source ... ok

test result: ok. 16 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.24s
Exit code: 0
```

---

## 3. Detailed Implementations & Defect Resolutions

### 1. JSON Parse Value Leak Elimination
- **Boundary 1 (`validate_provider_credential`):**
  - Replaced `.map_err(|e| format!("invalid devin credential format: {e}"))` with `.map_err(|_| "invalid devin credential format".to_string())`.
  - Serde type errors (such as `invalid type: string "secret-sentinel", expected a boolean`) no longer leak into HTTP 400 responses.
- **Boundary 2 (`create_auth_file`):**
  - Replaced verbatim serde formatting with fixed `"invalid devin credential format"`.
- **Boundary 3 (`reload_from_file`):**
  - In `account.rs`, `serde_json::from_value` mapping produces `LoadError::Parse` with `msg: "invalid devin credential format".to_string()`.
  - Neither `Display` nor `Debug` representations of `LoadError` leak client values.
- **Boundary 4 (`load_account_members`):**
  - When `kind == ProviderKind::Devin` and `provider_account_from_value` fails, logs `error = "invalid devin credential format"` instead of raw `%e`. Non-Devin logging retains `%e` unchanged.
- **Verification:**
  - `test_devin_malformed_typed_json_http_response_does_not_leak_secret` verifies HTTP 400 response body does not contain secret sentinel.
  - `test_devin_reload_from_file_display_debug_does_not_leak_secret` verifies Display and Debug do not contain secret sentinel.
  - `test_devin_loader_skips_malformed_typed_json_without_raw_value_diagnostic` captures tracing logs and verifies secret sentinel is never emitted.

### 2. Route Contract & Documentation Parity
- **`docs/baseline/cliproxy-endpoints.txt` Inspection:**
  - File contains 114 historical upstream CLIProxyAPI endpoints.
  - Preserved strictly unmodified as mandated; extensions must not mutate the historical baseline inventory.
- **`docs/management-contract-v1.schema.json`:**
  - Completed `$defs['devin-import-cli']` defining:
    - `request`: object with optional `identity` (string, minLength: 1), `identity_slug` (string, minLength: 1), and `label` (string), `additionalProperties: false`.
    - `response`: object with required `status: "ok"`, `name` (string, pattern `^devin-.+\.json$`), and `identity_slug` (string), `additionalProperties: false`.
  - Added `"POST /v0/management/devin/import-cli": "crates/gateway/src/management/creds.rs"` into existing `x-route-registration-owners` map.
- **`docs/parity-matrix.md`:**
  - Added Section 6 ("Mahoquot Extension Routes") documenting `POST /v0/management/devin/import-cli` under the `Auth & Identity` cluster, tier P1, handled by `creds::devin_import_cli`.
  - Explicitly documents optional label allowing empty string, identity and identity_slug alias rules, host resolution priority, atomic write, untouched source TOML, and response schema.
- **Verification & Test Discipline:**
  - `test_management_contract_schema_and_parity_matrix_for_devin_import_cli` executes machine validation over `$defs['devin-import-cli']` and verifies route owner mapping equality in `x-route-registration-owners`.
  - Pure prose text assertions (`include_str!(parity-matrix.md).contains(endpoint)`) were removed in adherence to Test Discipline ("Never pin prose, prompt wording, or doc text with a test; test only machine-consumed values").

### 3. Path Escape & Strict Allowlist Hardening (Retained)
- Identity slug validated before host CLI read or touching filesystem (`target_path.parent() == Some(auth_dir.as_path())`).
- Strict body allowlist permitting only `identity`, `identity_slug`, `label`.
- Explicit empty strings for identity/identity_slug rejected; omitted defaults to `"devin"`; conflicting aliases rejected.
- Source TOML never mutated or removed; blocking file I/O offloaded via `spawn_blocking`.

### 4. Disk Rescan & Normalization Boundaries (Retained)
- `load_account_members` validates loaded `DevinAccount` via `.validate()`. Invalid files skipped with safe diagnostic.
- `create_auth_file` persists normalized `DevinAccount` object, ensuring disk bytes match memory.
- `describe()` restricts identity synthesis strictly to `kind == "devin"`.

---

## 4. Full Workspace & Tooling Verification

### LSP Diagnostics
- Path: `crates/gateway/src/management/creds.rs`: **0 diagnostics**
- Path: `crates/gateway/src/account.rs`: **0 diagnostics**
- Path: `crates/gateway/tests/devin_credentials.rs`: **0 diagnostics**

### Suite Verification Results
1. `cargo test -p mahoquot-providers --test devin_credentials`: **41 passed; 0 failed** (0.01s)
2. `cargo test -p mahoquot-gateway --test devin_credentials`: **16 passed; 0 failed** (0.23s)
3. `cargo test -p mahoquot-gateway --test management_registry`: **4 passed; 0 failed** (0.04s)
4. `cargo test -p mahoquot-gateway --test t13_provider_oauth`: **18 passed; 0 failed** (0.22s)
5. `cargo check`: **Finished with 0 errors (exit code 0)**
6. `cargo check -p mahoquot-gateway`: **Finished with 0 errors (exit code 0)**

### Router & Evidenced Gaps Inspection
- **Runtime Module Verification:**
  - Verifier speculation regarding a runtime `crates/gateway/src/management/contracts.rs` introspection gap was confirmed invalid: `crates/gateway/src/management/contracts.rs` does not exist (ENOENT) in the repository. As directed, no synthetic handler or module was created.
- **Router Mounting Verification:**
  - `POST /v0/management/devin/import-cli` is registered in `crates/gateway/src/management/creds.rs::creds_routes()`.
  - `creds_routes()` is mounted into `crates/gateway/src/management/mod.rs::management_router()`, which is nested at `/v0/management` in `crates/gateway/src/routes.rs`.
- **Evidenced Cross-Suite Contract Gap:**
  - `crates/gateway/tests/config_endpoints_tests.rs::contract_route_names_have_one_registration_owner` (lines 134-138) contains a hardcoded assertion that allows only `owner == "crates/gateway/src/management/contracts.rs" || owner == "crates/gateway/src/management/registry.rs"`.
  - Because `POST /v0/management/devin/import-cli` has owner `"crates/gateway/src/management/creds.rs"`, `config_endpoints_tests.rs` fails on that assertion.
  - `config_endpoints_tests.rs` is strictly outside the authorized file scope for this phase and was preserved untouched. The lead or the config endpoint owner should broaden `config_endpoints_tests.rs` to allow `"crates/gateway/src/management/creds.rs"` (or future management modules).

---

## 5. Downstream Contracts & Next Phase Hand-Off

- **Wire Codec (P2):** Upstream Basic auth uses literal `Basic <token>-<token>`. Protobuf messages require Connect RPC headers (`content-type: application/connect+proto`, `connect-protocol-version: 1`).
- **Relay Protocol (P3):** Will implement protocol streaming for Connect RPC over `https://server.codeium.com` and replace the temporary unsupported error in `relay.rs:448`.
- **Model Discovery (P4):** Will implement dynamic model catalog discovery via `GetCascadeModelConfigs` and contribute models to runtime pool generation.
- **Management & UI (P6):** Consumes `POST /v0/management/devin/import-cli`, `POST /v0/management/auth-files`, and `GET /v0/management/auth-files`. Quota correctly displays as unknown (unsupported) rather than 0% or unlimited.

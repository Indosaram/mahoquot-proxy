# Devin P6 — Console UI Verification & Cross-Layer Integration Audit

**Document Path**: `/Users/indo/code/project/mahoquot-proxy/.omo/evidence/devin/p6-verification.md`  
**Date**: 2026-09-12  
**Task ID**: `st_01a092cf`  
**Worker Node**: `hephaestus`  
**Assigned Model**: `mahoquot/gemini-3.8-flash-high` (sole execution, no delegation)  
**Parent / Root Session**: `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Target Scopes**:
- Frontend: `/Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend`
- Backend Contracts & Handlers: `/Users/indo/code/project/mahoquot-proxy/crates/gateway`, `crates/providers`, & `docs/`
- Production Artifacts: Zero modifications to production source or shipped bundles (`crates/monitor-ui/ui/index.html` and `mahoquot-proxy/ui/index.html` untouched; build artifacts routed strictly to `/tmp/devin-console-p6-build`)

---

## 1. Executive Summary & Verification Matrix

The Devin console UI implementation under `quotio-rs/crates/monitor-ui/frontend` was comprehensively verified using non-destructive, read-only validation commands and cross-layer inspection against backend contracts and runtime implementations. All Lead Source Review corrections have been verified in source code and unit tests.

| Category | Command / Inspection | Status | Comparison to `baseline.md` |
|---|---|---|---|
| **TypeScript Typecheck** | `bun run typecheck` (`tsc --noEmit`) | **PASS** (exit 0) | Clean (0 compiler errors), matches baseline exit 0. |
| **Vitest Unit & Surface Suite** | `bun run test` (all suites once) | **PASS** (exit 0) | **45 files, 401 passed** in 9.62s (baseline: 43 files, 372 passed; +2 test files, +29 tests added). |
| **Console Linter (Repo-wide)** | `bun run lint` (`biome check src`) | **FAIL** (exit 1) | **20 errors** across 135 files (18 baseline errors preserved; 2 pre-existing errors in `SettingsSurface.tsx`; **0 Devin errors**). |
| **Devin Files Linter** | `bunx biome check <devin-scoped-files>` | **PASS** (exit 0) | **9 files checked, 0 errors, 0 warnings**. |
| **Production Build Isolation** | `bun run build -- --outDir /tmp/devin-console-p6-build` | **PASS** (exit 0) | Singlefile bundle generated cleanly at `/tmp/devin-console-p6-build/index.html` (875.29 kB); shipped UI untouched. |
| **Injective Filename Mapping** | Injective `devinCredentialFileName(identity)` | **PASS** (verified) | `work` -> `devin-work.json`, `devin-work` -> `devin-devin-work.json`. Distinct identities never collide or overwrite. |
| **Two-Account Coexistence** | Full E2E coexistence & reimport/reauth test | **PASS** (verified) | Simultaneous rendering of `work` and `devin-work`; distinct re-import payloads; distinct reauth file targets. |
| **Model Entitlement Isolation** | Two-account isolation regression test | **PASS** (verified) | Devin cards display `Unknown` per-account rather than falsely attributing global `gatewayModels` union. |
| **Exact Identity Preservation** | Canonical slug & filename generation test | **PASS** (verified) | Both `work` and `devin-work` preserved verbatim across reimport/reauth without stripping `devin-` prefix. |
| **Discovery Contract Parity** | `DevinModelRefreshResponseSchema` test | **PASS** (verified) | Explicit machine contract: accepts `error: null` on success, rejects empty `{}` and `{ error: null }` alone. |
| **Secret Non-Exposure** | Storage, memory, and network inspection | **PASS** (verified) | Password masking, zero tokens in storage or logs, CLI import sends zero local credentials. |
| **Brand Glyph Asset & Styling** | Logo asset & monochrome inversion check | **PASS** (verified) | Byte-exact Cognition Mintlify favicon SVG; registered in monochrome logo set. |
| **Backend Credential Persistence** | `POST /v0/management/auth-files` with `type="devin"` | **PASS** (backend test) | Gateway `validate_provider_credential` accepts `"devin"`. Verified by `devin_credentials.rs`. |
| **Backend CLI Import Route** | `POST /v0/management/devin/import-cli` | **PASS** (backend test) | Mounted in `creds.rs`; imports host TOML atomically. Verified by `devin_credentials.rs`. |
| **Backend CLI Filename Mapping** | Host CLI import filename generation | **DEFECT** (backend gap) | `creds.rs:1139` uses non-injective prefix collapse (`slug.starts_with("devin-") ? format!("{slug}.json") : format!("devin-{slug}.json")`), colliding `devin-work` into `devin-work.json`. |
| **Backend Discovery Refresh Route** | `POST /v0/management/devin/models/refresh` | **DEFECT / PENDING** | Unimplemented on gateway backend; hits Axum router fallback (HTTP 404). |
| **Backend Per-Account Model Stats** | `GET /admin/stats` `models` field | **DEFECT / PENDING** | `AccountStats` in `metrics.rs` lacks `models: Option<Vec<String>>`; per-account models cannot be shown yet. |
| **Contract Schema Parity** | `docs/management-contract-v1.schema.json` | **DEFECT / PENDING** | Devin management endpoints omitted from OpenAPI/contract schema declaration. |

---

## 2. Command Executions and Output

### 2.1 TypeScript Typecheck
- **Directory**: `/Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend`
- **Command**: `bun run typecheck`
- **Exit Code**: `0`
- **Output**:
  ```text
  $ tsc --noEmit
  ```
- **Evaluation**: PASS. Zero compiler diagnostics across all source and test files.

### 2.2 Frontend Test Suite (All 45 Test Files Once)
- **Directory**: `/Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend`
- **Command**: `bun run test`
- **Exit Code**: `0`
- **Execution Time**: 9.62s
- **Output Summary**:
  ```text
  $ vitest run

   RUN  v3.2.7 /Volumes/T9-Mac/project/mahoquot/crates/monitor-ui/frontend

   ✓ src/__tests__/todo15-focused-red-green.test.tsx (3 tests) 568ms
   ✓ src/__tests__/agents-surface.test.tsx (9 tests) 255ms
   ✓ src/__tests__/durable-logs.test.tsx (11 tests) 630ms
   ✓ src/__tests__/totp-vault.test.ts (7 tests) 107ms
   ✓ src/__tests__/tray-panel.test.tsx (2 tests) 204ms
   ✓ src/__tests__/overview-token-usage.test.tsx (17 tests) 230ms
   ✓ src/__tests__/native-focus.test.ts (3 tests) 61ms
   ✓ src/__tests__/accounts-surface.test.tsx (15 tests) 672ms
   ✓ src/__tests__/task15-runtime-model-alignment.test.tsx (6 tests) 328ms
   ✓ src/__tests__/review-round1-regressions.test.tsx (2 tests) 887ms
   ✓ src/__tests__/dial-probe.test.tsx (2 tests) 535ms
   ✓ src/__tests__/totp-vault-hook.test.tsx (5 tests) 271ms
   ✓ src/__tests__/connection-settings.test.tsx (5 tests) 1854ms
   ✓ src/__tests__/tunnel-settings.test.tsx (2 tests) 1921ms
   ✓ src/__tests__/durable-logs-api.test.ts (2 tests) 10ms
   ✓ src/__tests__/use-gateway-polling.test.tsx (3 tests) 210ms
   ✓ src/__tests__/layout.test.tsx (15 tests) 196ms
   ✓ src/__tests__/operations-history.test.tsx (8 tests) 2405ms
   ✓ src/__tests__/shared-keys.test.tsx (17 tests) 3755ms
   ✓ src/__tests__/codex-launcher.test.tsx (1 test) 142ms
   ✓ src/__tests__/dither-area.test.tsx (3 tests) 141ms
   ✓ src/__tests__/toasts.test.tsx (7 tests) 205ms
   ✓ src/__tests__/provider-mix-colors.test.tsx (2 tests) 69ms
   ✓ src/__tests__/context-menu.test.ts (6 tests) 16ms
   ✓ src/__tests__/logs-settings-surface.test.tsx (9 tests) 3040ms
   ✓ src/__tests__/api.test.ts (16 tests) 15ms
   ✓ src/__tests__/telemetry.test.ts (33 tests) 11ms
   ✓ src/__tests__/startup-loading.test.ts (5 tests) 128ms
   ✓ src/__tests__/storage.test.ts (9 tests) 12ms
   ✓ src/__tests__/devin-onboarding-lifecycle.test.tsx (20 tests) 3499ms
   ✓ src/__tests__/artifact.test.ts (1 test) 65ms
   ✓ src/__tests__/provider-catalog.test.ts (19 tests) 14ms
   ✓ src/__tests__/schemas.test.ts (7 tests) 4ms
   ✓ src/__tests__/log-stream.test.ts (4 tests) 3ms
   ✓ src/__tests__/theme-contrast.test.ts (2 tests) 10ms
   ✓ src/__tests__/zcode-auth-response.test.ts (1 test) 4ms
   ✓ src/__tests__/plan-tier.test.ts (17 tests) 6ms
   ✓ src/__tests__/accounts.test.ts (14 tests) 5ms
   ✓ src/__tests__/pending.test.ts (7 tests) 5ms
   ✓ src/__tests__/relay-plans.test.ts (5 tests) 4ms
   ✓ src/__tests__/notch.test.ts (8 tests) 5ms
   ✓ src/components/dither-kit/axis-ticks.test.ts (5 tests) 8ms
   ✓ src/__tests__/devin-catalog.test.ts (9 tests) 4ms
   ✓ src/__tests__/provider-colors.test.ts (4 tests) 1ms
   ✓ src/__tests__/app.test.tsx (53 tests) 5835ms

   Test Files  45 passed (45)
        Tests  401 passed (401)
     Duration  9.62s
  ```
- **Comparison to Baseline**:
  - `baseline.md`: 43 test files, 372 passed tests.
  - Current suite: 45 test files, 401 passed tests (+2 files, +29 tests added).
  - New test files: `src/__tests__/devin-catalog.test.ts` (9 tests) and `src/__tests__/devin-onboarding-lifecycle.test.tsx` (20 tests).
  - Note on test count progression: The initial P6 suite had 19 tests in `devin-onboarding-lifecycle.test.tsx` (400 total). The lead's direct review correction added an explicit two-account coexistence regression test, bringing the suite to 20 tests (401 total).
  - Zero test failures, zero regressions across all suites. Pre-existing React `act(...)` console warnings in `app.test.tsx` match the baseline exactly.

### 2.3 Biome Linter
- **Directory**: `/Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend`
- **Command**: `bun run lint`
- **Exit Code**: `1`
- **Diagnostic Count**: 20 errors across 135 checked files.
- **Detailed Baseline Audit**:
  All 18 errors recorded in `baseline.md` are accounted for (line numbers shifted naturally due to upstream edits):
  1. `src/__tests__/native-focus.test.ts`: formatting (Baseline item #1)
  2. `src/__tests__/overview-token-usage.test.tsx`: formatting (Baseline item #2)
  3. `src/__tests__/zcode-auth-response.test.ts`: formatting (Baseline item #3)
  4. `src/__tests__/use-gateway-polling.test.tsx`: formatting (Baseline item #4)
  5. `src/App.tsx:97`: `lint/style/useImportType` (Baseline item #5, shifted from line 91)
  6. `src/App.tsx:1196`: `lint/correctness/useExhaustiveDependencies`, missing `setNotice` (Baseline item #6, shifted from line 1081)
  7. `src/components/OverviewTokenUsage.tsx:116`: `lint/style/noNonNullAssertion` (Baseline item #7)
  8. `src/components/OverviewTokenUsage.tsx:118`: `lint/style/noNonNullAssertion` (Baseline item #8)
  9. `src/components/OverviewTokenUsage.tsx:122`: `lint/style/noNonNullAssertion` (Baseline item #9)
  10. `src/components/OverviewTokenUsage.tsx:124`: `lint/style/noNonNullAssertion` (Baseline item #10)
  11. `src/components/OverviewTokenUsage.tsx`: formatting (Baseline item #11)
  12. `src/App.tsx`: `organizeImports` (Baseline item #12)
  13. `src/__tests__/telemetry.test.ts`: formatting (Baseline item #13)
  14. `src/components/OverviewDashboard.tsx`: formatting (Baseline item #14)
  15. `src/App.tsx`: formatting (Baseline item #15)
  16. `src/hooks/useGatewayPolling.ts:81`: `lint/correctness/useExhaustiveDependencies` (Baseline item #16)
  17. `src/lib/accounts.ts:344`: `lint/complexity/useOptionalChain` (Baseline item #17, shifted from line 309)
  18. `src/lib/telemetry.ts`: formatting (Baseline item #18)
  
  The 2 additional diagnostics are pre-existing in `SettingsSurface.tsx` from concurrent proxy-policy work (which was preserved untouched per prompt instructions):
  19. `src/components/SettingsSurface.tsx:625`: `lint/style/useNumberNamespace`
  20. `src/components/SettingsSurface.tsx`: formatting

- **Dedicated Devin Scope Audit**:
  Command:
  ```bash
  bunx biome check \
    src/__tests__/devin-catalog.test.ts \
    src/__tests__/devin-onboarding-lifecycle.test.tsx \
    src/lib/provider-catalog.ts \
    src/lib/schemas.ts \
    src/lib/api.ts \
    src/lib/onboarding.ts \
    src/components/AccountCard.tsx \
    src/components/AccountsSurface.tsx \
    src/components/ProviderGlyph.tsx
  ```
  Result: **Checked 9 files in 13ms. No fixes applied. Clean (0 errors, 0 warnings)**.

### 2.4 Isolated Production Build
- **Directory**: `/Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend`
- **Command**: `bun run build -- --outDir /tmp/devin-console-p6-build`
- **Exit Code**: `0`
- **Execution Time**: 2.11s
- **Raw Output**:
  ```text
  $ vite build --outDir "/tmp/devin-console-p6-build"
  vite v6.4.3 building for production...
  transforming...
  ✓ 1945 modules transformed.
  rendering chunks...
  [plugin vite:singlefile] 

  [plugin vite:singlefile] Inlining: index-BDtRTx7f.js
  [plugin vite:singlefile] Inlining: style-CA-2q-pr.css
  computing gzip size...
  ../../../../../../../tmp/devin-console-p6-build/index.html  875.29 kB │ gzip: 304.45 kB
  ✓ built in 2.11s
  ```
- **Artifact Verification**:
  - Path: `/tmp/devin-console-p6-build/index.html`
  - Size: 875,290 bytes (855 kB on disk)
  - Shipped UI safety: Neither `crates/monitor-ui/ui/index.html` nor `mahoquot-proxy/ui/index.html` were modified or overwritten by this build.

---

## 3. Lead Source Review Corrections Verification

All items identified in the lead source review were audited directly against source code and regression tests:

### 3.1 Injective Filename Mapping & Re-Authentication Targeting
- **Defect Identified**: Previously, `devinCredentialFileName(identity)` checked `clean.startsWith("devin-") ? ... : ...`, which caused `devinCredentialFileName("work")` and `devinCredentialFileName("devin-work")` to BOTH return `"devin-work.json"`. This caused distinct accounts to collide and overwrite each other's credentials.
- **Remediation in Code**:
  - `src/lib/schemas.ts:593-597`:
    ```typescript
    export const devinCredentialFileName = (identity: string): string => {
      const clean = identity.trim();
      if (!clean) return "devin.json";
      return `devin-${clean}.json`;
    };
    ```
    The mapping is strictly injective: $f(a) = f(b) \iff a = b$.
    - `work` maps to `"devin-work.json"`.
    - `devin-work` maps to `"devin-devin-work.json"`.
    - User identities beginning with `devin-` are never collapsed into the namespace prefix.
  - `src/App.tsx:1081`:
    ```typescript
    const fileName = form.credentialName || devinCredentialFileName(form.identity.trim());
    await clients.management.importCredential(fileName, normalized);
    ```
    When an existing account is re-authenticated, `reauthenticate` passes `account.credentialName`, ensuring re-auth stably targets the exact file the account was loaded from without creating new files or recalculating.
  - `src/lib/accounts.ts:280-290`:
    In `pairAccountsWithCredentials`, pairing prioritizes exact `c.identity_slug === account.id` or `c.name === devinCredentialFileName(account.id)` before falling back to stem heuristics, preventing greedy mispairing when `work` and `devin-work` coexist.
- **Verification in Test** (`src/__tests__/devin-onboarding-lifecycle.test.tsx:516-664`):
  - Injective mapping unit test:
    ```typescript
    expect(devinCredentialFileName("work")).toBe("devin-work.json");
    expect(devinCredentialFileName("devin-work")).toBe("devin-devin-work.json");
    expect(devinCredentialFileName("work")).not.toBe(devinCredentialFileName("devin-work"));
    expect(devinCredentialFileName("devin-cli-work")).toBe("devin-devin-cli-work.json");
    ```
  - Two-account coexistence test mounts both accounts simultaneously and verifies:
    1. Simultaneous rendering of "Work Account" and "Devin Work Account" cards.
    2. Re-import CLI on "work" sends `{ identity: "work", label: "Work Account" }`.
    3. Re-import CLI on "devin-work" sends `{ identity: "devin-work", label: "Devin Work Account" }`.
    4. Re-auth on "work" targets `"devin-work.json"` with `{ identity_slug: "work" }`.
    5. Re-auth on "devin-work" targets `"devin-devin-work.json"` with `{ identity_slug: "devin-work" }`.

### 3.2 Model Entitlement Isolation & Anti-Leakage
- **Defect Identified**: `AccountsSurface.tsx` previously computed Devin card models from global `gatewayModels` and staleness from global `modelRegistryStatus.stale`, falsely attributing global catalog models to individual Devin accounts.
- **Remediation in Code** (`src/components/AccountsSurface.tsx:205-217`):
  ```typescript
  // Until P4 per-account discovery data exists, do NOT claim global union
  // (gatewayModels) is each account entitlement. Show unknown per-account.
  const perAccountModels = (account as { models?: readonly string[] }).models;
  const devinModels = isDevin && perAccountModels ? perAccountModels : undefined;
  const discoveryState = isDevin
    ? hasDiscoveryError
      ? ("error" as const)
      : devinModels && devinModels.length > 0
        ? ("available" as const)
        : ("unknown" as const)
    : undefined;
  ```
- **Verification in Test** (`src/__tests__/devin-onboarding-lifecycle.test.tsx:870-945`):
  The two-account regression test mounts two Devin accounts (`devin-work` and `devin-personal`) alongside `/v1/models` containing `devin/glm-5-2` and `devin/swe-1-7`. It asserts that both account cards render `<Badge tone="neutral">Unknown</Badge>` and neither card displays or leaks the global models list.

### 3.3 Exact Identity Preservation
- **Defect Identified**: `extractDevinSlug` previously stripped `devin-` prefixes even when the user's canonical identity legitimately began with `devin-` (e.g. `devin-work` normalized to `work`).
- **Remediation in Code**:
  - `src/lib/accounts.ts:497`:
    ```typescript
    export const extractDevinSlug = (account: NormalizedAccount): string => {
      if (account.identitySlug) {
        return account.identitySlug;
      }
      if (account.runtimeId) {
        return account.runtimeId;
      }
      if (account.credentialName) {
        return account.credentialName.replace(/\.json$/i, "");
      }
      return account.id.replace(/^cred-/, "").replace(/\.json$/i, "");
    };
    ```
  - Both `cleanAccountLabel` and `cleanDetail` retain the exact identity without stripping `devin-`.
- **Verification in Test** (`src/__tests__/devin-onboarding-lifecycle.test.tsx:666-805`):
  Verifies re-import CLI and re-auth for both `devin-work` and `work`:
  - `devin-work` preserves `{ identity: "devin-work", label: "Devin Work" }` and reauth pre-fills `"devin-work"`.
  - `work` preserves `{ identity: "work", label: "Work" }` and reauth pre-fills `"work"`.

### 3.4 Explicit Machine Contract for Model Discovery Schema
- **Defect Identified**: `DevinModelRefreshResponseSchema` previously rejected `error: null` despite the published response contract including it, and allowed empty `{}` success.
- **Remediation in Code** (`src/lib/schemas.ts:608-628`):
  ```typescript
  export const DevinModelRefreshResponseSchema = z
    .object({
      status: z.string().optional(),
      models: z.array(z.string()).optional(),
      outcome: z.enum(["success", "error", "never"]).optional(),
      error: z.string().nullable().optional(),
    })
    .passthrough()
    .refine(
      (data) =>
        Boolean(
          data.status !== undefined ||
            data.outcome !== undefined ||
            data.models !== undefined ||
            (data.error !== undefined && data.error !== null),
        ),
      {
        message: "Devin model refresh response must contain status, outcome, models, or error",
      },
    );
  ```
- **Verification in Test** (`src/__tests__/devin-onboarding-lifecycle.test.tsx:88-120`):
  - Validates success with `error: null` and models array -> succeeds.
  - Validates error outcome with string error message -> succeeds.
  - Validates empty `{}` -> strictly throws schema validation error.
  - Validates `{ error: null }` alone without status, outcome, or models -> strictly throws schema validation error.

---

## 4. Security, Token Non-Exposure & UI Invariants

1. **Secret Masking**: In `src/App.tsx:2020`, the manual session token input uses `type="password"`.
2. **Zero Client Storage**: In `src/__tests__/devin-onboarding-lifecycle.test.tsx:850`, assertions confirm that neither `localStorage` nor `sessionStorage` contains any part of the entered session token (`devin-secret-token$...`).
3. **No Query Parameter Exposure**: Neither `importCredential`, `importDevinCli`, nor `refreshDevinModels` appends tokens to URL query parameters.
4. **Zero Local Credentials Sent on CLI Import**: In `src/lib/api.ts:495` (`importDevinCli`), the request payload sends exclusively `{ identity, label }` (or `{ identity_slug, label }`). No browser-local files or token bytes are transmitted over the wire.
5. **Token Boundary Validation**: `DevinManualCredentialInputSchema` in `src/lib/schemas.ts:544` enforces that tokens must not contain whitespace or ASCII control characters (`charCodeAt(0) < 32 || 127`), guarding against HTTP header injection. Maximum token length is capped at 4096 bytes.
6. **Unknown Quota Representation**: Devin quota capability is strictly `"unsupported"`, rendering `Not reported by provider`. The UI never invents 0%, free, or unlimited metrics.
7. **Brand Glyph Integrity**: `src/assets/provider-logos/devin.svg` matches byte-exact Cognition Mintlify docs favicon SVG (`viewBox="0 0 425 425"`). It is registered in `MONOCHROME_LOGOS` with black-to-white inverter styling for dark theme contrast.

---

## 5. Honest Cross-Layer Gaps & Defects (Backend vs Frontend)

A cross-layer audit was conducted across `/Users/indo/code/project/mahoquot-proxy/crates/gateway`, `crates/providers`, and `docs/`.

### Resolved Backend Capabilities (Verified with `cargo test`)
The following items previously noted as pending were confirmed implemented in `mahoquot-proxy`:
- `GenericAccount.email` initializers in `account.rs` and `models_route.rs` have been updated; `cargo test -p mahoquot-registry -p mahoquot-providers -p mahoquot-gateway --lib` compiles cleanly and passes all 381 unit tests.
- `ProviderKind::Devin` and `ProviderAccount::Devin` are wired in `crates/gateway/src/account.rs`.
- `validate_provider_credential` and `is_credential_document` in `creds.rs` recognize `"devin"`.
- `POST /v0/management/devin/import-cli` is mounted in `creds.rs` and reads `credentials.toml` from the host.
- Integration tests in `crates/gateway/tests/devin_credentials.rs` (5 tests) pass with exit 0.

### Concrete Remaining Defects & Gaps

#### Gap 1: Host CLI Import Filename Collapsing in Backend `creds.rs` (Defect)
- **Frontend Contract**: Uses injective filename mapping `devinCredentialFileName(identity) = "devin-${identity}.json"`.
  - `work` -> `"devin-work.json"`
  - `devin-work` -> `"devin-devin-work.json"`
- **Backend Loader (`account.rs:1563`)**: Strips `"devin-"` once:
  - `"devin-work.json"` -> slug `"work"`
  - `"devin-devin-work.json"` -> slug `"devin-work"`
- **Backend Import Route (`creds.rs:1139`)**:
  ```rust
  let slug = identity.unwrap_or_else(|| "devin".to_string());
  let filename = if slug.starts_with("devin-") {
      format!("{slug}.json")
  } else {
      format!("devin-{slug}.json")
  };
  ```
- **Observed Defect**: When the user imports CLI credentials with identity `"devin-work"`, backend `creds.rs` generates `"devin-work.json"`. This collapses the filename, overwrites any account named `"work"`, and causes `account.rs` to reload it as `"work"` rather than `"devin-work"`.
- **Required Backend Fix**: Align `creds.rs:1139` with the injective rule: `let filename = format!("devin-{slug}.json");`.

#### Gap 2: Missing Model Discovery Refresh Endpoint (Defect / Pending P4)
- **Frontend Action**: `refreshDevinDiscovery` invokes `POST /v0/management/devin/models/refresh?identity_slug=<slug>`.
- **Backend Code**: `crates/gateway/src/management/registry.rs:89`:
  Only `/model-registry` is mounted.
- **Observed Defect**: Probing `/v0/management/devin/models/refresh` falls through to Axum's fallback handler and returns **`HTTP 404 NOT FOUND`**.

#### Gap 3: Missing Per-Account Model Entitlements in `GET /admin/stats` (Defect / Pending P4)
- **Frontend Behavior**: Displays `Unknown` on Devin account cards unless `account.models` is provided by the gateway.
- **Backend Code**: `crates/gateway/src/metrics.rs:73` (`AccountStats`):
  Lacks a `models: Option<Vec<String>>` field. The gateway runtime pool maintains no per-account model list for Devin.
- **Observed Defect**: Frontend has no machine data source for per-account Devin entitlements; properly displays `Unknown` to prevent cross-account misattribution.

#### Gap 4: Missing Contract Schema Registration (Defect / Pending Documentation)
- **Contract Schema**: `docs/management-contract-v1.schema.json` defines OpenAPI routes under `x-route-registration-owners`.
- **Observed Defect**: Does not declare `/v0/management/devin/import-cli` or `/v0/management/devin/models/refresh`.

#### Gap 5: Backend Devin Relay Protocol Unimplemented (Pending P3)
- **Backend Code**: `crates/gateway/src/relay.rs:448`:
  `return Err("devin relay protocol is not implemented yet; model discovery and relay are planned for next phase".to_string());`
- **Observed Defect**: Streaming LLM requests targeting `devin/*` will fail at the relay handler until P3 Connect protocol proxying is wired.

---

## 6. Exact Remaining Cross-Layer Items for Backend Integration

The following sequence of changes is required on the backend before live E2E verification:

1. **Fix Injective Filename Mapping in `creds.rs` (P1)**:
   In `crates/gateway/src/management/creds.rs:1139`, change:
   ```rust
   let filename = format!("devin-{slug}.json");
   ```
   to prevent `devin-work` and `work` from colliding on `"devin-work.json"`.
2. **Implement Management Route `POST /v0/management/devin/models/refresh` (P4)**:
   In `crates/gateway/src/management/registry.rs` or `creds.rs`, implement a route handler that:
   - Accepts optional `identity_slug` query parameter.
   - Dispatches `GetCascadeModelConfigs` unary Connect RPC to Devin upstream (`https://server.codeium.com`).
   - Merges discovered models (`devin/<uid>`) into `RegistrySnapshot`.
   - Returns `{ "status": "ok", "outcome": "success", "models": [...], "error": null }`.
3. **Expose Per-Account Models in Stats (P4)**:
   In `crates/gateway/src/metrics.rs`, add `pub models: Option<Vec<String>>` to `AccountStats`. Populate it in `state.get_stats()` from the account member's discovered model set.
4. **Declare Routes in Contract Schema**:
   Add entries for `/v0/management/devin/import-cli` and `/v0/management/devin/models/refresh` to `docs/management-contract-v1.schema.json`.
5. **Implement Devin Relay Protocol (P3)**:
   Wire Connect framing decoder/encoder in `crates/gateway/src/relay.rs` using `crates/gateway/src/compat/devin.rs`.
6. **Production UI Bundle Synchronization**:
   Once backend routes are active and tested, build and synchronize the production bundle:
   ```bash
   cd /Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend
   bun run build
   bun run sync:proxy
   ```
7. **Final Browser QA by Gateway Lead**:
   Per task instructions, browser QA against the live gateway with real upstream credentials remains owned by the gateway lead following backend completion. This report explicitly does not claim live browser QA is complete.

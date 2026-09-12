# Devin P6 — Frontend Management and Discovery Integration Evidence Report

**Task ID:** `st_01a093fe`  
**Implementer / Verifier Node:** hephaestus  
**Parent Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Root Session:** `01a090bb-807b-7869-b40a-dbc89de2af7a`  
**Depth:** 1  
**Model:** `mahoquot/gemini-3.8-flash-high` (exclusive assigned execution; zero subdelegation)  
**Date:** 2026-09-12  
**Target Scope:** `src/lib/schemas.ts`, `src/lib/api.ts`, `src/lib/accounts.ts`, `src/lib/onboarding.ts`, `src/App.tsx`, `src/components/AccountsSurface.tsx`, `src/components/AccountCard.tsx`, and focused tests in `src/__tests__/`  
**Deliverable Path:** `.omo/evidence/devin/p6-integration.md`  

---

## 1. Executive Summary & Verification Verdict

### Verdict: PASS (100% Tests Green, Typecheck Clean, Vite Build Clean)

The P6 frontend Devin management and discovery integration in `quotio-rs/crates/monitor-ui/frontend` is complete. Frontend components, schemas, and API clients now speak the authoritative backend gateway contracts for Devin model status and refresh, enforce per-account truth over misleading global outcomes, and provide complete user feedback for import, update, disable, delete, and model refresh actions without credential disclosure.

### Key Deliverables:
1. **Typed Schemas for Actual Backend Contracts (`src/lib/schemas.ts`)**:
   - `DevinAccountStatusSchema`: parses `{ identity_slug, status: "active" | "stale" | "uninitialized", models, stale, last_refresh_at, error, disabled }`.
   - `DevinModelsStatusResponseSchema`: validates `GET /v0/management/devin/models/status` returning `{ status, models, accounts, generation }`.
   - `DevinAccountRefreshResultSchema`: parses `{ identity_slug, status: "success" | "error", models, stale, last_refresh_at, error }`.
   - `DevinModelRefreshResponseSchema`: updated to type and validate the `accounts` array from `POST /v0/management/devin/models/refresh`.
2. **API Client Additions (`src/lib/api.ts`)**:
   - `getDevinModelsStatus(): Promise<DevinModelsStatusResponse>` added to `GatewayClients.management`, calling `GET /v0/management/devin/models/status`.
   - `refreshDevinModels(identitySlug?: string): Promise<DevinModelRefreshResponse>` query serialization preserved and responses parsed through typed schema.
3. **Per-Account Truth Enforcement (`src/App.tsx` & `src/components/AccountsSurface.tsx`)**:
   - `refreshDevinDiscovery(account)` inspects `resp.accounts` for the target account's slug:
     - If the account reported `status: "error"` or `error: string`, the notice announces `Devin model discovery failed for <label>: <error>`, even when top-level `resp.outcome === "success"`.
     - If the account reported `stale: true`, notice announces `Devin models refreshed for <label> (stale models preserved): N available.`
     - If successful and fresh, notice announces `Devin models refreshed for <label>: N available.`
   - `devinStatusBySlug` state is stored in `App.tsx` and forwarded to `<AccountsSurface />`.
   - `<AccountsSurface />` maps account discovery states strictly per-account:
     - Error -> `<Badge tone="bad">Discovery error</Badge>`
     - Stale -> `<Badge tone="warn">Stale models</Badge>`
     - Uninitialized -> `<Badge tone="neutral">Never loaded</Badge>`
     - Empty array (`[]`) -> `<Badge tone="neutral">No models</Badge>`
     - Undefined -> `<Badge tone="neutral">Unknown</Badge>`
     - Populated -> Comma-separated list of account models.
   - Never falls back to the global `gatewayModels` union.
4. **Lifecycle Feedback & Security Invariants**:
   - Manual token import: "Devin account saved and live in the runtime pool."
   - Manual token update (re-auth): "Devin account <label> updated and live in the runtime pool."
   - Host CLI import: "Devin CLI credentials imported from proxy host."
   - Host CLI re-import: "Devin CLI credentials re-imported for <label>."
   - Disable/Enable: "<label> disabled." / "<label> enabled."
   - Remove account: "Devin account <label> removed from the runtime pool."
   - Token inputs remain `type="password"` with no storage in `localStorage`/`sessionStorage` and no disclosure in error toasts or logs.
   - Quota capability remains `"unsupported"`, rendering "Not reported by provider" (never 0% or free).

---

## 2. Exact Backend Gateway Contracts

Audited against `mahoquot-proxy/crates/gateway/src/management/registry.rs` and `metrics.rs`:

| Endpoint | Method | Request Shape | Response Wire Shape | Frontend Consumer |
|---|---|---|---|---|
| `/v0/management/devin/models/status` | `GET` | (none) | `{ status: "ok", models: string[], accounts: [{ identity_slug: string, status: "active" \| "stale" \| "uninitialized", models: string[], stale: bool, last_refresh_at: u64 \| null, error: string \| null, disabled: bool }], generation: u64 }` | `clients.management.getDevinModelsStatus()` -> `devinStatusBySlug` |
| `/v0/management/devin/models/refresh` | `POST` | `?identity_slug=<slug>` or `?identity=<slug>` (or JSON body `{ "identity_slug": "<slug>" }`) | `{ status: "ok", outcome: "never" \| "success" \| "error", models: string[], error: string \| null, accounts: [{ identity_slug: string, status: "success" \| "error", models: string[], stale: bool, last_refresh_at: u64 \| null, error: string \| null }], generation: u64 }` | `clients.management.refreshDevinModels()` -> `refreshDevinDiscovery` |
| `/v0/management/devin/import-cli` | `POST` | `{ identity: string, label: string }` | `{ status: "ok", name: "devin-<slug>.json", error?: string }` | `clients.management.importDevinCli()` |
| `/v0/management/auth-files` | `POST` | `{ name: "devin-<slug>.json", content: DevinNormalizedCredential }` | `{ ok: true }` | `clients.management.importCredential()` |
| `/v0/management/auth-files/disabled` | `PUT` | `{ name: "devin-<slug>.json", disabled: boolean }` | `{ ok: true }` | `clients.management.setCredentialDisabled()` |
| `/v0/management/auth-files?name=...` | `DELETE` | `?name=devin-<slug>.json` | `{ ok: true }` | `clients.management.removeCredential()` |

---

## 3. Red-Green TDD Regression Evidence

### 3.1 RED State Output (Failing-First)
Before implementing the missing schema definitions, client methods, and per-account truth feedback, a regression test was created in `src/__tests__/devin-discovery-management.test.tsx` asserting `DevinModelsStatusResponseSchema.parse()`, `clients.management.getDevinModelsStatus()`, per-account error feedback over global success outcome, stale preservation feedback, and delete feedback.

```
 FAIL  src/__tests__/devin-discovery-management.test.tsx > Devin models status and refresh typed schemas > validates DevinModelsStatusResponseSchema matching backend GET /v0/management/devin/models/status
TypeError: Cannot read properties of undefined (reading 'parse')

 FAIL  src/__tests__/devin-discovery-management.test.tsx > Devin API client GET status integration > calls GET /v0/management/devin/models/status and parses typed response
TypeError: clients.management.getDevinModelsStatus is not a function

 FAIL  src/__tests__/devin-discovery-management.test.tsx > Devin discovery per-account truth feedback and lifecycle in App > reports per-account error truth on refresh even if top-level outcome claims success
TestingLibraryElementError: Unable to find an element with the text: /devin model discovery failed for devin failing: upstream connect timeout/i.

 FAIL  src/__tests__/devin-discovery-management.test.tsx > Devin discovery per-account truth feedback and lifecycle in App > reports stale models preserved on discovery refresh when account is stale
TestingLibraryElementError: Unable to find an element with the text: /devin models refreshed for devin stale \(stale models preserved\): 1 available/i.
```

### 3.2 GREEN State Output (Focused Devin Suites)
After implementing the minimal fixes across `src/lib/schemas.ts`, `src/lib/api.ts`, `src/App.tsx`, and `src/components/AccountsSurface.tsx`:

```
$ npx vitest run src/__tests__/devin-*

 RUN  v3.2.7 /Volumes/T9-Mac/project/mahoquot/crates/monitor-ui/frontend

 ✓ src/__tests__/devin-discovery-schemas.test.ts (5 tests) 5ms
 ✓ src/__tests__/devin-catalog.test.ts (9 tests) 4ms
 ✓ src/__tests__/devin-models.test.tsx (1 test) 44ms
 ✓ src/__tests__/devin-discovery-management.test.tsx (3 tests) 223ms
 ✓ src/__tests__/devin-onboarding-lifecycle.test.tsx (20 tests) 601ms

 Test Files  5 passed (5)
      Tests  38 passed (38)
   Duration  2.20s
```

---

## 4. Verification & Build Evidence

### 4.1 TypeScript Compiler (`tsc --noEmit`)
- **Command:** `cd /Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend && bun run typecheck`
- **Exit Code:** `0`
- **Output:** Clean, zero type errors.

### 4.2 Full Test Suite (`vitest run` / `bun run test`)
- **Command:** `cd /Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend && bun run test`
- **Exit Code:** `0`
- **Result:** **48 test files passed (100%), 410 tests passed (100%), 0 failures**.
- **Execution Time:** ~7.20s.

### 4.3 Unique `/tmp` Vite Build
- **Command:**
  ```bash
  TMP_BUILD_DIR=$(mktemp -d /tmp/mahoquot-devin-p6-verify-XXXXXX)
  cd /Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend
  bunx vite build --outDir "$TMP_BUILD_DIR"
  ```
- **Exit Code:** `0` (`FINAL_BUILD_EXIT_CODE=0`)
- **Output:**
  - `transforming... 1945 modules transformed.`
  - `[plugin vite:singlefile] Inlining: index-C5hi3l6c.js, style-CA-2q-pr.css`
  - Output file: `/tmp/mahoquot-devin-p6-verify-MAjgT8/index.html (878.05 kB | gzip: 305.24 kB)`
  - Cleanup: Temporary build directory unlinked.

### 4.4 Code Smells & Pure LOC Metrics
Measured via `awk '!/^[[:space:]]*$/ && !/^[[:space:]]*(\/\/|#|--)/' <file> | wc -l`:
- `src/__tests__/devin-discovery-schemas.test.ts`: **134 pure LOC** (<= 250 pure LOC ceiling)
- `src/__tests__/devin-discovery-management.test.tsx`: **230 pure LOC** (<= 250 pure LOC ceiling)
- `src/components/AccountsSurface.tsx`: **247 pure LOC** (<= 250 pure LOC ceiling)
- Pre-existing files modified with minimal non-invasive diffs; baseline formatting preserved; no blanket formatter applied.

---

## 5. Security and Operational Boundaries

1. **No Token Disclosure**: Password field masking, zero local storage persistence, zero token leaking in error payloads or action notices.
2. **No Proxy Bundle Overwrite**: `sync:proxy` script was NOT executed, keeping proxy bundle untouched for coordinated lead sync.
3. **No Unapproved Deploy / Commit / Credentials**: No git commits or branches created; zero live secrets used.
4. **Browser QA Handoff**: Visual browser QA at 375 / 768 / 1280px viewports is owned by the lead/browser worker node.

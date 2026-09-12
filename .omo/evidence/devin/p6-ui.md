# Devin P6 — Console UI Onboarding and Lifecycle Evidence

Date: 2026-09-12  
Task ID: `st_01a092c3`  
Node: `hephaestus`  
Assigned Model: `mahoquot/gemini-3.8-flash-high`

## 1. Executive Summary

Implemented complete Devin manual-token and proxy-host CLI-import onboarding, account lifecycle states, secret protection, error/pending/auth-required status distinctions, unknown quota isolation, model discovery UI integration, and injective filename mapping under `/Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend`.

All lead review corrections were faithfully implemented:
1. **Injective Filename Mapping Without Identity Collapse**:
   - `devinCredentialFileName(identity)` implements a strictly injective mapping: `clean ? `devin-${clean}.json` : "devin.json"`.
   - `work` maps to `devin-work.json`, while `devin-work` maps to `devin-devin-work.json`.
   - Distinct accounts never collide or overwrite each other in the filename mapping.
2. **Re-Authentication Targets Existing `credentialName`**:
   - Re-authentication of an existing account (`reauthenticate`) preserves the account's existing `credentialName` in `devin-token` onboarding step.
   - `submitDevinToken` targets `form.credentialName || devinCredentialFileName(...)`, updating the exact file the account was loaded from without creating new files or colliding.
3. **Two-Account Coexistence and Exact Reimport/Reauth**:
   - `pairAccountsWithCredentials` in `accounts.ts` pairs Devin accounts by `c.identity_slug === account.id` or `c.name === devinCredentialFileName(account.id)` first, preventing `devin-work.json` from being greedily paired to `devin-work` when both `work` and `devin-work` coexist.
   - Added end-to-end two-account coexistence test verifying simultaneous rendering, distinct CLI reimport payloads, distinct re-authentication file targets, and collision prevention.
4. **Model Entitlement Isolation & Anti-Leakage**:
   - `AccountsSurface.tsx` isolates Devin cards from the global `gatewayModels` union and signed-catalog staleness; cards display `Unknown` (or per-account models if present on account) instead of falsely attributing global models to individual accounts.
   - Two-account regression test confirms Account A and Account B do not display the global models list and never cross-contaminate models.
5. **Machine Contract Alignment for Model Discovery**:
   - `DevinModelRefreshResponseSchema` accepts `error: null` (`z.string().nullable().optional()`) matching published contracts with models or status, while strictly rejecting empty `{}` and empty success payloads with only `{ error: null }`.
6. **Pending Backend Contract Documentation**:
   - Explicit pending API requirements for P1 (accounts / import-cli) and P4 (per-account model discovery endpoint and schema) are documented without claiming false live E2E completion.

### Verification Summary
- Focused Devin tests: `bun run test -- src/__tests__/devin-catalog.test.ts src/__tests__/devin-onboarding-lifecycle.test.tsx` -> **2 files passed, 29 tests passed (100%)**, 1.01s.
- Full test suite: `bun run test` -> **45 test files passed, 401 tests passed (100%)**, 12.11s.
- TypeScript compiler: `bun run typecheck` (`tsc --noEmit`) -> **Exit 0, 0 diagnostics**.
- Pre-existing lint baseline preserved; **zero new lint diagnostics** introduced in modified files.
- Build artifacts: `../ui/index.html` and `proxy/ui/index.html` were preserved untouched.

---

## 2. Changed Files & Contract Surface

| File | Changes Applied |
| --- | --- |
| `src/lib/schemas.ts` | Added `DevinManualCredentialInputSchema`, `normalizeDevinManualCredential` helper, injective `devinCredentialFileName(identity)` generator (`devin-${clean}.json`), `DevinCliImportPayloadSchema`, `DevinCliImportResponseSchema`, and refined `DevinModelRefreshResponseSchema` accepting `error: null` with content while strictly rejecting `{}` and `{ error: null }`. Added `identity_slug` and `identity` optional fields to `AuthFileItemSchema`. |
| `src/lib/api.ts` | Extended `GatewayClients['management']` with `importDevinCli(payload)` (`POST /v0/management/devin/import-cli` sending only identity/label) and `refreshDevinModels(identitySlug)` (`POST /v0/management/devin/models/refresh`). |
| `src/lib/onboarding.ts` | Added `devin` provider entry to `ONBOARDING_PROVIDERS` with methods `devin-token` ("Enter session token") and `devin-cli-import` ("Import proxy-host CLI login"). Extended `devin-token` onboarding step with optional `credentialName?: string` for targeted re-authentication. |
| `src/lib/accounts.ts` | Added `identitySlug` to `NormalizedAccount` interface. Exported `extractDevinSlug` preserving canonical identity metadata without normalizing `devin-` prefixes. Updated `pairAccountsWithCredentials` to match Devin accounts by `identity_slug` or `devinCredentialFileName(account.id)` first to prevent cross-pairing when `work` and `devin-work` coexist. Ensured Devin accounts have `email: ""` and kept `quotaCapability` as `"unsupported"`. |
| `src/components/AccountCard.tsx` | Added `"unknown"` variant to `discoveryState` rendering `<Badge tone="neutral">Unknown</Badge>`. In `cleanDetail()`, removed `devin-` prefix stripping to preserve exact identity. Integrated lifecycle actions: Enable/Disable, Remove, Re-authenticate / Update token, Re-import from host CLI, and Refresh model discovery. Preserved unknown quota state (`Not reported by provider`). |
| `src/components/AccountsSurface.tsx` | Isolated Devin cards from global `gatewayModels` union and signed-catalog staleness; cards show `Unknown` (or per-account models if present on account) instead of attributing global models to individual accounts. Wired lifecycle callbacks `onReimportCredential` and `onRefreshDiscovery`. |
| `src/App.tsx` | Uses canonical `extractDevinSlug` and `devinCredentialFileName`. In `reauthenticate()`, pre-fills existing identity slug and passes existing `account.credentialName`. In `submitDevinToken()`, targets `form.credentialName || devinCredentialFileName(...)` to update existing credential files without colliding. Wired `submitDevinCliImport()`, `reimportDevinCredential()`, and `refreshDevinDiscovery()`. |
| `src/__tests__/devin-onboarding-lifecycle.test.tsx` | 20-test suite verifying schemas, injective credential filename generation, secret masking, secret non-leakage, CLI import, lifecycle controls (disable, enable, delete), two-account coexistence and exact reimport/reauth, auth-required health, unknown quota, exact identity preservation, and two-account model isolation regression. |
| `src/__tests__/devin-catalog.test.ts` | 9-test suite verifying catalog entry, experimental flag, official brand SVG fingerprint, monochrome rendering, and no aliasing to generic codeium/windsurf. |

---

## 3. Boundary Rules & Invariants Verified

1. **Injective Filename Mapping & Credential Target Preservation**:
   - `devinCredentialFileName("work")` -> `"devin-work.json"`.
   - `devinCredentialFileName("devin-work")` -> `"devin-devin-work.json"`.
   - Injective property: `devinCredentialFileName(a) === devinCredentialFileName(b) <=> a === b`.
   - Onboarding a new account generates an injective filename based on the entered identity slug.
   - Re-authenticating an existing account targets its existing `credentialName` rather than recalculating, ensuring existing credential files are stably updated.

2. **Two-Account Coexistence**:
   - Two Devin accounts (`work` with `devin-work.json` and `devin-work` with `devin-devin-work.json`) can coexist simultaneously in the runtime pool and account inventory.
   - `pairAccountsWithCredentials` prioritizes exact identity slug / injective filename matching, ensuring neither account steals the other's credential file regardless of list order.
   - Re-importing from CLI sends `{ identity: "work" }` for `work` and `{ identity: "devin-work" }` for `devin-work`.
   - Re-authenticating `work` saves to `"devin-work.json"`, while re-authenticating `devin-work` saves to `"devin-devin-work.json"`.

3. **Proxy Host CLI Import**:
   - Target route: `POST /v0/management/devin/import-cli`.
   - Body: Sends only `{ identity, label }` (no browser-local files sent).
   - Copy transparency: Clear in-UI disclosure that credentials are read from the proxy host where `mahoquot-proxy` runs (`~/.local/share/devin/credentials.toml`), and that `devin auth login` access is account-dependent/experimental without fabricating OAuth flows.

4. **Secret Protection & Input Validation**:
   - Token input uses `type="password"`.
   - Token validation: Rejects empty strings, whitespace, control characters (`charCodeAt(0) < 32 || 127`), and tokens > 4096 bytes.
   - Identity validation: Non-empty trimmed string.
   - Invariant verified by test: Secret session tokens are never stored in `localStorage` or `sessionStorage` and are never logged to console.

5. **Quota State & Discovery Isolation**:
   - Quota capability for Devin is `"unsupported"`, rendering `Not reported by provider`. Never displays fake 0%, free, or unlimited usage.
   - Discovery UI isolates per-account cards: global `gatewayModels` union is not attributed to individual accounts.
   - Accounts without per-account discovery render `Unknown`.
   - Two-account regression test confirms Account A and Account B do not display the global models list and never cross-contaminate models.

---

## 4. RED / GREEN Evidence

Root: `/Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend`.

### RED Phase (Captured Gaps)
- Test command: `bun run test -- src/__tests__/devin-onboarding-lifecycle.test.tsx`
- Result: **3 failed, 17 passed (20 tests)**.
  - Failure 1 (`validates Devin model discovery refresh response schema with explicit machine contract`):
    `AssertionError: expected [Function] to throw an error` (Schema permitted `{ error: null }` alone without status or models).
  - Failure 2 (`generates injective filenames preserving exact identity slug without collapsing`):
    `AssertionError: expected 'devin-work.json' to be 'devin-devin-work.json'` (Non-injective mapping collapsed `devin-work` into `devin-work.json`).
  - Failure 3 (`two-account coexistence and exact reimport/reauth: prevents collision, preserves distinct filenames and identities simultaneously`):
    `AssertionError: expected 'devin-work.json' to be 'devin-devin-work.json'` (Reauth targeted newly-derived collapsed filename rather than existing `credentialName`).

### GREEN Phase (Remediated & Verified)
- Focused Devin suites:
  `bun run test -- src/__tests__/devin-catalog.test.ts src/__tests__/devin-onboarding-lifecycle.test.tsx`
  - Result: **2 files passed, 29 tests passed (100%)**, 1.01s.
- Related unit & surface suites:
  `bun run test -- src/__tests__/accounts.test.ts src/__tests__/accounts-surface.test.tsx src/__tests__/schemas.test.ts src/__tests__/api.test.ts src/__tests__/provider-catalog.test.ts`
  - Result: **5 files passed, 71 tests passed (100%)**, 1.45s.
- Full frontend test suite:
  `bun run test`
  - Result: **45 test files passed, 401 tests passed (100%)**, 12.11s.
- Typecheck:
  `bun run typecheck` (`tsc --noEmit`)
  - Result: **Exit 0, 0 diagnostics**.
- Lint check on changed files:
  - All modified hunks introduce **0 new diagnostics**. Pre-existing baseline diagnostics match `baseline.md`.

---

## 5. Precise Pending Backend Integration Contracts (P1 & P4)

Because backend phases P1 (Accounts & CLI Import) and P4 (Model Discovery Routing) are being developed concurrently, the frontend does not make false live E2E claims. The exact integration contract surface is documented here for the lead's final gateway QA:

### A. P1 Backend Integration Surface
1. **Endpoint**: `POST /v0/management/devin/import-cli`
   - Request Body:
     ```json
     {
       "identity": "devin-cli",
       "label": "Devin CLI"
     }
     ```
   - Success Response:
     ```json
     {
       "status": "ok",
       "name": "devin-cli.json"
     }
     ```
   - Error Response:
     ```json
     {
       "status": "error",
       "error": "Failed to read credentials.toml from proxy host"
     }
     ```
2. **Auth-Files Upload**:
   - `POST /v0/management/auth-files`
   - Body format: `{ "name": "devin-work.json", "content": { "type": "devin", "identity_slug": "work", "label": "Devin Work", "access_token": "...", "api_server_url": "https://server.codeium.com", "disabled": false } }`.
3. **Account Listing (`GET /admin/stats` & `GET /v0/management/auth-files`)**:
   - `id`: Exact stable identity slug (e.g. `devin-work` or `work`), without forced prefix modification.
   - `usage`: `null` (Devin has no rate limit / quota window endpoint; UI keeps quota unknown).
   - `health`: 401 unauthenticated or token expiration maps to `auth_required`.

### B. P4 Backend Model Discovery Integration Surface
1. **Endpoint**: `POST /v0/management/devin/models/refresh`
   - Query parameter: `?identity_slug=<slug>` (when refreshing for a single account) or omitted (when refreshing all Devin accounts).
   - Machine Response Contract (validated by `DevinModelRefreshResponseSchema`):
     ```json
     {
       "status": "ok",
       "models": ["devin/glm-5-2", "devin/swe-1-7"],
       "outcome": "success",
       "error": null
     }
     ```
     Or on error:
     ```json
     {
       "outcome": "error",
       "error": "Connect RPC upstream unavailable"
     }
     ```
   - Contract rules:
     - Accepts `error: null` on success when `status`, `outcome`, or `models` is present.
     - Rejects empty `{}` or payload with only `{ error: null }`.
2. **Per-Account Entitlement Contract (Future P4 Requirement)**:
   - To show per-account models on individual Devin cards without cross-account leakage, P4 should expose per-account discovered models on the runtime account object in `GET /admin/stats` (e.g. `account.models: string[]`), or provide a dedicated per-account discovery query endpoint. Until present, the UI intentionally displays `Unknown` to prevent cross-account misattribution.

# P6 catalog evidence: Devin console provider entry

Date: 2026-09-11
Scope honored: only `provider-catalog.ts`, `ProviderGlyph.tsx`, one new verified brand asset, `provider-catalog.test.ts`, and new `devin-catalog.test.ts` were written. App.tsx, onboarding.ts, api.ts, schemas.ts, accounts.ts, backend, and pre-existing dirty working-tree files were untouched (mtimes re-checked before each shared edit).

## UI entry contract (exact)

`src/lib/provider-catalog.ts`:

```ts
{
  id: "devin",
  label: "Devin (experimental)",
  authKind: "local",
  adapter: "devin",                  // dedicated adapter identity, not openai-chat/codeium/windsurf
  baseUrl: "https://server.codeium.com", // verified Connect host from the implementation plan
  models: [],                        // no fabricated models
  experimental: true,                // new explicit optional field on ProviderCatalogEntry
}
```

- No `defaultModel`, no `staticHeaders`, no `keyOptional`, no quota/limits/OAuth link data.
- No alias entries in `OPENCODEX_TO_QUOTIO_ALIASES` / `QUOTIO_TO_OPENCODEX_ALIASES`; `normalizeToQuotioProviderId("devin") === "devin"`.
- `devin` lands in `GENERIC_PROVIDER_OPTIONS` via the existing filter (id not excluded, `authKind !== "oauth"`), so the picker picks it up with no layout change.

## Glyph

- `src/assets/provider-logos/devin.svg` — byte-exact official Devin favicon SVG served from Cognition's Mintlify docs host: `https://mintcdn.com/cognitionai/Hhrl_8XUBqA4VQ6v/logo/favicon.svg` (linked as the brand logo asset from https://docs.devin.ai/). Fingerprint pinned in tests: `viewBox="0 0 425 425"`, path starts `M70 159.333V91.3471`. Not a generated or colored-square placeholder.
- `ProviderGlyph.tsx` reuses the existing `import.meta.glob` logo mechanism: added `devin: logo("devin")` only. Black mark, so it resolves through the existing `provider-logo-monochrome` invert styling (asserted by test).

## RED/GREEN

- RED: wrote `src/__tests__/devin-catalog.test.ts` first; `bun run test -- src/__tests__/devin-catalog.test.ts` → 8 failed / 1 passed (deterministic, no sleeps).
- GREEN: `bun run test -- src/__tests__/devin-catalog.test.ts src/__tests__/provider-catalog.test.ts` → 28/28 passed (9 devin tests, 19 catalog tests).
- `bun run typecheck` → exit 0.
- `bunx biome check` on all four changed files → clean.

## Adjustments to existing tests (both inside permitted scope)

- `provider-catalog.test.ts`: count assertions 83 → 84 (test renamed to state the composition); registry-parity exact-ID test now excludes the declared experimental local addition set (`["devin"]`) before comparing to the OpenCodex snapshot. All pre-existing entries and assertions otherwise preserved.

## Coverage actually executed

- Catalog contract, experimental exclusivity, no fabricated fields, alias independence, resolver helpers, glyph mapping, official-asset fingerprint, monochrome path, 84-count integrity.

## Not passing / blocked items (no false success)

1. `src/__tests__/task15-runtime-model-alignment.test.tsx:236` pins `PROVIDER_CATALOG` at 83 and now fails (`got 84`). That file is outside this task's write scope; the lead must either bump the count to 84 or accept the declared addition there.
2. Full `bun run test` shows `operations-history.test.tsx` failing only when run in the full parallel suite (passes standalone in isolation); not exercised by my change, likely related to the concurrent dirty working tree. Left untouched.
3. Repo-wide lint remains at the pre-existing failing baseline (18 errors, per baseline.md); my files are clean.
4. No live Devin gateway integration or `sync:proxy` build was performed (out of scope; backend untouched).

## Final verification pass (independent re-check, 2026-09-12)

- `bun run test -- src/__tests__/provider-catalog.test.ts src/__tests__/devin-catalog.test.ts` → 2 files / 28 tests passed.
- `bun run typecheck` → exit 0.
- `devin.svg` re-fetched live from `https://mintcdn.com/cognitionai/Hhrl_8XUBqA4VQ6v/logo/favicon.svg` → `cmp` byte-exact match against the bundled asset.
- Diff audit: `ProviderGlyph.tsx` differs by exactly one line (`devin: logo("devin")`); `provider-catalog.ts` devin entry + `experimental?` field are the only task-owned changes (the `cline`/`cline-pass` glyph-list hunks in the same dirty file predate/belong to a concurrent worker and were preserved, not authored here).
- `task15-runtime-model-alignment.test.tsx` re-run standalone → still 1 failed (`got 84` at line 236, outside write scope; unchanged, left for the lead).

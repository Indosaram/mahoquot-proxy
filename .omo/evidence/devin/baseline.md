# Devin pre-implementation baseline

Captured before any Devin production Rust or frontend changes.

## Rust

Command: `cargo test -p mahoquot-registry -p mahoquot-providers -p mahoquot-gateway --lib`

Result: exit 101. Gateway library tests do not compile because five existing `GenericAccount` initializers omit `email`:

- `crates/gateway/src/account.rs:256`
- `crates/gateway/src/account.rs:279`
- `crates/gateway/src/models_route.rs:608`
- `crates/gateway/src/models_route.rs:630`
- `crates/gateway/src/models_route.rs:657`

Exact diagnostic: `error[E0063]: missing field `email` in initializer of `account::GenericAccount``.
LSP error diagnostics on `relay.rs` reported none; this does not supersede the compiler result.
Monitor `mon_EK8DGPMVNNQ9H5MH`, bash session `bash_1`.

## Console

Root: `/Users/indo/code/project/quotio-rs/crates/monitor-ui/frontend`.

- `bun run typecheck`: exit 0.
- `bun run lint`: exit 1, 18 errors across 133 checked files; no fixes applied.
- `bun run test`: exit 0, 43 files and 372 tests passed, 21.31 seconds.
- Existing React `act(...)` warnings occurred in `app.test.tsx`.

Monitor `mon_TDHVVVCWY178ENMH`, bash session `bash_2`.
Final sentinel: `BASELINE_UI typecheck=0 lint=1 tests=0`.

The initial console output dropped the lint details, so the fast read-only lint command was rerun once to recover the exact failures:

1. `src/__tests__/native-focus.test.ts`: format.
2. `src/__tests__/overview-token-usage.test.tsx`: format.
3. `src/__tests__/zcode-auth-response.test.ts`: format.
4. `src/__tests__/use-gateway-polling.test.tsx`: format.
5. `src/App.tsx:91`: `lint/style/useImportType`.
6. `src/App.tsx:1081`: `lint/correctness/useExhaustiveDependencies`, missing `setNotice`.
7. `src/components/OverviewTokenUsage.tsx:116`: `lint/style/noNonNullAssertion`.
8. `src/components/OverviewTokenUsage.tsx:118`: `lint/style/noNonNullAssertion`.
9. `src/components/OverviewTokenUsage.tsx:122`: `lint/style/noNonNullAssertion`.
10. `src/components/OverviewTokenUsage.tsx:124`: `lint/style/noNonNullAssertion`.
11. `src/components/OverviewTokenUsage.tsx`: format.
12. `src/App.tsx`: organizeImports.
13. `src/__tests__/telemetry.test.ts`: format; Biome abbreviated its proposed formatting diff by 25 lines.
14. `src/components/OverviewDashboard.tsx`: format.
15. `src/App.tsx`: format.
16. `src/hooks/useGatewayPolling.ts:81`: `lint/correctness/useExhaustiveDependencies`, unnecessary `clients`.
17. `src/lib/accounts.ts:309`: `lint/complexity/useOptionalChain`.
18. `src/lib/telemetry.ts`: format.

These failures are a comparison baseline, not permission to reformat unrelated files or weaken tests.

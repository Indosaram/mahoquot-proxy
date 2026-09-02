# CONTRACTS

The parity policy and desktop contract are owned by sibling `quotio-rs/docs/CONTRACTS.md`. This repository owns the gateway management schema at `docs/management-contract-v1.schema.json` and the matching request and response examples at `docs/examples/management-contract-v1.json`. Both are version 1 and pinned to the approved Quotio v0.7.7 commit `21d75d08b38a23f97fbd534a1768a829cb147f2c`.

## Gateway-owned contracts

Scheduling is a transient overlay and never rewrites credential JSON. Manual disable remains persistent. The quota scheduler uses 3 percent exhaustion entry, 5 percent recovery, a 10-minute minimum hold, a 15-minute switch margin, manual priority, auth isolation, and three consecutive non-auth failures. Invalid state, no eligible target, and all exhausted candidates fail open to the configured base strategy.

Request history uses SQLite WAL mode and bounded asynchronous ingestion. Event IDs are idempotent. Queries support arbitrary ranges, account, provider, model, inbound-key label, and status filters, plus minute, hour, and day buckets. Raw keys and secrets aren't stored. Queue overflow and database faults are explicit degraded states and don't block relay streaming. Retention and size limits prune in bounded chunks.

Reset retries are idempotent. After token refresh, at most one retry is allowed and it reuses the same `redeem-request-id`. Unsupported provider, no credit, auth failure, network failure, and upstream rejection remain distinct non-2xx errors. Price rows are versioned, and every estimated cost identifies its price version.

Graceful shutdown is available only to an authenticated owner. It stops acceptance, drains in-flight work, flushes history and telemetry, then exits within the supplied deadline. A desktop may invoke it only for a gateway process it owns. Remote gateway profiles can't be stopped or updated.

## Shared management surface

The canonical future route owner is `crates/gateway/src/management/contracts.rs`. Each route listed in `x-route-registration-owners` in the schema has exactly one owner. Implementations must not register the same method and path in another group.

The contract covers scheduler, history, reset, graceful shutdown, pricing, CLI agent configuration, TOTP storage state, cloudflared tunnel ownership, Codex launcher binding, signed updater compatibility, and native platform parity. Desktop-only operations are described here because the management schema is the shared type boundary, but their native execution remains in `quotio-rs`.

Errors use this envelope:

```json
{"error":{"code":"history_bucket_invalid","message":"time-bucket must be minute, hour, or day","retryable":false,"field":"time-bucket"}}
```

Status semantics are 400 for malformed or invalid fields, 401 for missing management authentication, 403 for forbidden operations, 404 for unknown resources, 409 for ownership or hash conflicts, 422 for an unsupported provider or profile operation, 503 for unavailable credential store, history, tunnel, or updater services, and 504 for a bounded lifecycle timeout. `error.code` is stable and machine-readable. `retryable` states whether the same request can be tried again without user changes.

Provider credentials remain exclusively under `~/.mahoquot/auth`. Desktop management secrets and TOTP material may use native credential stores, but there is no plaintext fallback. Verification uses local mocks only, with no live Codex or Claude model calls.

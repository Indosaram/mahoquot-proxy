# L4 auth repair

## Implemented
- **L4-04** — `crates/gateway/src/management/creds.rs:507-538,1919-1934`: duplicate identity now prefers account_id, project_id, identity_slug, then normalized email; opaque IDs retain case and identity field namespaces prevent fallback collisions. Added `codex_import_keys_on_account_id` (distinct same-email accounts, duplicate account with changed email, case-distinct IDs, email fallback).
- **L4-05** — `crates/gateway/src/management/oauth.rs:424,1193-1238,3078-3090`: Antigravity reauthentication merges only issued credential/expiry fields into latest metadata; disabled, label and routing metadata are retained. Empty issued refresh token retains the existing one. Added `reauth_preserves_operator_disabled`, including concurrent grant-conflict rejection.
- **L4-06** — `crates/gateway/src/management/oauth.rs:188-204,393-424,1167-1269,3092-3126`: every production OAuth credential persistence path now takes composition_lock, including local storage. Antigravity re-reads target inside lock and compares identity/access/refresh generation before merge; named reauth captures expected generation before provider dispatch. Added `oauth_write_takes_composition_lock`, an event-coordinated local-file test retaining a metadata edit made while the coordinator is held. No sleeps or real provider traffic.

## Not implemented: ownership blockers
- **L4-01** — requires `pre_expiry_refresh.rs` and `account.rs`, neither owned by this lane. Generation-scoped rejected refresh tracking must be installed at actual refresh/selection boundaries; an uncalled policy helper would not fix it.
- **L4-02** — shared classification belongs in `crates/providers/src/refresh_exec.rs`, with proactive caller in `pre_expiry_refresh.rs` and reactive caller in forbidden `relay.rs`. None owned. No fake `recoverable_402_does_not_disable_credential` test added.
- **L4-03** — RefreshError Display/Debug is in non-owned `crates/providers/src/refresh_exec.rs`. No superficial logging-only workaround added.
- **L4-07** — actual pre-dispatch admission and receipt recovery require non-owned `account.rs` and authoritative credential-store boundaries. Management-only claims cannot prevent refresh replay. No unintegrated claim helper/test added.
- **L4-08** — lane report shows WRR is absent from the router, not merely stale management state. Actual selection requires non-owned `crates/router/src/lib.rs` and runtime snapshot publication code. I did not replace the explicit request with WRR rejection or claim a management-only distribution fix.

## Existing tests / verification
- Existing test expectations changed: none; no old-behavior assertions found in touched modules.
- No cargo, tests, builds, real provider requests, commit/stash/checkout run, as instructed. Parent must execute final verification.
- `rustfmt --edition 2021` parsed both changed modules; final OAuth parse checked with `--emit stdout` (no file rewrite). Scoped `git diff --check` passed.
- Unscoped `git diff --check` reports pre-existing CRLF/trailing-whitespace changes in `.omo/evidence/model-registry/task-11-routing.http` and `task-12-model-apis.http`, which I did not touch.
- Concurrent unrelated Command Code edits appeared in oauth.rs while this lane worked; retained intact, not claimed as this lane's changes.

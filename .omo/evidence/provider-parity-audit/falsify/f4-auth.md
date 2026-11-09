# F4: auth falsification review

Source-only review; no cargo, tests, provider calls, or product edits. O = mahoquot-proxy; A = opencodex; B = cliproxyapi at the task's paths. Read the lane, register, each cited implementation in O and A/B, and O's deviation files. All eight failures survive, with qualifications below. Proposed tests/patches are not executed or applied.

## L4-01
**CONFIRMED** - An enabled account with an access token expiring in one hour and a rejected Codex refresh grant (400 invalid_grant), scanned with the default two-hour lead, becomes AuthFailed immediately and cannot serve its still-valid token.

Disproof attempted: expiry gate, stale-token guard, and terminal-retry suppression. O/pre_expiry_refresh.rs:15,92-113 intentionally bypasses actual expiry with the presented-token argument; :124-133 benches immediately. O/account.rs:2455-2463 only prevents stale generations, not this first rejection; :1340-1345 exposes the resulting health to selection. O/router/src/lib.rs:158-160 excludes unavailable health. B/sdk/cliproxy/auth/conductor_refresh.go:1493-1497,1578-1599 distinguishes access rejection from grant rejection and retains inference until access expiry. That is superior for a client with working access credentials. O's comments describe benching but do not document a deliberate parity/availability tradeoff; neither deviation document declares it.

**MINIMAL PATCH:** In AccountMember add generation-scoped rejected-refresh state (shared across rescans, like refresh_lock). In `refresh_expiring`, record the rejected grant instead of setting AuthFailed when access is unexpired; `is_refreshable` must skip that unchanged grant. In `refresh_with_store`, reject reuse of that marked grant and only bench once access expires (or inference actually returns 401). Clear the marker when the grant changes. Do not merely remove the health assignment: that would replay a dead rotating grant every scan.

**Test:** `proactive_rejected_grant_keeps_unexpired_access`: inject time; scan a token expiring at now+3600 with terminal refresh; assert selectable, one refresh dispatch across repeated scans, then unavailable at expiry without another dispatch.

## L4-02
**CONFIRMED** - An expired account whose token endpoint temporarily returns HTTP 402, or an unexpired account whose proactive refresh returns 400 invalid_request, is marked AuthFailed and excluded after the endpoint recovers.

Disproof attempted: serving versus background caller and status normalization. O/providers/src/refresh_exec.rs:27-35 classifies all 400..=403; O/gateway/src/relay.rs:5946-5964 actually consumes that classification. Background has its own narrower status test but still accepts every 400 (pre_expiry_refresh.rs:63-78,124-133). There is no body-code guard. B/sdk/cliproxy/auth/conductor_cooldown.go:2352-2391 uses terminal error evidence and Codex-specific invalid_grant, not blanket 400-403; conductor_refresh.go:1551-1577 schedules transient failures. B is superior for recoverable endpoint failures.

**MINIMAL PATCH:** Replace both classifiers with one shared predicate in `RefreshError::is_auth_failure`; have `is_grant_rejection` delegate to it. Keep 401 terminal; for other statuses parse structured OAuth error codes and recognize terminal grant evidence, not arbitrary text or the HTTP range. A short baseline for the existing enum is:
```rust
match self {
    RefreshError::Status { code: 401, .. } => true,
    RefreshError::Status { code: 400, body } => {
        let value = serde_json::from_str::<Value>(body).ok();
        matches!(value.as_ref().and_then(|v| v.get("error"))
            .and_then(Value::as_str),
            Some("invalid_grant" | "invalid_client"))
    }
    _ => false,
}
```
Retain transient failover/backoff; provider-specific nested error shapes can be handled at that provider's boundary rather than treating all 403 as auth failure.

**Test:** `refresh_terminal_classification_preserves_transient_accounts`: table 400 invalid_grant/invalid_request, 401, 402, WAF 403, 429, 500; assert only explicit terminal cases bench and recovered transient credentials remain eligible.

## L4-03
**CONFIRMED** - A token endpoint returning HTTP 400 with body `{"error":"invalid_grant","refresh_token":"SECRET_SENTINEL"}` causes the sentinel to be written in the proactive warning.

Disproof attempted: token Debug redaction and downstream logging sanitization. O/providers/src/refresh_exec.rs:19-20 interpolates `body` in Display; :150-162 preserves the entire response. O/pre_expiry_refresh.rs:129-143 logs `%error`; main.rs:527-530 installs the ordinary fmt subscriber, not a redacting field layer. Tokens Debug is not on this path. B/sdk/cliproxy/auth/conductor_cooldown.go:2345-2350 redacts before truncation, and :2417-2454 sanitizes the error before conductor logging; B/internal/auth/codex/refresh_journal.go:28-36 also avoids secret-bearing cause text. B is superior for diagnostic confidentiality.

**MINIMAL PATCH:** In `RefreshError`, remove upstream body from Display AND derived Debug (otherwise a future `?error` restores the leak); retain the private raw body only for classification. Emit status and an allowlisted safe OAuth code. For the status variant, changing its error attribute to `#[error("status {code}: token endpoint rejected refresh")]` removes the demonstrated Display leak. Implement diagnostic-safe Debug and format Http with `without_url()` rather than exposing token-bearing URLs; do not log raw bodies elsewhere.

**Test:** `refresh_error_diagnostics_do_not_expose_secrets`: mock 400/500 bodies containing bearer, access/refresh tokens and token query sentinels; capture the real warning/debug formatter and format both Display/Debug; assert all sentinels absent and HTTP status present.

## L4-04
**CONFIRMED** - Importing two differently named Codex files with email `u@example.com` and distinct account_id values `personal` and `team` returns 409 duplicate_credential instead of accepting both.

Disproof attempted: filename uniqueness and provider normalization. O/management/creds.rs:510-530 explicitly chooses email first; O/management/accounts.rs:669-685 separately checks filenames and then rejects the colliding identity, including within one batch. B/internal/auth/codex/filename.go:9-26 explicitly includes account hash to keep same-email accounts distinct. Distinct workspaces are legitimate capacity; the reference is superior. The O comment describes the heuristic, not a declared decision to prohibit multiple Codex workspaces.

**MINIMAL PATCH:** In `credential_identity`, use provider-specific strong identity first. For Codex use trimmed account_id without lowercasing its opaque value, falling back to normalized email; use generic identity_slug before email and Vertex project identity before email. Keep provider normalization. A Codex branch before the fallback is sufficient for the concrete Codex failure; do not lower-case all identifier types.

**Test:** `import_codex_same_email_distinct_account_ids`: real import accepts both files (201); importing another filename with either same account_id returns 409. Assert case-distinct opaque IDs are not collapsed.

## L4-05
**CONFIRMED** - Completing same-account Antigravity reauthentication over an existing disabled:true credential persists disabled:false and enables it on rescan.

Disproof attempted: preserving old object, session validation, and reference explicitly enabling on login. O/management/oauth.rs:367-376 creates disabled:false, and :419-436 extends old with all new keys; the preservation merge thus overwrites the operator flag. The session check at :1207-1224 only validates session ownership/TTL. B/internal/api/handlers/management/auth_files_provider_oauth.go:857-884 omits disabled from fresh metadata; auth_files_fields.go:995-998,1018-1022 calls the merge; B/sdk/cliproxy/auth/metadata_merge.go:33-36 preserves the old flag. B's policy is superior because login is not an enable operation. The lane's cited relogin test lines 183-199 are Claude setup, not an Antigravity proof; the actual Antigravity caller above establishes it.

**MINIMAL PATCH:** In `exchange_antigravity_code`, before extending an existing object, remove the default operator flag from the new map:
```rust
credential.as_object_mut().unwrap().remove("disabled");
```
Do this only on the existing-file branch, retaining disabled:false for new onboarding. L4-06 still requires an under-lock latest-state merge to handle concurrent changes.

**Test:** `antigravity_reauth_preserves_operator_disable`: complete a real mocked same-email exchange over disabled:true; assert rotated tokens, disabled:true on disk, and disabled health after rescan.

## L4-06
**CONFIRMED** - After Antigravity OAuth reads an existing credential, a successful management label edit followed by OAuth persistence restores the old label, losing an acknowledged edit.

Disproof attempted: shared coordinator and late CAS. O/management/oauth.rs:389-436 reads/merges before persistence; :1189-1204 bypasses composition_lock for local files, and the backed path locks only around writing already-rendered bytes. O/management/creds.rs:807-814 locks management edits, so it cannot coordinate with the unlocked local write or repair a stale pre-lock merge. A/src/oauth/store.ts:670-677 acquires its file lock before reading and mutating the current store; that pattern is superior for concurrent writes. B/auth_files_fields.go:956-998 re-reads existing metadata but alone does not prove a serialized merge, so it is not sufficient evidence of concurrency superiority.

Important correction to lane scope: O/account.rs:2659-2677 does NOT generally bypass the coordinator in production. O/state.rs:1459-1467 passes Some(settings), and commit_refresh locks before re-reading and merging, preserving cosmetic edits (:2707-2745). L4-06 survives on OAuth persistence, not on that normal refresh caller.

**MINIMAL PATCH:** In `exchange_antigravity_code`, capture expected identity/grant before provider dispatch; replace the existing rendered-byte save with an OAuth-specific coordinated commit. Inside `block_in_place`, always acquire composition_lock (local and backed), re-read target, check expected type/email/grant or expected absence, and merge ONLY issued token/project/expiry fields into current metadata before `write_session_credential_with` and `persist_credential`. Keep the session-active check. Merely adding a lock around the existing byte write does not fix the lost update.

**Test:** `antigravity_reauth_merges_concurrent_management_edit`: event barrier after old read; perform actual label/status patch and await its success; release OAuth; assert latest label/disabled plus new tokens, or explicit generation conflict with no overwrite. No sleeps.

## L4-07
**CONFIRMED** - Two gateway processes sharing one Codex credential both read refresh grant R and dispatch it before either commits; a rotating issuer consumes R once and treats the second dispatch as reuse, potentially revoking the newly issued family.

Disproof attempted: shared member mutex, pre-dispatch disk generation check, post-dispatch CAS, and a process-wide/durable admission guard. O/account.rs:2444-2463 serializes only Arc<tokio::sync::Mutex>; :2560-2584 compares disk before dispatch but does not claim it; :2632-2645 persists only after response. commit_refresh's :2665-2705 lock/CAS is process-local and too late to prevent the second HTTP request. No durable claim or single-instance guard was found in repository Rust source. A/store.ts:183-205 refuses to overwrite an existing intent; B/internal/auth/codex/refresh_journal.go:16-26,72-103 claims before `run`, retains unknown outcomes, and recovers receipts. B/sdk/auth/refresh_journal.go:13-37 binds its file journal to the authoritative directory. This is not just B's in-memory fallback. The reference is superior for rotation safety and response-loss recovery.

**MINIMAL PATCH:** At the Codex dispatch in `AccountMember::refresh_with_store`, add a durable claim keyed by SHA-256(client_id || NUL || refresh_token), using atomic create_new in the authoritative credential store (including backed-store claim semantics). Before execute_refresh_spec, persist/sync the claim; existing pending/unknown claim must return a safe outcome-unknown error without dispatch. Persist a protected successful token receipt before commit_refresh; subsequent callers recover that receipt without resending the grant. Never delete pending claims on timeout/crash, and never store the token in a filename. This requires a small claim/receipt store boundary; a few-line mutex or commit CAS cannot safely fix it. Keep the existing in-process mutex and merge guard.

**Test:** `codex_refresh_claim_survives_process_and_response_loss`: two subprocesses coordinate by explicit IPC barriers over one credential; assert mock issuer sees exactly one R. Kill claimant after mock consumption, restart, assert zero additional R requests; with saved receipt assert credential reconciliation without network dispatch. Also assert claim/store write failure prevents dispatch.

## L4-08
**CONFIRMED** - Starting from fill-first, PUT /routing/strategy with {"value":"wrr"} returns 200 and GET reports weighted-round-robin, but subsequent non-affine selections still always pick the first available member.

Disproof attempted: settings validation, aliases, alternate selection path. O/management/scalar_table.rs:64-83,211-230 accepts WRR but cannot parse it into Strategy; management/settings.rs:928-939 validates using that accepting normalizer; scalars.rs:77-91 consequently makes no runtime update. O/router/src/lib.rs:27-38,129-194 only implements RR and fill-first. B/sdk/cliproxy/auth/selector.go:671-714 actually filters positive weights and invokes smooth weighted selection; B/internal/credentialweight/weight.go:12-28 defines zero-weight exclusion. Honest rejection is minimally correct; B's implemented distribution is superior when WRR is requested. No documented WRR deviation exists.

**MINIMAL PATCH:** Until actual weighted scheduling exists, reject WRR in `normalize_routing_strategy` (remove its WRR match arm). This makes scalar writes and config validation refuse before persistence and keeps GET from claiming support. Do not silently map WRR to ordinary RR.

**Test:** `routing_wrr_is_rejected_without_mutation`: real PUT wrr returns 400; persisted config and Router::strategy remain FillFirst; GET remains fill-first. Assert startup/settings validation rejects configured weighted-round-robin. No implementation/distribution test is warranted until WRR is implemented.

## New high-severity defects

None established beyond these eight. An apparent stale access-token capture in relay.rs:5944-5986 was not promoted: send_upstream at :1748-1752 normally rebuilds headers from the refreshed member, disproving that candidate for ordinary providers.

## Verification boundary

All verdicts are source-proven concrete scenarios, not live reproductions. Read O/docs/reference/provider-parity-deviations.json in full (only Kiro auth-kind and Command Code label entries) and O/docs/reference-parity.md (pointer only); neither declares these failures. Product source remains unchanged. This report is the only authored artifact.

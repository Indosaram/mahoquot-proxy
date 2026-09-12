# P7: Devin E2E relay defects

## Outcome and scope

Fixed credential endpoint resolution in `crates/gateway/src/account.rs:1713-1723`: explicit `upstream_override` wins; Devin otherwise uses `api_server_url`; Generic retains `base_url`; other providers retain their previous behavior.

The apparent Messages/Gemini model-selection defect is an account-health consequence of the wrong-endpoint 401, not a model-normalization discrepancy. Healthy discovered accounts select correctly through both real routes even before the loader fix when the fixture supplies an explicit endpoint. After the loader fix, native-endpoint-only credentials also reach the local mock through both routes.

No production discovery/P4 logic, relay normalization, health policy, or `PoolSnapshot` Debug implementation was changed. No commits, deployment, restarts, stack mutations, live credentials, model fallback, or delegation. The running E2E stack was only inspected with the GET requests explicitly authorized by the lead. The existing process remains unfixed in memory; this task does not claim a successful rerun or deployment of the running E2E stack.

## Root causes and path comparison

### 1. Wrong upstream

- `account.rs:1713-1723` previously read `upstream_override` and Generic `base_url`, but ignored Devin `api_server_url`.
- `account.rs:937-943` discovery obtains the effective base from the Devin account when no override exists, explaining successful discovery.
- `relay.rs:651-656` sends `member.upstream_override` to `url::build_provider_url` for GetChatMessage.
- `url.rs:41-43` falls back to `DEVIN_DEFAULT_API_SERVER_URL` when that override is absent.
- The actual compiled default observed by the red test is `https://server.codeium.com`, not `api.devin.ai` as initially described by the lead. No test sent a token or request to that external endpoint.

### 2. Later Messages/Gemini 503s

The account-selection model remains `devin/glm-5-2` across all four surfaces:

| Surface | Model path |
| --- | --- |
| Chat | `compat/request.rs:23-28` preserves the input model; `relay.rs:1339` uses `translated.model`. |
| Responses | `relay.rs:1322` extracts the original model. |
| Messages | `compat/claude.rs:128-133` reads the input model unchanged; `relay.rs:1283` uses the translated model. |
| Gemini | `cp_routes.rs:761` inserts the full path model into the body; `relay.rs:1298` extracts it. |

`relay.rs:1395-1416` checks the requested, canonical, and upstream IDs against Devin permissions. `runtime_state.rs:180-190` accepts prefixed and bare Devin IDs. Crucially, `relay.rs:1576` rejects unavailable health BEFORE `account_declares_binding_model`. Connect `unauthenticated` sets `AuthFailed` at `relay.rs:2744`; HTTP 401 does so at `relay.rs:2837`. Later requests therefore return 503 without needing a model mismatch.

Read-only live evidence, collected after lead authorization:

```sh
curl --max-time 10 -sS -H 'Authorization: Bearer devin-dummy-mgmt-key' \
  http://127.0.0.1:55149/v0/management/devin/models/status
curl --max-time 10 -sS -H 'Authorization: Bearer devin-dummy-mgmt-key' \
  http://127.0.0.1:55149/admin/stats
```

Both exited 0. Relevant returned machine fields:

```json
{"generation":4,"status":"ok","models":["devin/glm-5-2","devin/swe-1-7"],"accounts":[{"identity_slug":"devin-lead-qa","disabled":false,"error":null,"status":"stale","stale":true,"models":["devin/glm-5-2","devin/swe-1-7"]}]}
```

The `/admin/stats` account contained:

```json
{"id":"devin-lead-qa","health":{"status":"auth_failed"},"ok":0,"fails":2,"last_error":{"unix_ms":1789215077004,"status":401,"message":"unauthenticated request to upstream"},"models":["devin/glm-5-2","devin/swe-1-7"]}
```

The gateway stdout log under the authorized E2E directory contained startup/listening and skipped telemetry/usage credential-file warnings, not per-surface request traces. The exact historical request ordering is not independently replayed here. Live health, the last 401, code tracing, and independent route tests support the health explanation; no independent normalization bug was reproduced.

## Failing-first evidence

### Endpoint unit/integration seam

Command before production edit:

```sh
cargo test -p mahoquot-gateway --test devin_relay relay_target
```

Output (full log: `p7-logs/relay-red.log`):

```text
running 2 tests
relay_uses_explicit_override_when_native_endpoint_differs ... ok
relay_uses_api_server_url_when_explicit_override_is_absent ... FAILED
assertion `left == right` failed
  left: "https://server.codeium.com/exa.api_server_pb.ApiServerService/GetChatMessage"
 right: "http://127.0.0.1:54321/exa.api_server_pb.ApiServerService/GetChatMessage"
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 21 filtered out
EXIT_CODE=101
```

After the loader fix, the full `devin_relay` run includes both tests passing (23/23, exit 0; `p7-logs/relay-green.log`). Distinct explicit/native fixture URLs ensure precedence cannot pass by selecting the fallback.

### Messages/Gemini real-route selection

Command before production edit:

```sh
RUSTC_WRAPPER= cargo test -p mahoquot-gateway --test devin_surfaces surface_selection
```

Output (full log: `p7-logs/surfaces-red.log`):

```text
running 6 tests
messages_selects_discovered_account_when_only_native_endpoint_is_set ... FAILED
gemini_selects_discovered_account_when_only_native_endpoint_is_set ... FAILED
messages_returns_unavailable_when_discovered_account_is_auth_failed ... ok
gemini_returns_unavailable_when_discovered_account_is_auth_failed ... ok
messages_selects_discovered_account_when_endpoint_is_explicit ... ok
gemini_selects_discovered_account_when_endpoint_is_explicit ... ok
```

Both failures were the local URL guard:

```text
  left: "https://server.codeium.com/exa.api_server_pb.ApiServerService/GetChatMessage"
 right: "http://127.0.0.1:62122/exa.api_server_pb.ApiServerService/GetChatMessage"
```

Gemini's isolated fixture used port 62119. Summary: `4 passed; 2 failed; 8 filtered out`, exit 101. These red tests deliberately stop before off-host I/O; they do NOT claim a failing model-selection assertion. The explicit-endpoint control tests ran the real routes before the fix and selected the account successfully, disproving the proposed normalization regression for the supplied snapshot.

After the fix, all six pass within the full 14/14 `devin_surfaces` run (exit 0; `p7-logs/surfaces-green.log`). Each healthy route test asserts HTTP 200, exactly one mock capture, and wire `chat_model_uid == "glm-5-2"`. The unavailable-health tests retain HTTP 503 and zero upstream captures. Fixtures publish a PoolSnapshot declaring both discovered public IDs. Request completion is awaited with bounded timeouts; no sleeps/polling were added.

## Catalog test adjustment

The first post-fix catalog run produced 25 passes and one failure, exit 101 (`p7-logs/catalog-fixture-failure.log`):

```text
test_devin_same_token_endpoint_change_race ... FAILED
publication must reject catalog with mismatched effective base URL
```

That test mutated `member.inner` directly while the loaded effective endpoint now correctly lives in `member.upstream_override`. It no longer changed the effective endpoint, so rejecting publication was not the expected behavior for that fixture.

Updated `crates/gateway/tests/devin_catalog.rs:920-965` to write the changed endpoint into the credential file and call `state.rescan_pool()`, matching actual credential replacement. The token stays identical; the old catalog still must be rejected as `StalePublication`. The assertion was neither removed nor weakened. The corrected test passes in the full 26/26 catalog run. No `devin_catalog.rs` production logic or publication logic was modified.

## Final verification

Commands ran serially, each in its own cargo invocation, from the repository root. For final runs, `RUSTC_WRAPPER` was exported as an empty string to bypass the workstation's contended sccache, not to change compiler flags or suppress diagnostics.

| Exact cargo command | Result | Exit | Full log |
| --- | --- | --- | --- |
| `cargo test -p mahoquot-gateway --test devin_relay` | 23 passed, 0 failed, 0 ignored | 0 | `p7-logs/relay-green.log` |
| `cargo test -p mahoquot-gateway --test devin_surfaces` | 14 passed, 0 failed, 0 ignored | 0 | `p7-logs/surfaces-green.log` |
| `cargo test -p mahoquot-gateway --test devin_catalog` | 26 passed, 0 failed, 0 ignored | 0 | `p7-logs/catalog-green.log` |
| `cargo check -p mahoquot-gateway --tests` | Finished dev profile, no warnings/errors | 0 | `p7-logs/check-green.log` |

Total final test coverage: 63 passing tests across the three required suites. The final catalog-only edit did not affect relay/surface source or fixtures. LSP diagnostics on every changed Rust file returned no diagnostics using the real `/Volumes/T9-Mac/project/mahoquot-proxy` path. The first symlink-path LSP attempt reported file-not-found, not clean diagnostics. `git diff --check` on the tracked changed account file and catalog test returned exit 0.

Infrastructure note: two initial surface-red cargo attempts were terminated by tool timeouts at 180 seconds while blocked on an unrelated build/compilation, with no test result or cargo exit code. A separate `mahoquot` build script launched cargo against this checkout. I observed its exit via a bounded macOS process-exit event, did not kill it, and never launched two task-owned cargo invocations concurrently. Later runs also encountered external build locks; the surface final run took 14m41s including that contention. See `p7-logs/build-contention.log`.

## Files changed by this task

- `crates/gateway/src/account.rs`: add Devin-native endpoint fallback with explicit-override precedence in the existing loader.
- `crates/gateway/tests/devin_relay.rs`: register the separate endpoint regression module.
- `crates/gateway/tests/devin_surfaces.rs`: register the separate surface-selection regression module.
- `crates/gateway/tests/devin_regressions/relay_target.rs`: endpoint fallback and precedence tests through auth loading plus the relay URL builder.
- `crates/gateway/tests/devin_regressions/surface_selection.rs`: six real-route tests separating native endpoint routing, existing canonical selection, and auth-failed rejection.
- `crates/gateway/tests/devin_catalog.rs`: exercise endpoint replacement via persisted credential/rescan instead of inner-only mutation.
- `.omo/evidence/devin/p7-e2e-defects.md` and `p7-logs/*`: this report and captured red/green outputs.

## Design choice and review

Considered changing target construction to call the effective-base helper, versus fixing credential loading once. Chose the loader seam because it satisfies the required explicit-override contract for startup and reload, uses the existing relay URL path, and avoids per-surface patches. Considered a normalization change, but the control tests and live auth-failed state contradict the premise; preserving normalization and health rejection is the smaller correct fix.

Post-write review: new modules each own one responsibility (endpoint regression; surface selection). No production untyped boundary was introduced; the existing credential boundary still validates the provider record. Provider matching is exhaustive. No unsafe code, production unwrap/expect, casts, warning suppression, defensive internal checks, new logging, parameter bloat, negative names, or speculative helper was introduced. Test helpers have multiple callers. Tests use real routes, local mocks, unique directories, ephemeral ports, awaited completion, and unchanged machine-contract assertions. No new prose pins were added.

Measured pure LOC: account.rs 1606; existing relay test root 1823; existing surface test root 674; catalog test 1431; new relay_target.rs 35; new surface_selection.rs 115. The existing oversized files are inherited debt; broad splits were deliberately excluded by the focused scope/preserve-dirty-edits constraints. New test bodies live in small separate modules instead of expanding the oversized roots. This task does not claim those inherited architectural defects are resolved.

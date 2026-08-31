# OpenCodex + CLIProxyAPI Reference Parity

Scope is limited to concrete implementations in `/Users/indo/code/project/opencodex`
and the vendored CLIProxyAPI sources in `.omo/upstream`. The executable source of
truth is `reference-parity.json`, enforced by
`crates/gateway/tests/t20_reference_parity.rs`.

## Authentication and credential lifecycle

| Flow | Reference evidence | Implementation owner | GREEN proof |
|---|---|---|---|
| Claude OAuth | `../opencodex/src/oauth/anthropic.ts:7` | `crates/gateway/src/management/oauth.rs:847` | `t13_provider_oauth::test_anthropic_oauth_flow_end_to_end` |
| Codex PKCE OAuth | `../opencodex/src/oauth/chatgpt.ts:94` | `crates/gateway/src/management/oauth.rs:353` | `t13_provider_oauth::test_codex_oauth_flow_persists_a_routable_account` |
| Antigravity OAuth | `../opencodex/src/oauth/google-antigravity.ts:185` | `crates/gateway/src/management/oauth.rs:50` | `t13_provider_oauth::test_antigravity_oauth_flow_end_to_end` |
| xAI PKCE OAuth | `../opencodex/src/oauth/xai.ts:164` | `crates/gateway/src/management/oauth.rs:392` | `t13_provider_oauth::xai_pkce_callback_writes_a_live_generic_account` |
| Kimi device grant | `../opencodex/src/oauth/kimi.ts:129` | `crates/gateway/src/management/oauth.rs:2158` | `t13_provider_oauth::device_oauth_starts_polls_and_writes_generic_provider_credentials` |
| Cursor PKCE polling | `../opencodex/src/oauth/cursor.ts:121` | `crates/gateway/src/management/oauth.rs:897` | `t13_provider_oauth::test_cursor_oauth_flow_end_to_end` |
| GitHub Copilot device grant | `../opencodex/src/oauth/github-copilot.ts:40` | `crates/gateway/src/management/oauth.rs:2170` | `t13_provider_oauth::device_oauth_starts_polls_and_writes_generic_provider_credentials` |
| Command Code callback OAuth | `../opencodex/src/oauth/command-code.ts:8` | `crates/gateway/src/management/oauth.rs:1576` | `t13_provider_oauth::test_command_code_oauth_flow_end_to_end` |
| Kiro credential import | `../opencodex/src/oauth/kiro.ts:34` | `crates/gateway/src/management/creds.rs:783` | `management::creds::provider_imports_accept_reference_credential_shapes` |
| Vertex service account | `.omo/upstream/internal__api__handlers__management__vertex_import.go:16` | `crates/gateway/src/management/creds.rs:528` | `t17_account_lifecycle::vertex_import_exchanges_service_account_and_joins_google_pool` |
| Generic API key | `../opencodex/src/oauth/key-providers.ts:7` | `crates/gateway/src/management/creds.rs:335` | `t17_account_lifecycle::generic_openai_key_provider_joins_pool` |
| Expired credential refresh | `../opencodex/src/oauth/token-guardian.ts:2` | `crates/providers/src/refresh_exec.rs:29` | `t7_refresh::generic_oauth_accounts_refresh_with_provider_contracts` |
| Atomic persistence | `../opencodex/src/config/atomic-write.ts:95` | `crates/gateway/src/management/creds.rs:385` | `management::creds::writing_a_credential_leaves_no_temp_file` |
| Runtime pool rescan | `.omo/upstream/internal__api__handlers__management__auth_files_crud.go:131` | `crates/gateway/src/management/creds.rs:244` | `t15_pool_rescan::an_imported_credential_appears_in_the_live_pool_without_restart` |

## Proxy and streaming contracts

| Runtime | Reference evidence | Implementation owner | GREEN proof |
|---|---|---|---|
| Codex OpenAI compatibility | `../opencodex/src/adapters/openai-chat.ts:1360` | `crates/gateway/src/account.rs:73` | `t8_openai_compat::test_t8_non_streaming_returns_single_completion` |
| Claude Messages | `../opencodex/src/adapters/anthropic.ts:920` | `crates/gateway/src/compat/claude.rs:112` | `t12_provider_relay::claude_relays_native_anthropic_wire_end_to_end` |
| Antigravity Gemini wire | `../opencodex/src/adapters/google-antigravity-wire.ts:6` | `crates/gateway/src/compat/gemini.rs:127` | `t9_antigravity_compat::test_t9_request_targets_antigravity_envelope` |
| Google AIStudio | `../opencodex/src/adapters/google-http.ts:107` | `crates/gateway/src/relay.rs:776` | `t12_provider_relay::google_ai_studio_uses_generativelanguage_wire_and_api_key_header` |
| Vertex Gemini wire | `.omo/upstream/internal__api__handlers__management__vertex_import.go:16` | `crates/providers/src/vertex.rs:42` | `t19_vertex_runtime::test_vertex_runtime_lifecycle_wire_and_refresh` |
| Generic OpenAI-compatible | `../opencodex/src/adapters/openai-chat.ts:1360` | `crates/gateway/src/account.rs:37` | `t12_provider_relay::generic_openai_chat_provider_relays_json_without_codex_translation` |
| Responses compact | `../opencodex/src/server/responses/compact.ts:264` | `crates/gateway/src/cp_routes.rs:376` | `t8_openai_compat::test_t8_responses_compact_relays_to_codex_upstream` |

## Explicitly excluded surfaces

| Surface | Reference evidence | Boundary owner | GREEN proof |
|---|---|---|---|
| xAI image/video models without a quotio executor | `../opencodex/src/images/xai-client.ts:13` | `crates/gateway/src/capability.rs:117` | `t8_openai_compat::test_t8_unimplemented_xai_media_models_fail_closed` |
| Codex WebSocket transport | `../opencodex/src/codex/websocket-registry.ts:1` | `crates/gateway/src/realtime.rs:58` | `t8_openai_compat::test_t8_unsupported_websocket_transport_fails_before_upgrade` |
| CLIProxyAPI pluginhost | `.omo/upstream/internal__api__handlers__management__plugins.go:22` | `crates/gateway/src/management/mod.rs:29` | `t14_management_route_parity::unimplemented_pluginhost_routes_are_not_advertised_as_handlers` |

Kiro is intentionally cataloged as credential import rather than browser OAuth.
Command Code is cataloged as OAuth because its loopback callback lifecycle is now
implemented and tested. ZCode remains a provisioned composite-key flow; no excluded
Gajae Code OAuth behavior is advertised.

## Executable verification

```text
cargo test -p mahoquot-gateway --test t20_reference_parity -- --nocapture
running 1 test
test every_reference_backed_flow_maps_to_an_owner_green_test_and_evidence ... ok
test result: ok. 1 passed; 0 failed
```

The verifier checks every matrix row in both directions: the exact reference
`file:line` contains its declared needle, the implementation owner and GREEN test
exist at their recorded locations, evidence files exist, row IDs are unique, and
each row is explicitly `included` or `excluded`.

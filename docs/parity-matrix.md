# CLIProxyAPI Parity Matrix & Subsystem Audit

> **Baseline**: 114 endpoints vendored from upstream CLIProxyAPI (`docs/baseline/cliproxy-endpoints.txt`).
> **Audit Target**: `mahoquot-proxy` Axum gateway (`crates/gateway/src/`).
> **Status**: Living artifact tracking parity classifications, mount alignments, tier porting obligations, and deferred subsystem boundaries.

---

## 1. Executive Summary & Reconciliation

### Classification Totals

| Classification | Count | Description |
| --- | ---: | --- |
| `same-path` | **71** | Implemented on the identical path with matching request/response semantics. |
| `different-path` | **22** | Implemented in `mahoquot-proxy`, mounted under standard provider/API subpaths (compatibility aliases tracked in Section 4). |
| `partial` | **7** | Management CRUD / routing skeleton in place; full downstream engine wiring or multi-field updates targeted for completion. |
| `missing→ported` | **0** | Missing functionality to be implemented in downstream todos (P0/P1). |
| `deferred` | **14** | Subsystem explicitly deferred (plugin runtime host, TUI, Redis queue, test harness fixtures). |
| **Total Reconciled** | **114** | **100% of the 114 raw upstream endpoints accounted for.** |

### Cluster Rollup & Reconciliation

| Cluster | Endpoints | same-path | different-path | partial | missing→ported | deferred |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| API Key Management | 3 | 3 | 0 | 0 | 0 | 0 |
| Auth & Identity | 19 | 18 | 1 | 0 | 0 | 0 |
| Config & Settings | 17 | 17 | 0 | 0 | 0 | 0 |
| Core / Meta & Health | 5 | 5 | 0 | 0 | 0 | 0 |
| Core Inference & Chat | 8 | 1 | 7 | 0 | 0 | 0 |
| Media / Images & Video | 9 | 0 | 9 | 0 | 0 | 0 |
| Observability & Debug | 6 | 6 | 0 | 0 | 0 | 0 |
| Plugins & Extensions | 6 | 0 | 0 | 0 | 0 | 6 |
| Provider Key Config Lists | 7 | 0 | 0 | 7 | 0 | 0 |
| Realtime & Live | 16 | 14 | 2 | 0 | 0 | 0 |
| Responses & Search | 7 | 3 | 3 | 0 | 0 | 1 |
| Routing & Quota | 4 | 4 | 0 | 0 | 0 | 0 |
| Test Harness & Diagnostics | 7 | 0 | 0 | 0 | 0 | 7 |
| **Total** | **114** | **71** | **22** | **7** | **0** | **14** |

---

## 2. 114-Endpoint Parity Matrix

| # | CLIProxyAPI Endpoint | Cluster | Classification | Tier | Upstream Handler | mahoquot Route / Handler | Implementation / Port Notes |
| ---: | --- | --- | --- | :---: | --- | --- | --- |
| 1 | `/` | Core / Meta & Health | `same-path` | — | `s.engine.GET("/")` (`server_routes.go:131`) | `cp_routes::root` (`crates/gateway/src/routes.rs:124`) | Root server banner returning JSON service identification and advertised endpoints. |
| 2 | `/abort` | Test Harness & Diagnostics | `deferred` | — | `engine.GET("/abort") (test fixture)` (`internal/logging/gin_logger_test.go:17`) | `—` (`Not implemented`) | Deferred: internal Go Gin logger test fixture testing 500 error abort handling; non-production route. |
| 3 | `/alpha/search` | Responses & Search | `different-path` | — | `v1.POST, codexDirect.POST` (`server_routes.go:80,117`) | `cp_routes::alpha_search` (`crates/gateway/src/routes.rs:46,55`) | Upstream mounts on `/v1/alpha/search` and `/backend-api/codex/alpha/search`; both mounted in mahoquot. Unprefixed `/alpha/search` alias candidate. |
| 4 | `/anthropic-auth-url` | Auth & Identity | `same-path` | — | `mgmt.GET s.mgmt.RequestAnthropicToken` (`server_management.go:176`) | `auth_url_for("anthropic")` (`crates/gateway/src/management/oauth.rs:2413`) | Generates Anthropic OAuth authorization URL with PKCE parameters and state cookie. |
| 5 | `/anthropic/callback` | Auth & Identity | `same-path` | — | `s.engine.GET("/anthropic/callback")` (`server_routes.go:145`) | `cp_routes::oauth_callback` (`crates/gateway/src/routes.rs:126`) | Public browser OAuth callback redirect handler capturing state/code for Anthropic. |
| 6 | `/antigravity-auth-url` | Auth & Identity | `same-path` | — | `mgmt.GET s.mgmt.RequestAntigravityToken` (`server_management.go:178`) | `antigravity_auth_url_handler` (`crates/gateway/src/management/oauth.rs:2390`) | Generates Antigravity OAuth authorization URL and initializes pending session. |
| 7 | `/antigravity/callback` | Auth & Identity | `same-path` | — | `s.engine.GET("/antigravity/callback")` (`server_routes.go:173`) | `cp_routes::oauth_callback` (`crates/gateway/src/routes.rs:128`) | Public browser OAuth callback redirect handler capturing state/code for Antigravity. |
| 8 | `/api-call` | Observability & Debug | `same-path` | — | `mgmt.POST s.mgmt.APICall` (`server_management.go:68`) | `core::api_call` (`crates/gateway/src/management/core.rs:176`) | Management debugging proxy endpoint executing direct provider API calls with stored credentials. |
| 9 | `/api-key-usage` | API Key Management | `same-path` | — | `mgmt.GET s.mgmt.GetAPIKeyUsage` (`server_management.go:83`) | `apikeys::api_key_usage` (`crates/gateway/src/management/apikeys.rs:134`) | Returns usage token counts and request metrics for inbound gateway API keys. |
| 10 | `/api-keys` | API Key Management | `same-path` | — | `mgmt.GET/PUT/PATCH/DELETE s.mgmt.*APIKeys` (`server_management.go:79-82`) | `apikeys::KEY_LISTS (CRUD)` (`crates/gateway/src/management/apikeys.rs:25`) | CRUD management for global inbound proxy API access keys list. |
| 11 | `/auth-files` | Auth & Identity | `same-path` | — | `mgmt.GET/POST/DELETE s.mgmt.*AuthFile` (`server_management.go:166,170,171`) | `creds::list_auth_files / create / delete` (`crates/gateway/src/management/creds.rs:915`) | Account auth files CRUD (lists active accounts, uploads new credential JSONs, removes accounts). |
| 12 | `/auth-files/download` | Auth & Identity | `same-path` | — | `mgmt.GET s.mgmt.DownloadAuthFile` (`server_management.go:169`) | `creds::download_auth_file` (`crates/gateway/src/management/creds.rs:924`) | Exports and downloads credential JSON file for a specified account. |
| 13 | `/auth-files/fields` | Auth & Identity | `same-path` | — | `mgmt.PATCH s.mgmt.PatchAuthFileFields` (`server_management.go:173`) | `creds::patch_auth_file_fields` (`crates/gateway/src/management/creds.rs:930`) | Selective multi-field JSON patching over auth file documents on disk with pool rescan. |
| 14 | `/auth-files/models` | Auth & Identity | `same-path` | — | `mgmt.GET s.mgmt.GetAuthFileModels` (`server_management.go:167`) | `creds::auth_file_models` (`crates/gateway/src/management/creds.rs:923`) | Returns active model catalogs and custom model mappings for each registered auth file. |
| 15 | `/auth-files/status` | Auth & Identity | `same-path` | — | `mgmt.PATCH s.mgmt.PatchAuthFileStatus` (`server_management.go:172`) | `creds::patch_auth_file_status` (`crates/gateway/src/management/creds.rs:925`) | Toggles enabled/disabled/quota status flag for an individual account auth file. |
| 16 | `/backend-api/codex/responses` | Responses & Search | `same-path` | — | `codexDirect.GET/POST responses` (`server_routes.go:114-115`) | `cp_routes::ws_upgrade / codex_responses_handler` (`crates/gateway/src/routes.rs:38`) | ChatGPT Codex CLI direct responses route (Websocket streaming upgrade + HTTP POST response). |
| 17 | `/chat/completions` | Core Inference & Chat | `different-path` | — | `v1.POST openaiHandlers.ChatCompletions` (`server_routes.go:66`) | `chat_completions_handler` (`crates/gateway/src/routes.rs:34`) | Upstream mounts on `/v1/chat/completions`; mahoquot mounts at `/v1/chat/completions`. OpenAI chat completions proxy. |
| 18 | `/claude-api-key` | Provider Key Config Lists | `partial` | P0 | `mgmt.GET/PUT/PATCH/DELETE ClaudeKeys` (`server_management.go:126-129`) | `apikeys::KEY_LISTS` (`crates/gateway/src/management/apikeys.rs:35`) | Management CRUD in `apikeys.rs`; wiring stored keys into relay credential pool resolution completed in Todo 2. |
| 19 | `/codex-api-key` | Provider Key Config Lists | `partial` | P0 | `mgmt.GET/PUT/PATCH/DELETE CodexKeys` (`server_management.go:131-134`) | `apikeys::KEY_LISTS` (`crates/gateway/src/management/apikeys.rs:40`) | Management CRUD in `apikeys.rs`; wiring stored keys into relay credential pool resolution completed in Todo 2. |
| 20 | `/codex-auth-url` | Auth & Identity | `same-path` | — | `mgmt.GET s.mgmt.RequestCodexToken` (`server_management.go:177`) | `codex_auth_url_handler` (`crates/gateway/src/management/oauth.rs:2388`) | Generates OpenAI Codex OAuth device flow URL and initializes polling session. |
| 21 | `/codex/callback` | Auth & Identity | `same-path` | — | `s.engine.GET("/codex/callback")` (`server_routes.go:159`) | `cp_routes::oauth_callback` (`crates/gateway/src/routes.rs:127`) | Public browser OAuth callback redirect handler capturing state/code for Codex. |
| 22 | `/committed` | Test Harness & Diagnostics | `deferred` | — | `engine.GET("/committed") (test fixture)` (`internal/logging/cpa_trace_test.go:51`) | `—` (`Not implemented`) | Deferred: internal Go CPA trace test fixture verifying committed span lifecycle; non-production route. |
| 23 | `/completions` | Core Inference & Chat | `different-path` | — | `v1.POST openaiHandlers.Completions` (`server_routes.go:67`) | `completions_handler` (`crates/gateway/src/routes.rs:35`) | Upstream mounts on `/v1/completions`; mahoquot mounts at `/v1/completions`. Legacy OpenAI text completions proxy. |
| 24 | `/config.yaml` | Config & Settings | `same-path` | — | `mgmt.GET/PUT s.mgmt.*ConfigYAML` (`server_management.go:31-32`) | `core::get_config_yaml / put_config_yaml` (`crates/gateway/src/management/core.rs:174`) | Raw YAML configuration document export and replacement endpoint. |
| 25 | `/config` | Config & Settings | `same-path` | — | `mgmt.GET s.mgmt.GetConfig` (`server_management.go:30`) | `core::get_config` (`crates/gateway/src/management/core.rs:173`) | JSON configuration document retrieval endpoint. |
| 26 | `/debug` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH s.mgmt.*Debug` (`server_management.go:43-45`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:89`) | Get/set boolean flag for debug mode logging. |
| 27 | `/error-logs-max-files` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH ErrorLogsMaxFiles` (`server_management.go:55-57`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:103`) | Get/set maximum retained error log file count. |
| 28 | `/force-model-prefix` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH ForceModelPrefix` (`server_management.go:118-120`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:142`) | Get/set boolean flag to enforce provider model name prefixing. |
| 29 | `/gemini-api-key` | Provider Key Config Lists | `partial` | P0 | `mgmt.GET/PUT/PATCH/DELETE GeminiKeys` (`server_management.go:86-89`) | `apikeys::KEY_LISTS` (`crates/gateway/src/management/apikeys.rs:30`) | Management CRUD in `apikeys.rs`; wiring stored keys into relay credential pool resolution completed in Todo 2. |
| 30 | `/get-auth-status` | Auth & Identity | `same-path` | — | `mgmt.GET s.mgmt.GetAuthStatus` (`server_management.go:181`) | `oauth::auth_status` (`crates/gateway/src/management/oauth.rs:2387`) | Polls status and token completion for active in-flight OAuth login flows. |
| 31 | `/healthz` | Core / Meta & Health | `same-path` | — | `s.engine.GET/HEAD("/healthz")` (`server_routes.go:51`) | `routes::healthz_handler` (`crates/gateway/src/routes.rs:122`) | Liveness and health check endpoint returning 200 `{"status":"ok"}`. |
| 32 | `/images/edits` | Media / Images & Video | `different-path` | — | `v1.POST openaiHandlers.ImagesEdits` (`server_routes.go:69`) | `cp_routes::images_edits` (`crates/gateway/src/routes.rs:60`) | Upstream mounts on `/v1/images/edits`; mahoquot mounts at `/v1/images/edits`. Image editing inference proxy. |
| 33 | `/images/generations` | Media / Images & Video | `different-path` | — | `v1.POST openaiHandlers.ImagesGenerations` (`server_routes.go:68`) | `cp_routes::images_generations` (`crates/gateway/src/routes.rs:56`) | Upstream mounts on `/v1/images/generations`; mahoquot mounts at `/v1/images/generations`. Image generation inference proxy. |
| 34 | `/interactions-api-key` | Provider Key Config Lists | `partial` | P0 | `mgmt.GET/PUT/PATCH/DELETE InteractionsKeys` (`server_management.go:91-94`) | `apikeys::KEY_LISTS` (`crates/gateway/src/management/apikeys.rs:55`) | Management CRUD in `apikeys.rs`; wiring stored keys into relay credential pool resolution completed in Todo 2. |
| 35 | `/interactions` | Core Inference & Chat | `different-path` | — | `v1beta.POST geminiHandlers.Interactions` (`server_routes.go:125`) | `cp_routes::v1beta_interactions` (`crates/gateway/src/routes.rs:116`) | Upstream mounts on `/v1beta/interactions`; mahoquot mounts at `/v1beta/interactions`. Unprefixed `/interactions` alias candidate. |
| 36 | `/keep-alive` | Core / Meta & Health | `same-path` | — | `s.engine.GET s.handleKeepAlive` (`server_keepalive.go:24`) | `routes::keep_alive_handler` (`crates/gateway/src/routes.rs:223`) | Public connection keepalive/heartbeat ping endpoint returning 200 `{"status":"ok"}`. |
| 37 | `/kimi-auth-url` | Auth & Identity | `same-path` | — | `mgmt.GET s.mgmt.RequestKimiToken` (`server_management.go:179`) | `device_auth_url("kimi")` (`crates/gateway/src/management/oauth.rs:2400`) | Generates Kimi/Moonshot device authorization URL and tracking session. |
| 38 | `/latest-version` | Core / Meta & Health | `same-path` | — | `mgmt.GET s.mgmt.GetLatestVersion` (`server_management.go:33`) | `core::get_latest_version` (`crates/gateway/src/management/core.rs:175`) | Checks upstream repository release tags and returns latest version metadata. |
| 39 | `/live` | Realtime & Live | `different-path` | — | `v1.POST s.codexLiveHandler.Handle` (`server_routes.go:81`) | `cp_routes::realtime_offer` (`crates/gateway/src/routes.rs:75`) | Upstream mounts on `/v1/live`; mahoquot mounts at `/v1/live`. Realtime WebRTC session negotiation. |
| 40 | `/live/:call_id` | Realtime & Live | `different-path` | — | `v1.GET s.codexLiveHandler.HandleSideband` (`server_routes.go:82`) | `cp_routes::live_sideband` (`crates/gateway/src/routes.rs:76`) | Upstream mounts on `/v1/live/:call_id`; mahoquot mounts at `/v1/live/{call_id}`. Unprefixed `/live/:call_id` alias candidate. |
| 41 | `/logging-to-file` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH LoggingToFile` (`server_management.go:47-49`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:96`) | Get/set boolean flag enabling file-based logging output. |
| 42 | `/logs-max-total-size-mb` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH LogsMaxTotalSizeMB` (`server_management.go:51-53`) | `observability::logs_max_total_size` (`crates/gateway/src/management/observability.rs:262`) | Get/set maximum total size in megabytes for log retention. |
| 43 | `/logs` | Observability & Debug | `same-path` | — | `mgmt.GET/DELETE s.mgmt.*Logs` (`server_management.go:96-97`) | `observability::get_logs / delete_logs` (`crates/gateway/src/management/observability.rs:249`) | Retrieves recent in-memory log lines and clears buffer. |
| 44 | `/management.html` | Core / Meta & Health | `same-path` | — | `s.engine.GET serveManagementControlPanel` (`server_routes.go:54`) | `cp_routes::management_html` (`crates/gateway/src/routes.rs:125`) | Serves embedded web management console control panel UI. |
| 45 | `/max-retry-credentials` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH MaxRetryCredentials` (`server_management.go:111-113`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:128`) | Get/set maximum number of alternative credentials attempted on transient upstream failures. |
| 46 | `/max-retry-interval` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH MaxRetryInterval` (`server_management.go:114-116`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:135`) | Get/set backoff interval between retry attempts in seconds. |
| 47 | `/messages` | Core Inference & Chat | `different-path` | — | `v1.POST claudeCodeHandlers.ClaudeMessages` (`server_routes.go:75`) | `messages_handler` (`crates/gateway/src/routes.rs:36`) | Upstream mounts on `/v1/messages`; mahoquot mounts at `/v1/messages`. Anthropic Messages API proxy. |
| 48 | `/messages/count_tokens` | Core Inference & Chat | `different-path` | — | `v1.POST ClaudeCountTokens` (`server_routes.go:76`) | `count_tokens_handler` (`crates/gateway/src/routes.rs:37`) | Upstream mounts on `/v1/messages/count_tokens`; mahoquot mounts at `/v1/messages/count_tokens`. Anthropic token counting proxy. |
| 49 | `/model-definitions/:channel` | Auth & Identity | `same-path` | — | `mgmt.GET GetStaticModelDefinitions` (`server_management.go:168`) | `creds::model_definitions` (`crates/gateway/src/management/creds.rs:933`) | Returns static model capabilities catalog for specified release channel. |
| 50 | `/models` | Core Inference & Chat | `different-path` | — | `v1.GET, v1beta.GET unifiedModelsHandler` (`server_routes.go:65,124`) | `models_handler / v1beta_models` (`crates/gateway/src/routes.rs:24,111`) | Upstream mounts on `/v1/models` & `/v1beta/models`; mahoquot mounts at both. Model catalog listing. |
| 51 | `/models/*action` | Core Inference & Chat | `different-path` | — | `v1beta.POST/GET GeminiHandler` (`server_routes.go:126-127`) | `cp_routes::v1beta_action` (`crates/gateway/src/routes.rs:112`) | Upstream mounts on `/v1beta/models/*action`; mahoquot mounts at `/v1beta/models/{*action}`. Unprefixed `/models/*action` alias candidate. |
| 52 | `/oauth-callback` | Auth & Identity | `different-path` | — | `SDK helper / test handler` (`sdk/auth/antigravity.go:244`) | `management::oauth::oauth_callback` (`crates/gateway/src/routes.rs:129`) | Upstream mounts at `/v0/management/oauth-callback`; mahoquot mounts at `/v0/management/oauth-callback`. Top-level alias candidate. |
| 53 | `/oauth-excluded-models` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH/DELETE OAuthExcludedModels` (`server_management.go:151-154`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:197`) | Per-provider list of models excluded from OAuth credential usage. |
| 54 | `/oauth-model-alias` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH/DELETE OAuthModelAlias` (`server_management.go:156-159`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:219`) | Per-provider model name alias mapping table. |
| 55 | `/oauth-request-scoped-errors` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH/DELETE RequestScopedErrors` (`server_management.go:161-164`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:226`) | Per-provider error pattern matching and transformation rules. |
| 56 | `/oauth-session` | Auth & Identity | `same-path` | — | `mgmt.DELETE s.mgmt.CancelAuthSession` (`server_management.go:182`) | `oauth::cancel_session` (`crates/gateway/src/management/creds.rs:938`) | Cancels and discards active pending OAuth authorization session. |
| 57 | `/openai-compatibility` | Provider Key Config Lists | `partial` | P0 | `mgmt.GET/PUT/PATCH/DELETE OpenAICompat` (`server_management.go:141-144`) | `apikeys::KEY_LISTS` (`crates/gateway/src/management/apikeys.rs:60`) | Management CRUD in `apikeys.rs`; wiring custom OpenAI base URLs and keys into relay execution in Todo 3. |
| 58 | `/panic` | Test Harness & Diagnostics | `deferred` | — | `engine.GET("/panic") (test fixture)` (`internal/logging/gin_logger_test.go:49`) | `—` (`Not implemented`) | Deferred: internal Go Gin logger test fixture testing panic recovery middleware; non-production route. |
| 59 | `/plugin-store` | Plugins & Extensions | `deferred` | — | `mgmt.GET s.mgmt.ListPluginStore` (`server_management.go:35`) | `plugins::list_plugin_store (unmerged)` (`crates/gateway/src/management/plugins.rs:90`) | Deferred: pluginhost subsystem deferred; dynamic marketplace plugin store repository out of scope. |
| 60 | `/plugin-store/:id/install` | Plugins & Extensions | `deferred` | — | `mgmt.POST s.mgmt.InstallPluginFromStore` (`server_management.go:36`) | `plugins::install_plugin (unmerged)` (`crates/gateway/src/management/plugins.rs:91`) | Deferred: pluginhost subsystem deferred; dynamic plugin package installation out of scope. |
| 61 | `/plugins` | Plugins & Extensions | `deferred` | — | `mgmt.GET s.mgmt.ListPlugins` (`server_management.go:34`) | `plugins::list_plugins (unmerged)` (`crates/gateway/src/management/plugins.rs:89`) | Deferred: pluginhost subsystem deferred; dynamic runtime plugin management out of scope. |
| 62 | `/plugins/:id` | Plugins & Extensions | `deferred` | — | `mgmt.DELETE s.mgmt.DeletePlugin` (`server_management.go:37`) | `plugins::delete_plugin (unmerged)` (`crates/gateway/src/management/plugins.rs:92`) | Deferred: pluginhost subsystem deferred; plugin deletion out of scope. |
| 63 | `/plugins/:id/config` | Plugins & Extensions | `deferred` | — | `mgmt.GET/PUT/PATCH s.mgmt.*PluginConfig` (`server_management.go:39-41`) | `plugins::get_plugin_config (unmerged)` (`crates/gateway/src/management/plugins.rs:94`) | Deferred: pluginhost subsystem deferred; per-plugin JSON configuration store out of scope. |
| 64 | `/plugins/:id/enabled` | Plugins & Extensions | `deferred` | — | `mgmt.PATCH s.mgmt.PatchPluginEnabled` (`server_management.go:38`) | `plugins::patch_plugin_enabled (unmerged)` (`crates/gateway/src/management/plugins.rs:93`) | Deferred: pluginhost subsystem deferred; plugin enable/disable toggle out of scope. |
| 65 | `/proxy-url` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH/DELETE ProxyURL` (`server_management.go:63-66`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:154`) | Configures outbound corporate/datacenter HTTP/HTTPS proxy URL. |
| 66 | `/quota-exceeded/switch-preview-model` | Routing & Quota | `same-path` | — | `mgmt.GET/PUT/PATCH SwitchPreviewModel` (`server_management.go:74-76`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:190`) | Get/set toggle enabling automatic fallback to preview models on quota exhaustion. |
| 67 | `/quota-exceeded/switch-project` | Routing & Quota | `same-path` | — | `mgmt.GET/PUT/PATCH SwitchProject` (`server_management.go:70-72`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:183`) | Get/set toggle enabling automatic fallback across GCP/Vertex projects on quota exhaustion. |
| 68 | `/race` | Test Harness & Diagnostics | `deferred` | — | `engine.GET("/race") (test fixture)` (`internal/logging/cpa_trace_test.go:93`) | `—` (`Not implemented`) | Deferred: internal Go CPA trace test fixture verifying concurrency race conditions; non-production route. |
| 69 | `/request-error-logs` | Observability & Debug | `same-path` | — | `mgmt.GET s.mgmt.GetRequestErrorLogs` (`server_management.go:98`) | `observability::request_error_logs` (`crates/gateway/src/management/observability.rs:250`) | Lists saved request failure artifacts and error payloads. |
| 70 | `/request-error-logs/:name` | Observability & Debug | `same-path` | — | `mgmt.GET DownloadRequestErrorLog` (`server_management.go:99`) | `observability::request_error_log_by_name` (`crates/gateway/src/management/observability.rs:251`) | Downloads specific request error dump artifact by file name. |
| 71 | `/request-log-by-id/:id` | Observability & Debug | `same-path` | — | `mgmt.GET s.mgmt.GetRequestLogByID` (`server_management.go:100`) | `observability::request_log_by_id` (`crates/gateway/src/management/observability.rs:252`) | Retrieves detailed trace metadata and response info for a specific request ID. |
| 72 | `/request-log` | Observability & Debug | `same-path` | — | `mgmt.GET/PUT/PATCH RequestLog` (`server_management.go:101-103`) | `observability::get_request_log / put` (`crates/gateway/src/management/observability.rs:253`) | Configures request/response payload recording parameters. |
| 73 | `/request-retry` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH RequestRetry` (`server_management.go:108-110`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:121`) | Get/set maximum request retry attempts. |
| 74 | `/reset-quota` | Routing & Quota | `same-path` | — | `mgmt.POST s.mgmt.ResetQuota` (`server_management.go:77`) | `core::reset_quota` (`crates/gateway/src/management/core.rs:177`) | Resets rate limit, quota counter, and backoff states across accounts and providers. |
| 75 | `/responses` | Responses & Search | `different-path` | — | `v1.GET/POST ResponsesWebsocket/Responses` (`server_routes.go:77-78`) | `cp_routes::responses / ws_upgrade` (`crates/gateway/src/routes.rs:50`) | Upstream mounts on `/v1/responses` and `/backend-api/codex/responses`; both mounted in mahoquot. Unprefixed alias candidate. |
| 76 | `/responses/compact` | Responses & Search | `different-path` | — | `v1.POST openaiResponsesHandlers.Compact` (`server_routes.go:79`) | `cp_routes::responses_compact` (`crates/gateway/src/routes.rs:54`) | Upstream mounts on `/v1/responses/compact` & `/backend-api/codex/responses/compact`; mahoquot mounts at both. Compact format proxy. |
| 77 | `/routing/strategy` | Routing & Quota | `same-path` | — | `mgmt.GET/PUT/PATCH RoutingStrategy` (`server_management.go:122-124`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:162`) | Get/set credential selection strategy (`round-robin`, `weighted-round-robin`, `fill-first`). |
| 78 | `/selected` | Test Harness & Diagnostics | `deferred` | — | `engine.GET("/selected") (test fixture)` (`internal/logging/cpa_trace_test.go:41`) | `—` (`Not implemented`) | Deferred: internal Go CPA trace test fixture verifying span selection events; non-production route. |
| 79 | `/status` | Test Harness & Diagnostics | `deferred` | — | `router.GET("/status") (test fixture)` (`oauth_sessions_test.go:100`) | `—` (`Not implemented`) | Deferred: test fixture / plugin example route; canonical gateway endpoint is `/get-auth-status`. |
| 80 | `/unselected` | Test Harness & Diagnostics | `deferred` | — | `engine.GET("/unselected") (test fixture)` (`internal/logging/cpa_trace_test.go:46`) | `—` (`Not implemented`) | Deferred: internal Go CPA trace test fixture verifying unselected spans; non-production route. |
| 81 | `/usage-queue` | API Key Management | `same-path` | — | `mgmt.GET s.mgmt.GetUsageQueue` (`server_management.go:84`) | `apikeys::usage_queue` (`crates/gateway/src/management/apikeys.rs:135`) | Returns pending usage queue backlog (upstream uses Redis; mahoquot persists inline and returns empty `[]`). |
| 82 | `/usage-statistics-enabled` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH UsageStatisticsEnabled` (`server_management.go:59-61`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:110`) | Get/set boolean flag enabling usage metrics collection. |
| 83 | `/v0/management/config` | Config & Settings | `same-path` | — | `mgmt.GET s.mgmt.GetConfig` (`server_management.go:30`) | `management::core::get_config` (`crates/gateway/src/routes.rs:135`) | Direct absolute path to JSON management configuration document. |
| 84 | `/v0/management/oauth-callback` | Auth & Identity | `same-path` | — | `s.engine.GET/POST oauth-callback` (`server_management.go:24-25`) | `management::oauth::oauth_callback` (`crates/gateway/src/routes.rs:129`) | Direct absolute path for management OAuth callback processing. |
| 85 | `/v1/live` | Realtime & Live | `same-path` | — | `v1.POST s.codexLiveHandler.Handle` (`server_routes.go:81`) | `cp_routes::realtime_offer` (`crates/gateway/src/routes.rs:75`) | Canonical `/v1/live` WebRTC offer endpoint. |
| 86 | `/v1/live/:call_id` | Realtime & Live | `same-path` | — | `v1.GET s.codexLiveHandler.HandleSideband` (`server_routes.go:82`) | `cp_routes::live_sideband` (`crates/gateway/src/routes.rs:76`) | Canonical `/v1/live/{call_id}` WebRTC live sideband streaming channel. |
| 87 | `/v1/realtime` | Realtime & Live | `same-path` | — | `s.engine.GET/POST("/v1/realtime")` (`server_routes.go:87-88`) | `cp_routes::ws_upgrade / realtime_offer` (`crates/gateway/src/routes.rs:77`) | OpenAI Realtime API websocket upgrade and session initiation. |
| 88 | `/v1/realtime/calls` | Realtime & Live | `same-path` | — | `s.engine.POST("/v1/realtime/calls")` (`server_routes.go:89`) | `cp_routes::realtime_offer` (`crates/gateway/src/routes.rs:81`) | Realtime WebRTC call negotiation endpoint. |
| 89 | `/v1/realtime/calls/:call_id` | Realtime & Live | `same-path` | — | `s.engine.GET("/v1/realtime/calls/:call_id")` (`server_routes.go:90`) | `cp_routes::realtime_call_get` (`crates/gateway/src/routes.rs:82`) | Retrieve status and metadata for active realtime call. |
| 90 | `/v1/realtime/calls/:call_id/accept` | Realtime & Live | `same-path` | — | `s.engine.POST HandleSIPControl` (`server_routes.go:98`) | `cp_routes::realtime_sip_accept` (`crates/gateway/src/routes.rs:136`) | Explicit SIP call accept handler returning 501 capability_not_supported. |
| 91 | `/v1/realtime/calls/:call_id/hangup` | Realtime & Live | `same-path` | — | `s.engine.POST HandleHangup` (`server_routes.go:97`) | `cp_routes::realtime_hangup` (`crates/gateway/src/routes.rs:86`) | Terminates active realtime audio/WebRTC call. |
| 92 | `/v1/realtime/calls/:call_id/refer` | Realtime & Live | `same-path` | — | `s.engine.POST HandleSIPControl` (`server_routes.go:100`) | `cp_routes::realtime_sip_refer` (`crates/gateway/src/routes.rs:144`) | Explicit SIP call transfer/refer handler returning 501 capability_not_supported. |
| 93 | `/v1/realtime/calls/:call_id/reject` | Realtime & Live | `same-path` | — | `s.engine.POST HandleSIPControl` (`server_routes.go:99`) | `cp_routes::realtime_sip_reject` (`crates/gateway/src/routes.rs:140`) | Explicit SIP call reject handler returning 501 capability_not_supported. |
| 94 | `/v1/realtime/client_secrets` | Realtime & Live | `same-path` | — | `s.engine.POST CreateClientSecret` (`server_routes.go:91`) | `cp_routes::realtime_client_secrets` (`crates/gateway/src/routes.rs:94`) | Generates ephemeral client credentials for browser Realtime WebRTC connections. |
| 95 | `/v1/realtime/sessions` | Realtime & Live | `same-path` | — | `s.engine.POST CreateLegacySession` (`server_routes.go:92`) | `cp_routes::realtime_sessions` (`crates/gateway/src/routes.rs:98`) | Creates Realtime session tokens and configuration. |
| 96 | `/v1/realtime/transcription_sessions` | Realtime & Live | `same-path` | — | `s.engine.POST HandleTranscriptionSession` (`server_routes.go:93`) | `cp_routes::realtime_transcription` (`crates/gateway/src/routes.rs:99`) | Initializes realtime speech transcription session. |
| 97 | `/v1/realtime/translations` | Realtime & Live | `same-path` | — | `s.engine.GET/POST HandleTranslation` (`server_routes.go:94-95`) | `cp_routes::realtime_translations` (`crates/gateway/src/routes.rs:103`) | Realtime translation streaming session handler (GET/POST). |
| 98 | `/v1/realtime/translations/client_secrets` | Realtime & Live | `same-path` | — | `s.engine.POST HandleTranslation` (`server_routes.go:96`) | `cp_routes::realtime_translations` (`crates/gateway/src/routes.rs:107`) | Generates ephemeral client secret for realtime translation session. |
| 99 | `/v1/responses` | Responses & Search | `same-path` | — | `v1.GET/POST ResponsesWebsocket/Responses` (`server_routes.go:77-78`) | `cp_routes::responses / ws_upgrade` (`crates/gateway/src/routes.rs:50`) | Canonical `/v1/responses` OpenAI Responses API endpoint. |
| 100 | `/v1/responses/compact` | Responses & Search | `same-path` | — | `v1.POST openaiResponsesHandlers.Compact` (`server_routes.go:79`) | `cp_routes::responses_compact` (`crates/gateway/src/routes.rs:54`) | Canonical `/v1/responses/compact` compact response format endpoint. |
| 101 | `/v1/responses/ws` | Responses & Search | `deferred` | — | `s.AttachWebsocketRoute (test harness)` (`logging/request_logger_home_test.go:175`) | `—` (`Not implemented`) | Deferred: test fixture route for custom websocket attachment; production websockets upgrade on `/v1/responses`. |
| 102 | `/v1beta/models/*action` | Core Inference & Chat | `same-path` | — | `v1beta.POST/GET GeminiHandler` (`server_routes.go:126-127`) | `cp_routes::v1beta_action` (`crates/gateway/src/routes.rs:112`) | Canonical `/v1beta/models/{*action}` Gemini action dispatcher. |
| 103 | `/vertex-api-key` | Provider Key Config Lists | `partial` | P0 | `mgmt.GET/PUT/PATCH/DELETE VertexCompatKeys` (`server_management.go:146-149`) | `apikeys::KEY_LISTS` (`crates/gateway/src/management/apikeys.rs:50`) | Management CRUD in `apikeys.rs`; wiring stored keys into relay credential pool resolution completed in Todo 2. |
| 104 | `/vertex/import` | Auth & Identity | `same-path` | — | `mgmt.POST s.mgmt.ImportVertexCredential` (`server_management.go:174`) | `creds::vertex_import` (`crates/gateway/src/management/creds.rs:934`) | Imports Google Cloud service account JSON credentials for Vertex AI. |
| 105 | `/videos` | Media / Images & Video | `different-path` | — | `v1.POST, openaiV1.POST` (`server_routes.go:70,105`) | `cp_routes::videos / cp_routes::openai_videos` (`crates/gateway/src/routes.rs:61,66`) | Upstream mounts on `/v1/videos` and `/openai/v1/videos`; both mounted in mahoquot. Unprefixed `/videos` alias candidate. |
| 106 | `/videos/:request_id` | Media / Images & Video | `different-path` | — | `v1.GET XAIVideosRetrieve` (`server_routes.go:74`) | `cp_routes::videos_by_id` (`crates/gateway/src/routes.rs:65`) | Upstream mounts on `/v1/videos/:request_id`; mahoquot mounts at `/v1/videos/{request_id}`. Unprefixed alias candidate. |
| 107 | `/videos/:video_id` | Media / Images & Video | `different-path` | — | `openaiV1.GET VideosRetrieve` (`server_routes.go:107`) | `cp_routes::openai_videos` (`crates/gateway/src/routes.rs:67`) | Upstream mounts on `/openai/v1/videos/:video_id`; mahoquot mounts at `/openai/v1/videos/{video_id}`. Unprefixed alias candidate. |
| 108 | `/videos/:video_id/content` | Media / Images & Video | `different-path` | — | `openaiV1.GET VideosContent` (`server_routes.go:106`) | `cp_routes::openai_videos` (`crates/gateway/src/routes.rs:71`) | Upstream mounts on `/openai/v1/videos/:video_id/content`; mahoquot mounts at `/openai/v1/videos/{video_id}/content`. |
| 109 | `/videos/edits` | Media / Images & Video | `different-path` | — | `v1.POST XAIVideosEdits` (`server_routes.go:72`) | `cp_routes::videos` (`crates/gateway/src/routes.rs:63`) | Upstream mounts on `/v1/videos/edits`; mahoquot mounts at `/v1/videos/edits`. Video editing inference proxy. |
| 110 | `/videos/extensions` | Media / Images & Video | `different-path` | — | `v1.POST XAIVideosExtensions` (`server_routes.go:73`) | `cp_routes::videos` (`crates/gateway/src/routes.rs:64`) | Upstream mounts on `/v1/videos/extensions`; mahoquot mounts at `/v1/videos/extensions`. Video extension inference proxy. |
| 111 | `/videos/generations` | Media / Images & Video | `different-path` | — | `v1.POST XAIVideosGenerations` (`server_routes.go:71`) | `cp_routes::videos` (`crates/gateway/src/routes.rs:62`) | Upstream mounts on `/v1/videos/generations`; mahoquot mounts at `/v1/videos/generations`. Video generation inference proxy. |
| 112 | `/ws-auth` | Config & Settings | `same-path` | — | `mgmt.GET/PUT/PATCH WebsocketAuth` (`server_management.go:104-106`) | `scalar_table::SCALARS` (`crates/gateway/src/management/scalar_table.rs:149`) | Get/set boolean flag enforcing authentication on websocket upgrades. |
| 113 | `/xai-api-key` | Provider Key Config Lists | `partial` | P0 | `mgmt.GET/PUT/PATCH/DELETE XAIKeys` (`server_management.go:136-139`) | `apikeys::KEY_LISTS` (`crates/gateway/src/management/apikeys.rs:45`) | Management CRUD in `apikeys.rs`; wiring stored keys into relay credential pool resolution completed in Todo 2. |
| 114 | `/xai-auth-url` | Auth & Identity | `same-path` | — | `mgmt.GET s.mgmt.RequestXAIToken` (`server_management.go:180`) | `xai_auth_url_handler` (`crates/gateway/src/management/oauth.rs:2391`) | Generates xAI OAuth authorization URL and initializes pending login session. |

---

## 3. Subsystem Rollup Ledger

Upstream CLIProxyAPI contains several large internal subsystems. This ledger documents upstream roles, downstream equivalents in `mahoquot-proxy`, and explicit deferral rationales.

| Subsystem | Files | Upstream Role | mahoquot-proxy Equivalent / Deferral Reason | Status |
| --- | ---: | --- | --- | --- |
| **`internal/pluginhost`** | 62 | Cgo/DLL/so dynamic plugin loader, JS runtime (Goja), Python bridge, WASM executor, plugin store repository, and dynamic request interceptors. | **Deferred**: Native Rust architecture handles request transformation and provider adapters in compiled crate lanes (`crates/providers`). Dynamic runtime plugin hosting is out of scope for this proxy iteration. | `deferred` |
| **`internal/signature`** | 17 | Request/response signing, payload sanitization (Gemini, Claude messages, Kimi), thinking token transforms, and client compatibility normalizers. | **Implemented in Providers**: Handled directly in `crates/providers` via strongly-typed AST transforms and stream filters. Request-signing engine deferred as unneeded for standard OAuth/API-key upstream authentication. | `implemented` / `deferred` |
| **`internal/tui`** | 13 | Interactive terminal UI built with Bubbletea/Lipgloss providing OAuth login screens, live logs viewer, key manager, and model catalogs. | **Deferred**: mahoquot-proxy uses the browser-based Vue/TypeScript console (`crates/monitor-ui`) served via `/management.html` and REST management endpoints (`crates/gateway/src/management/`). In-terminal TUI is deferred. | `deferred` |
| **`internal/redisqueue`** | 5 | Distributed usage logging queue, token rate limiting buffer, and background worker queue backed by Redis. | **Deferred / Native Inline**: mahoquot-proxy persists usage statistics inline via atomic state and SQLite storage without requiring an external Redis deployment. `/v0/management/usage-queue` returns an empty list. | `deferred` |
| **`internal/wsrelay`** | 4 | Bidirectional WebSocket session manager for realtime audio, WebRTC data channels, and Responses streaming. | **Implemented**: Handled natively in `crates/gateway/src/cp_routes.rs` and `relay.rs` using Axum WebSocket upgrades and Tokio async streaming. | `same-path` |
| **`internal/translator`** | 3 | Cross-format conversion between OpenAI, Claude Messages, and Gemini Interactions request/response schemas. | **Implemented**: Handled in `crates/providers` via provider request adapters and SSE stream translators. | `same-path` |
| **`internal/modelconfig`** | 3 | Static model catalogs, capabilities mapping, token limits, context length hashing, and release channels. | **Implemented**: Handled in `crates/gateway/src/models_route.rs` and `crates/gateway/src/management/creds.rs` (`/model-definitions/{channel}`). | `same-path` |
| **`internal/credentialweight`** | 2 | Weighted round-robin algorithm for account load balancing based on dynamic error rates and configured weights. | **Implemented**: Handled in `crates/pool` routing strategies (`round-robin`, `weighted-round-robin`, `fill-first`). | `same-path` |
| **`internal/safemode`** | 2 | Default/example API keys detector to prevent running in production with exposed demo credentials. | **Implemented**: Handled at gateway startup and inbound auth middleware; empty/wildcard credentials are rejected by default. | `same-path` |

---

## 4. Mount-Difference Aliases Ledger

The raw 114 endpoint extraction contains routes extracted relative to Gin router groups (such as `v1 := engine.Group("/v1")`, `v1beta := engine.Group("/v1beta")`, `openaiV1 := engine.Group("/openai/v1")`). In `mahoquot-proxy`, canonical routes are mounted under their standardized API paths. Todo 4 registers compatibility aliases so clients issuing requests to either the root path or the versioned prefix resolve identically.

| Raw Upstream Baseline String | Canonical Upstream Mount | mahoquot-proxy Mount | Alias Strategy (Todo 4) |
| --- | --- | --- | --- |
| `/alpha/search` | `/v1/alpha/search`, `/backend-api/codex/alpha/search` | `/v1/alpha/search`, `/backend-api/codex/alpha/search` | Add top-level alias route `POST /alpha/search` |
| `/chat/completions` | `/v1/chat/completions` | `/v1/chat/completions` | Canonical `/v1/chat/completions` (OpenAI standard) |
| `/completions` | `/v1/completions` | `/v1/completions` | Canonical `/v1/completions` (OpenAI standard) |
| `/images/edits` | `/v1/images/edits` | `/v1/images/edits` | Canonical `/v1/images/edits` (OpenAI standard) |
| `/images/generations` | `/v1/images/generations` | `/v1/images/generations` | Canonical `/v1/images/generations` (OpenAI standard) |
| `/interactions` | `/v1beta/interactions` | `/v1beta/interactions` | Add top-level alias route `POST /interactions` |
| `/live` | `/v1/live` | `/v1/live` | Canonical `/v1/live` |
| `/live/:call_id` | `/v1/live/:call_id` | `/v1/live/{call_id}` | Add top-level alias route `GET /live/{call_id}` |
| `/messages` | `/v1/messages` | `/v1/messages` | Canonical `/v1/messages` (Anthropic standard) |
| `/messages/count_tokens` | `/v1/messages/count_tokens` | `/v1/messages/count_tokens` | Canonical `/v1/messages/count_tokens` (Anthropic standard) |
| `/models` | `/v1/models`, `/v1beta/models` | `/v1/models`, `/v1beta/models` | Canonical provider mounts |
| `/models/*action` | `/v1beta/models/*action` | `/v1beta/models/{*action}` | Add top-level alias route `/models/{*action}` |
| `/oauth-callback` | `/v0/management/oauth-callback` | `/v0/management/oauth-callback` | Add top-level alias route `GET /oauth-callback` |
| `/responses` | `/v1/responses`, `/backend-api/codex/responses` | `/v1/responses`, `/backend-api/codex/responses` | Add top-level alias route `POST /responses` |
| `/responses/compact` | `/v1/responses/compact` | `/v1/responses/compact` | Add top-level alias route `POST /responses/compact` |
| `/videos` | `/v1/videos`, `/openai/v1/videos` | `/v1/videos`, `/openai/v1/videos` | Add top-level alias route `POST /videos` |
| `/videos/:request_id` | `/v1/videos/:request_id` | `/v1/videos/{request_id}` | Add top-level alias route `GET /videos/{request_id}` |
| `/videos/:video_id` | `/openai/v1/videos/:video_id` | `/openai/v1/videos/{video_id}` | Add top-level alias route `GET /videos/{video_id}` |
| `/videos/:video_id/content` | `/openai/v1/videos/:video_id/content` | `/openai/v1/videos/{video_id}/content` | Add top-level alias route `GET /videos/{video_id}/content` |
| `/videos/edits` | `/v1/videos/edits` | `/v1/videos/edits` | Canonical `/v1/videos/edits` |
| `/videos/extensions` | `/v1/videos/extensions` | `/v1/videos/extensions` | Canonical `/v1/videos/extensions` |
| `/videos/generations` | `/v1/videos/generations` | `/v1/videos/generations` | Canonical `/v1/videos/generations` |

---

## 5. Spot-Check Audit Records

Five endpoints across different functional areas were spot-checked against the actual Go source handlers in `/tmp/cliproxy` and the Rust handlers in `crates/gateway/src/`:

### Spot-Check 1: `/auth-files/status` (`Auth & Identity`)
- **Go Source**: `internal/api/handlers/management/auth_files_fields.go:25` (`PatchAuthFileStatus`)
- **Go Behavior**: Parses JSON `{name: string, auth_index: string, disabled: bool}`; validates required fields and non-empty name; verifies auth file existence and updates disabled state.
- **Rust Source**: `crates/gateway/src/management/creds.rs:540` (`patch_auth_file_status`)
- **Rust Parity**: Exact behavioral parity; validates required JSON fields (`name`, `disabled`), sanitizes name against directory traversal, and writes updated status.
- **Result**: `same-path` confirmed.

### Spot-Check 2: `/routing/strategy` (`Routing & Quota`)
- **Go Source**: `internal/api/handlers/management/config_basic.go:312` (`GetRoutingStrategy` / `PutRoutingStrategy`)
- **Go Behavior**: Normalizes strategy string against `round-robin` (`roundrobin`, `rr`), `weighted-round-robin` (`wrr`), and `fill-first` (`fillfirst`, `ff`). Rejects unknown with 400 `invalid strategy`.
- **Rust Source**: `crates/gateway/src/management/scalar_table.rs:60,162` (`normalize_routing_strategy`)
- **Rust Parity**: Exact normalizer implementation; returns canonical names and rejects invalid inputs with identical 400 `invalid strategy` error message.
- **Result**: `same-path` confirmed.

### Spot-Check 3: `/interactions` (`Core Inference & Chat`)
- **Go Source**: `internal/api/server_routes.go:125` (`v1beta.POST("/interactions", geminiHandlers.Interactions)`) and `sdk/api/handlers/gemini/interactions_handlers.go:93`
- **Go Behavior**: Handles Google Gemini native Interactions protocol requests under `/v1beta/interactions`.
- **Rust Source**: `crates/gateway/src/routes.rs:116` (`/v1beta/interactions` -> `cp_routes::v1beta_interactions`)
- **Rust Parity**: Mounted at `/v1beta/interactions`. Endpoint diff reveals raw list extracted `/interactions` relative path. Alias to be exposed in Todo 4.
- **Result**: `different-path` confirmed.

### Spot-Check 4: `/plugins/:id/enabled` (`Plugins & Extensions`)
- **Go Source**: `internal/api/handlers/management/plugins.go:213` (`PatchPluginEnabled`)
- **Go Behavior**: Dynamically enables/disables loaded Goja/Cgo plugin instances in `pluginhost`.
- **Rust Source**: `crates/gateway/src/management/plugins.rs:93` (unmerged)
- **Rust Parity**: Pluginhost runtime is deferred per project scope; Rust proxy uses native compiled provider lanes.
- **Result**: `deferred` confirmed.

### Spot-Check 5: `/v1/realtime/calls/:call_id/hangup` (`Realtime & Live`)
- **Go Source**: `internal/api/server_routes.go:97` (`s.engine.POST("/v1/realtime/calls/:call_id/hangup", standardAuth, s.codexLiveHandler.HandleHangup)`)
- **Go Behavior**: Accepts POST request with call ID parameter to terminate WebRTC media stream session.
- **Rust Source**: `crates/gateway/src/routes.rs:86` (`/v1/realtime/calls/{call_id}/hangup` -> `cp_routes::realtime_hangup`)
- **Rust Parity**: Exact route path and verb; handles call termination on active sessions.
- **Result**: `same-path` confirmed.


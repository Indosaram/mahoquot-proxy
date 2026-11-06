# mahoquot-proxy

High-concurrency LLM inference proxy and account router written in Rust. Distributes OpenAI, Anthropic, Gemini, Claude, Cursor, Kiro, Vertex, and Z-code traffic across multiple upstream accounts with sequence-stamped round-robin fairness, automatic token refreshing, in-flight failover, and lock-free runtime configuration.

## Quickstart

### Installation & Build

```bash
cargo build --release --bin mahoquot-gateway
```

### Running the Gateway

```bash
# Start proxy server pointing to an auth directory
./target/release/mahoquot-gateway serve --auth-dir ~/.mahoquot/auth --port 18801

# Or with environment variables
AUTH_DIR=~/.mahoquot/auth GATEWAY_PORT=18801 ./target/release/mahoquot-gateway
```

### CLI Commands

- `serve`: Run the proxy server (default command when no subcommand is provided).
- `doctor`: Validate authentication directory, accounts, and config files offline.
- `accounts`: Dump status table of all configured provider accounts.

```bash
# Run doctor on an auth directory
mahoquot-gateway doctor --auth-dir ~/.mahoquot/auth

# List account statuses
mahoquot-gateway accounts --auth-dir ~/.mahoquot/auth
```

## Configuration & Environment Variables

Every setting can be specified via CLI flag or environment variable (CLI flags take precedence):

| CLI Flag | Environment Variable | Default | Description |
|---|---|---|---|
| `--port <PORT>` | `GATEWAY_PORT` | `18801` | TCP port the gateway listens on |
| `--auth-dir <PATH>` | `AUTH_DIR` | *(Required)* | Directory storing provider credential JSON files |
| `--strategy <STRAT>` | `STRATEGY` | `round_robin` | Routing strategy (`round_robin` or `fill_first`) |
| `--max-failover <N>` | `MAX_FAILOVER` | `3` | Maximum failover retry attempts across accounts |
| `--config <PATH>` | `CONFIG_PATH` | `<AUTH_DIR>/config.yaml` | Path to persistent YAML settings file |
| `--api-keys <KEYS>` | `API_KEYS` | *(Empty / open)* | Comma-separated inbound bearer API keys |
| `--models <MODELS>` | `MODELS` | *(None)* | Model override/mapping configuration |
| `--refresh-url <URL>` | `REFRESH_URL` | *(Default OAuth)* | OAuth token refresh endpoint URL |
| `--auth-refresh <BOOL>`| `AUTH_REFRESH` | `true` | Enable automatic background token refreshing |
| `--usage-poll-secs <S>`| `USAGE_POLL_SECS` | `120` | Interval in seconds for polling provider usage endpoints |
| `--log-level <LEVEL>` | `LOG_LEVEL` | `info` | Logging filter level (`trace`, `debug`, `info`, `warn`, `error`) |

## Provider Matrix

| Provider | Authentication Modes | Relayed Protocols | Quota & Telemetry |
|---|---|---|---|
| **Codex** | OAuth (token exchange), API key | OpenAI Chat, Codex Responses | Header-based rate limit tracking & Wham usage endpoint |
| **Claude / Anthropic** | OAuth PKCE, API key, Relay Key | Anthropic Messages, OpenAI Chat | OAuth usage API, unified rate limit headers |
| **Cursor** | OAuth PKCE | Connect Protobuf, Agent Client | Individual plan usage breakdown |
| **Kiro** | OAuth (Social & PKCE) | Kiro EventStream | Usage breakdown & token limits |
| **Z-code** | Provisioned composite key pair | Anthropic Messages | Next reset time & limits percentage |
| **Antigravity / Gemini** | OAuth PKCE | Google Generative Language | Rate limit header tracking |
| **Vertex AI** | Service Account JSON | Google Generative Language | Project quotas & OAuth token exchange |
| **Generic Adapters** | API Key, OAuth | OpenAI Chat, Anthropic Messages | Custom base URL forwarding |

## Unified Runtime Model Registry

Mahoquot includes a centralized, offline-first runtime model registry (`mahoquot-registry`) that unifies model discovery, request routing, capability gating, and model list APIs (`/v1/models`, `/v1beta/models`).

New provider models, capabilities, and aliases can be added dynamically via a cryptographically signed catalog distributed on the orphan branch `model-catalog-v1` without rebuilding or reinstalling the gateway binary.

### Provider Admission Policy Matrix

| Policy | Providers | Dynamic Discovery | Codex Negative Space Fallback |
|---|---|---|---|
| **`Closed`** | `antigravity`, `claude`, `cursor`, `kiro`, `vertex`, `zcode` | Prohibited. Must be declared via signed catalog or local operator overrides. | Ineligible. Cannot claim arbitrary IDs. |
| **`Discovered`** | Generic OpenAI-compatible accounts with explicit discovery | Permitted within authority mask. Cannot overwrite signed catalog capabilities. | Ineligible. |
| **`Open`** | `codex` | Unmapped models fall back to Codex open negative space. | Yes (cannot steal closed provider IDs). |

### Source Precedence
1. **Local Override** (Highest: `config.yaml` aliases and exclusions)
2. **Authoritative Discovery** (Generic `/models` discovery)
3. **Remote Signed Catalog** (Published to `model-catalog-v1` branch)
4. **Last-Known-Good (LKG) Cache** (`~/.mahoquot/cache/models-v1.signed.json`, mode 0600)
5. **Embedded Fallback** (Compiled into binary)

For comprehensive details on catalog format, signing keys, key rotation, publication workflows, and operator tooling, see the [Model Catalog Publication Guide](docs/catalog-publication.md).


## Routing Strategies

- **Strict Round-Robin (`round_robin`)**: Requests rotate evenly across healthy accounts matching the requested model. Unhealthy accounts enter cooldown and bypass selection until restored.
- **Fill First (`fill_first`)**: Directs traffic to the first available healthy account until its rate limit or quota is exhausted, then fails over to subsequent accounts in pool order.

## Claude Account Safety

Claude subscriptions (OAuth) are the risky lane; Claude API keys (`x-api-key`, a Claude credential file with `api_key` set, imported with `POST /auth-files/import`) are the sanctioned one.

- **`claude-route-mode`**: `oauth_first` (default, unchanged order), `api_key_first` (API-key accounts are tried before OAuth accounts), `api_key_only` (OAuth accounts never receive a request). `api_key_first` is a preference: an existing session binding can still land on an OAuth account. Only `api_key_only` guarantees no OAuth use.
- **`claude-api-key-budgets`**: `{<account id>: <units per calendar month, UTC>}`. Units are weighted tokens, `input + output + cache_write + cache_read / 10`, summed from the request history since the first of the month. Convert a dollar budget with the current price sheet for the models you use. This is a rough threshold, not Anthropic's billing. If the history cannot be read, budgeted accounts are skipped (fail closed).
- **Suspected accounts**: a Claude 400/401/403 whose message reads like enforcement (organization disabled, terms of service, credential only authorized for Claude Code, etc.), or a second plain 403 within 10 minutes, puts the account on `claude-suspected-accounts` and returns the upstream error to the client without trying another account. Clear it with `DELETE /v0/management/accounts/{id}/suspected` after checking the account.
- **Session pinning**: Claude OAuth sessions are pinned to one account in `claude-affinity.store` in the auth directory (6 h idle TTL). A pinned account in a cooldown under 2 minutes answers `429` with `retry-after` instead of moving the session. Transient 5xx/529 failover serves one turn elsewhere without moving the pin. `x-client-request-id` is not a session key on Claude-only routes.
- **Warmup**: Claude probes skip 01:00-08:00 local time, wait 3-25 minutes (fixed per account and window) after a reset, and run once per window. Turning `warmup.providers.claude.enabled` off is still the quietest option.
- **`claude-account-egress`**: `{<account id>: <http|https|socks5|socks5h URL>}` sends that account's traffic through its own proxy. An entry that does not build is refused at save; one that fails at boot keeps the account out of rotation instead of using the host IP.
- **Client profile**: with cloaking on (default), OAuth requests present the Claude Agent SDK (TypeScript) identity in `crates/providers/src/claude.rs` (`AGENT_SDK_IDENTITY_HEADERS`, fixture `crates/gateway/tests/fixtures/agent_sdk_wire.json`). `disable-claude-cloak-mode: true` sends only auth, version, `anthropic-beta: oauth-2025-04-20` plus the client's betas and the client's own identity headers, without the injected system line or cache breakpoints.
  - **Recapture after an Agent SDK upgrade**: install the new `@anthropic-ai/claude-agent-sdk`, point `ANTHROPIC_BASE_URL` at a local listener with a dummy token, send one request twice, keep the headers equal in both runs (drop `x-claude-code-session-id`, never copy the billing block or `metadata.user_id`), then update the constant and the fixture together.
- **Visibility**: `/stats` account entries for Claude carry a `claude` object (route mode, suspected reason/time, pinned sessions, probes in 24 h, egress host only).
- **Measurement**: `scripts/claude_switch_cache.sql` (run with `sqlite3 -readonly`) reports account switches inside sessions and their cache-write share for a time window.

## Desktop App Integration

If running alongside the Mahoquot desktop monitor app, you can point the app to a custom proxy build using:

```bash
export MAHOQUOT_GATEWAY_BIN="/path/to/mahoquot-proxy/target/release/mahoquot-gateway"
```

## Console UI

The gateway serves the operations console embedded from `ui/index.html` (via `include_str!("../../../ui/index.html")` in `crates/gateway/src/static_pages.rs`).

After changing the frontend in `quotio-rs/crates/monitor-ui/frontend`:
1. Build the frontend and sync the artifact:
   ```bash
   cd /path/to/quotio-rs/crates/monitor-ui/frontend
   bun run build
   bun run sync:proxy
   ```
2. Rebuild and restart the gateway binary (`cargo build` / `cargo run`), because `ui/index.html` is embedded at compile time via `include_str!`. Running `sync:proxy` updates the repository copy in `ui/index.html`, but a gateway rebuild is required for the served UI to reflect the updated bundle.

## Testing & Benchmarks

```bash
# Run all workspace tests
cargo test --workspace

# Run deterministic benchmark harness
cargo build --release -p bench
./target/release/bench mock --port 18850 --ttft-ms 40 --chunks 20
./target/release/bench run --concurrency 500 --total 2000
```

## License

Dual-licensed under either of:
- MIT License ([LICENSE](LICENSE) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE](LICENSE) or http://www.apache.org/licenses/LICENSE-2.0)

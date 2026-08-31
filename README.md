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

## Routing Strategies

- **Strict Round-Robin (`round_robin`)**: Requests rotate evenly across healthy accounts matching the requested model. Unhealthy accounts enter cooldown and bypass selection until restored.
- **Fill First (`fill_first`)**: Directs traffic to the first available healthy account until its rate limit or quota is exhausted, then fails over to subsequent accounts in pool order.

## Desktop App Integration

If running alongside the Mahoquot desktop monitor app, you can point the app to a custom proxy build using:

```bash
export MAHOQUOT_GATEWAY_BIN="/path/to/mahoquot-proxy/target/release/mahoquot-gateway"
```

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

# GATEWAY SUBSYSTEM

## OVERVIEW
Multi-protocol AI inference proxy engine built on Axum 0.8 and Hyper 1.0. Relays inbound client requests (OpenAI/Anthropic/Gemini) to upstream provider accounts with transparent failover, token refreshing, and dynamic settings management.

## STRUCTURE
```
crates/gateway/
├── src/
│   ├── main.rs            # Binary entry point, state setup, listener binding
│   ├── state.rs           # Central AppState (pool, router, client, metrics)
│   ├── relay.rs           # Core proxy & failover engine (741 lines hotspot)
│   ├── routes.rs          # HTTP route table & handler dispatch
│   ├── cp_routes.rs       # WebSocket upgrades, realtime & compatibility routes
│   ├── quota.rs           # Upstream usage/quota poller
│   ├── metrics.rs         # Prometheus metrics & stats aggregation
│   ├── config.rs          # Environment & static configuration loader
│   ├── compat/            # Multi-protocol request/response translation engine
│   └── management/        # ArcSwap-backed dynamic settings & admin APIs
├── tests/                 # Integration test suite (t1..t10)
└── Cargo.toml             # Gateway crate manifest
```

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Request proxying & failover | `crates/gateway/src/relay.rs` | Upstream streaming, status classification, retry logic |
| Route registration | `crates/gateway/src/routes.rs` | Maps `/v1/*`, `/admin/*`, `/management/*` |
| Shared application state | `crates/gateway/src/state.rs` | `AppState`, member pool, active router |
| Dynamic settings updates | `crates/gateway/src/management/store.rs` | Lockless reads via `ArcSwapOption` |
| Upstream auth & refresh | `crates/gateway/src/relay.rs` | Triggers token refresh on 401 before retry |
| Integration tests | `crates/gateway/tests/` | t1 (fairness) through t10 (Anthropic compat) |

## CONVENTIONS
- **Lock-free Read Hotpath**: Read configuration and dynamic settings through `ArcSwap` / `ArcSwapOption` rather than `RwLock` or `Mutex`.
- **Zero In-flight Buffering**: Stream SSE and chunked responses byte-for-byte directly from upstream to client unless protocol translation is active.
- **Failover Scope**: Failover is only permissible before response bytes are committed to the downstream client.

## ANTI-PATTERNS (THIS SUBTREE)
- **DO NOT** block tokio executor threads with synchronous filesystem or network I/O.
- **DO NOT** allocate or buffer entire large streaming bodies in memory before proxying.
- **NEVER** retry an upstream request once downstream response headers/chunks have been flushed.
- **NO** mutable global static state; pass `Arc<AppState>` through Axum state extractors.

## COMMANDS
```bash
cargo test -p mahoquot-gateway                  # Run gateway integration & unit tests
cargo run -p mahoquot-gateway                   # Start local gateway server on :18801
cargo check -p mahoquot-gateway                 # Validate syntax & type safety
```

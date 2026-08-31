# MANAGEMENT & SETTINGS SUBSYSTEM

## OVERVIEW
Admin and management control plane providing session authentication, brute-force protection, dynamic YAML persistence, and atomic runtime settings synchronization.

## STRUCTURE
```
crates/gateway/src/management/
├── mod.rs            # Management API router definition & handler mapping
├── store.rs          # SettingsStore with ArcSwap atomic pointers & YAML disk persistence
├── auth.rs           # Admin password validation, session tokens, brute-force rate limiter
├── gate.rs           # Inbound auth middleware & permission verification
├── settings.rs       # Core settings model & serialization
├── apikeys.rs        # Client API key administration
├── creds.rs          # Provider credential secret management
├── oauth.rs          # OAuth flow callbacks & account onboarding
├── lists.rs          # Provider and model whitelist/blacklist definitions
├── observability.rs  # Log levels, tracing hooks, and telemetry configuration
├── plugins.rs        # Plugin registration and runtime toggle hooks
├── scalars.rs        # Scalar settings accessors and validation
└── scalar_table.rs   # Tabular schema and metadata definitions for settings
```

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Settings storage & disk sync | `crates/gateway/src/management/store.rs` | Atomic swaps and YAML file persistence |
| Management authentication | `crates/gateway/src/management/auth.rs` | Token issuance, hashing, brute-force backoff |
| API key management | `crates/gateway/src/management/apikeys.rs` | Inbound client key storage and validation |
| Router endpoints | `crates/gateway/src/management/mod.rs` | `/management/*` Axum routes |

## CONVENTIONS
- **Atomic Pointer Updates**: Always update settings by writing to disk and atomically replacing the in-memory pointer using `ArcSwap::store`.
- **Brute-force Shielding**: Protect all login and token validation paths with attempt tracking and backoff in `auth.rs`.

## ANTI-PATTERNS (THIS SUBTREE)
- **DO NOT** hold file locks across asynchronous await points.
- **NEVER** log plaintext API keys, tokens, or provider secret credentials.
- **NO** unvalidated YAML deserialization; validate configuration integrity before swapping pointers.

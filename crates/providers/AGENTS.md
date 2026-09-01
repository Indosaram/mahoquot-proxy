# PROVIDERS SUBSYSTEM

## OVERVIEW
Credential loaders and authentication lifecycle adapters for upstream LLM providers, including Codex JSON accounts, Antigravity service configurations, and async OAuth token refresh.

## STRUCTURE
```
crates/providers/
├── src/
│   ├── lib.rs              # Upstream provider loader registry & exports
│   ├── account.rs          # Codex file-based credential parsing & auth headers
│   ├── antigravity.rs      # Antigravity account parsing & endpoint construction
│   ├── refresh.rs          # Pure OAuth refresh request/response payload builders
│   └── refresh_exec.rs     # Async network client executing token refresh requests
└── Cargo.toml              # Providers crate manifest
```

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Codex credential parsing | `crates/providers/src/account.rs` | Loads JSON files from `AUTH_DIR` |
| Antigravity account loading | `crates/providers/src/antigravity.rs` | Project ID, location & endpoint resolution |
| OAuth token refresh flow | `crates/providers/src/refresh_exec.rs` | Network execution for expired tokens |
| Refresh request construction | `crates/providers/src/refresh.rs` | Pure builders for token grant payloads |

## CONVENTIONS
- **File System Discovery**: Discover provider accounts from `~/.mahoquot/auth/codex-*.json` and Antigravity directory structures.
- **Split Construction & Execution**: Keep OAuth request payload creation pure in `refresh.rs`, delegating I/O execution to `refresh_exec.rs`.

## ANTI-PATTERNS (THIS SUBTREE)
- **DO NOT** hardcode OAuth refresh endpoints or client secrets in multiple locations.
- **NEVER** expose decrypted credentials, refresh tokens, or bearer tokens in debug logs.
- **NO** panic on corrupted JSON credentials; log a warning and skip invalid files.

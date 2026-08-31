# TYPES SUBSYSTEM

## OVERVIEW
Zero-dependency domain contracts, traits, error types, and shared models across all Quotio workspace crates.

## STRUCTURE
```
crates/types/
├── src/
│   └── lib.rs         # PoolMember trait, Health, Outcome, SessionHint, Strategy
└── Cargo.toml         # Types crate manifest
```

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| PoolMember trait | `crates/types/src/lib.rs` | Interface implemented by upstream account instances |
| Health models | `crates/types/src/lib.rs` | `Health` status enum and transition logic |
| Routing strategy | `crates/types/src/lib.rs` | `Strategy` enum (`StrictRoundRobin`, `FillFirst`) |
| Outcome reporting | `crates/types/src/lib.rs` | `Outcome` enum (`Success`, `Retryable`, `Unretryable`) |
| Session hints | `crates/types/src/lib.rs` | `SessionHint` type for sticky session binding |

## CONVENTIONS
- **Zero External Dependencies**: Keep `quotio-types` minimal with standard library types and optional serde.
- **Contract Boundary**: Define core abstractions here so upstream providers and downstream routers remain decoupled.

## ANTI-PATTERNS (THIS SUBTREE)
- **DO NOT** add heavy runtime dependencies (tokio, hyper, axum) to `quotio-types`.
- **NO** business logic or network code inside domain contracts.

## COMMANDS
```bash
cargo test -p quotio-types                    # Run types crate unit tests
cargo check -p quotio-types                   # Verify types compilation
```

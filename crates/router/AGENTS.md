# ROUTER SUBSYSTEM

## OVERVIEW
High-performance deterministic account selection algorithms over pools of upstream credentials, featuring strict sequence-stamped round-robin, churn immunity, and session affinity.

## STRUCTURE
```
crates/router/
├── src/
│   └── lib.rs         # Router implementation, strategies, and fairness tests
└── Cargo.toml         # Router crate manifest
```

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Selection logic | `crates/router/src/lib.rs` | `select()` implementing StrictRoundRobin & FillFirst |
| Health outcome reporting | `crates/router/src/lib.rs` | `report_outcome()` handling success, retryable, unretryable |
| Churn immunity | `crates/router/src/lib.rs` | Monotonic sequence counters preventing pointer resets |
| Session affinity | `crates/router/src/lib.rs` | Affinity cache binding session hints to healthy accounts |
| Selection fairness tests | `crates/router/src/lib.rs` | Unit tests proving round-robin distribution guarantees |

## CONVENTIONS
- **Sequence-Stamped Fairness**: Track last served sequences per member ID to maintain round-robin fairness when members enter or leave the pool.
- **Pure Algorithm Separation**: The router depends solely on `quotio-types` with zero network or I/O dependencies.

## ANTI-PATTERNS (THIS SUBTREE)
- **DO NOT** introduce asynchronous runtimes or network dependencies into `quotio-router`.
- **NEVER** mutate pool order dynamically in ways that break rotation monotonicity.
- **NO** deadlocks; use minimal scoped mutex/rwlock guards for pool pointer updates.

## COMMANDS
```bash
cargo test -p quotio-router                   # Run pure algorithm & fairness tests
cargo check -p quotio-router                  # Verify zero-dependency crate compilation
```

# L6 integration coordination (in progress)
- L6-05 needs root-owned events.rs / compat accumulator / replay normalization edits: add `CodexEvent::KiroRedactedReasoning(String)` and preserve in Kiro-specific opaque carrier -> `kiroRedactedReasoning`. This lane will emit the distinct variant in kiro.rs.
- L6-08 assigned owned path `crates/gateway/src/compat/interactions.rs` does not exist. Actual handler is `crates/gateway/src/interactions.rs`, outside lane ownership. Root must implement handler routing (or explicitly grant ownership); creating a disconnected compat file would not fix eligibility.
- L6-07 root must declare compat::command_code and wire relay dispatch; constructor/decoder details follow in l6-adapters.md final report.

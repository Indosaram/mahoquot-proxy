# Streaming compatibility repair (compat lane)

Owned product file: `crates/gateway/src/compat/mod.rs` only.

- **L5-02 support:** lines 94, 528-550, 650, 686-708 add a 300-second upstream-silence budget, pure saturating accounting helper, and per-poll `min(15s, remaining)` timeout. Only upstream-await elapsed time is charged; successful chunks reset the budget. Expiration drains the state, replaces/drops upstream with an empty stream, and queues the existing protocol error path with `upstream stall timeout`.
- **L5-03:** lines 596-601 emit the complete inert `: keepalive\n\n` SSE comment for Responses/Gemini ticks; chat/Anthropic arms are unchanged.
- **L5-04:** lines 206-230 keep absent/empty text block starts and text deltas pending; reasoning exclusions and cap/deadline behavior are unchanged.

Tests added in the existing `#[cfg(test)] mod bounded_stream_tests` (lines 1302-1410):
- `responses_and_gemini_idle_tick_is_complete_comment`: exact repeated comment bytes and no termination.
- `anthropic_empty_text_preamble_remains_retryable`: four absent/empty variants; one-shot terminal gate proves preamble remains pending; message_stop remains unavailable and rate-limit remains resource_exhausted.
- `anthropic_nonempty_text_releases_preamble`: nonempty opener/delta release on the first poll even with a permanently pending source.
- `translate_stall_budget_accounts_only_upstream_wait`: pure accounting reset, unchanged zero-wait/backpressure, exact exhaustion, saturation, and final short-poll bound.

Existing test updated: `thinking_followed_by_text_is_released_whole` (lines 1280-1300) now supplies nonempty initial text (`answer`), rather than asserting that an empty opener is served.

All three assigned IDs implemented. Relay-owned plumbing, finish_success, Retry-After, and relay integration/accounting tests deliberately untouched as instructed.

Verification: scoped `git diff --check -- crates/gateway/src/compat/mod.rs` passed. No cargo, build, tests, diagnostics that compile, or provider requests were run; root session owns final verification. An initial unscoped diff check reported pre-existing trailing whitespace in `.omo/evidence/model-registry/task-11-routing.http` and `task-12-model-apis.http`; neither file was changed by this lane.

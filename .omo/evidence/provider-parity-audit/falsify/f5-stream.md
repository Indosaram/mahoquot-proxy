# F5 - streaming finding falsification

Read-only product review; only this evidence report is written. No cargo, runtime tests, or network calls. O = mahoquot-proxy; A = opencodex; B = CLIProxyAPI. Verdicts below are static control-flow proofs, not executed reproductions.

Checked O `docs/reference/provider-parity-deviations.json:1-21` and `docs/reference-parity.md:1-3`: neither documents these five behaviors. The latter is only an authoritative-copy pointer. Source comments relevant to intentional behavior are considered below.

| id | verdict | concrete result |
|---|---|---|
| L5-01 | CONFIRMED | Generic same-wire HTTP 200 SSE with zero body bytes reaches successful EOF accounting. |
| L5-02 | CONFIRMED | Translated stream that becomes permanently pending after its first frame never reaches an inactivity failure. |
| L5-03 | CONFIRMED | Translated Responses output receives no heartbeat during a quiet upstream interval. |
| L5-04 | CONFIRMED | Empty Anthropic text-block opener followed by message_stop produces empty successful completion. |
| L5-05 | CONFIRMED | A 429 with Retry-After HTTP-date ten minutes ahead, without reset headers, receives only five minutes of cooldown. |

## L5-01

**CONFIRMED -** A streaming chat request through a Generic same-wire account receiving `200 Content-Type: text/event-stream` and a zero-byte clean response returns empty HTTP 200 and increments served instead of failing before commit.

Disproof attempted: the outer counting wrapper might reject empty/truncated SSE, or all successful responses might already have been opened. Neither holds. `finish_success` returns directly at O `crates/gateway/src/relay.rs:4390-4402`; Native returns directly at `:4405-4418`. These returns precede `open_stream` / `open_anthropic_stream` at `:4496-4515`. `stream_response` at `:2620-2633` simply wraps `resp.bytes_stream()`. CountedStream maps clean EOF to `StreamEnd::Eof` at `:719-722`; finalization starts with success for HTTP <400 (`:384-389`) and increments served (`:403-411`). The error-tail guard at `:395-401`, implemented at `:564-578`, looks for actual error objects or failed/incomplete types: empty input or a nonterminal text delta contains neither. Thus it does not rescue this case. The comment at `:4387-4389` deliberately preserves same-wire bytes; it does not document accepting missing protocol completion as success.

References reread: A `src/server/chat-native-sse.ts:294-305` rejects clean EOF without done/finish and rejects unterminated frames. B `internal/runtime/executor/claude_executor_stream.go:424-491` explicitly checks message_stop even in the same-format forwarding branch. A `src/adapters/run-turn-queue.ts:43-61` detects adapter EOF before a non-heartbeat event, but that alone is not evidence for native-chat precommit failover. The narrower terminal-detection comparison is sound and superior: a real client sees a stated failure rather than a falsely successful request.

**MINIMAL PATCH:** In `finish_success`, retain current raw copying for non-SSE bodies, but for the two early raw-return branches with SSE content type, open/peek before returning and replay the bytes unchanged. Empty EOF must return `Err` into the existing precommit failover path (`relay.rs:6404-6417`). Wrap the continuing raw SSE stream with a bounded, framing-only terminal observer selected by the downstream wire (chat finish_reason/[DONE], Anthropic message_stop, Responses completed/incomplete/failed, Gemini finishReason). On EOF without terminal, yield `Err(std::io::Error::other("upstream SSE ended before a terminal event"))` rather than clean EOF; CountedStream then records transport failure using `:712-717`. This is sufficient to stop silent success without changing native payloads or inventing a cross-protocol failure-tail renderer. Preserve legitimate failed terminal frames for the existing error-tail detection. Do not put the terminal check in shared `stream_response` unconditionally (images/binary use it too).

**Tests to add:** `raw_sse_empty_fails_over_before_commit`: upstream one empty SSE response, upstream two valid completion; assert account two selected and its completion returned. `raw_sse_truncated_records_failure`: one nonempty delta then EOF; assert body consumption errors, failed accounting once, served unchanged, and no second-account dispatch. `raw_binary_copy_unchanged`: assert exact binary bytes and no SSE validation.

## L5-02

**CONFIRMED -** An Anthropic-to-chat stream that emits nonempty text, then stays permanently pending while the client keeps reading, emits 15-second ticks forever instead of releasing the upstream and completing as failure.

Disproof attempted: an HTTP body-read timeout, request deadline, or outer finalizer might eventually bound the stream. O `crates/gateway/src/proxy_policy.rs:260-283` configures pool idle timeout and TCP keepalive, not a request/body-read timeout. The dispatch at `relay.rs:1897-1911` sends without a request timeout; the normal translated response enters `compat::streaming_body` at `relay.rs:4604-4632`. In `compat/mod.rs:655-676`, every timeout calls tick and continues; no budget or counter accumulates. CountedStream finalizes only on error, EOF, or drop (`relay.rs:659-722`), none of which occurs while the consumer remains connected and polling. A broad timeout search found only Devin preflight and unrelated management/quota/registry timeouts, not an inference body guard. A stalled TCP connection that remains open is not resolved by pool idle timeout or TCP keepalive.

Reference reread: A `src/stall-timeout.ts:1-19` defaults to 300 seconds; A `src/bridge.ts:462-464,1470-1500` maintains independent stall ticks, emits response.incomplete/upstream_stall_timeout, and cancels upstream. It excludes gated/backpressured periods. This is superior for a permanently stalled request: finite resource retention and an explicit outcome. A configurable generous budget avoids mistaking legitimate long reasoning for a hang.

**MINIMAL PATCH:** Add a remaining upstream-silence budget to `TranslateState` (default 300 seconds); in `streaming_body`, time only the awaited upstream poll, not time between downstream pulls or ticks. Subtract time spent waiting; reset on `Some(Ok(_))`. Bound each poll by `min(STREAM_IDLE_TICK, remaining)`. On exhausted budget:

```rust
state.drained = true;
state.pending.extend(error_frames(
    &mut state.renderer,
    "upstream stall timeout",
));
```

Drop/replace `state.upstream` with `Box::pin(futures::stream::empty())` at that point so pending failure frames do not retain the live connection. Existing failure-frame accounting remains applicable. Proxy ticks must not reset this budget.

**Test to add:** `translated_upstream_stall_expires_without_charging_backpressure`: use a paused Tokio clock, a source with an exact poll-entry signal and drop signal, and a held downstream pull; advance only after the source is pending. Assert tick before budget, explicit terminal error at budget, source dropped, failed accounting once, no failover after commit; advance time while downstream is not polling and assert it does not consume the upstream-wait budget. No fixed sleeps or polling delays.

## L5-03

**CONFIRMED -** A streaming `/v1/responses` request translated from Anthropic that pauses for 30 seconds after nonempty text emits no heartbeat bytes during that interval, allowing a 20-second downstream idle timeout to disconnect an otherwise live request.

Disproof attempted: Responses/Gemini renderers could receive another periodic event, or native copying could be mistaken for translated behavior. O `compat/mod.rs:610-624` explicitly selects the Responses/Gemini renderers; `StreamRenderer::tick` at `:578-583` returns empty bytes for both. The quiet path calls only this method (`:659-675`). `relay.rs:4604-4632` exposes this body as SSE; CountedStream merely counts/forwards it. Pending frames eventually drain and no other scheduled heartbeat is injected. This finding does not apply to raw passthrough. Gemini has the same dispatch omission, but A's cited source directly substantiates Responses, not Gemini-specific client behavior.

Reference reread: A `src/bridge.ts:458-460` constructs complete SSE comment or named-heartbeat frames; `:1502-1518` enqueues them when wire-silent independently of upstream activity. Comments are semantically inert and preserve long-running real clients behind idle-timed intermediaries, so O has no compensating advantage. O's deliberate chat tick shape is unrelated to the empty Responses/Gemini arms.

**MINIMAL PATCH:** In `StreamRenderer::tick`, leave the two existing protocol-specific tick arms alone and replace the fallback with:

```rust
Self::Gemini(_) | Self::Responses(_) => Bytes::from_static(b": keepalive\n\n"),
```

The loop emits renderer frames only after pending frames drain, so this remains at complete downstream frame boundaries.

**Test to add:** `responses_and_gemini_idle_tick_is_complete_comment`: for each shape use a paused Tokio clock and a source poll-entry gate, await a pending upstream read, advance 15 seconds, and assert exactly `: keepalive\n\n`; resume a fragmented valid event and assert valid parsed output, unchanged usage, and no terminal event from the tick.

## L5-04

**CONFIRMED -** An Anthropic stream containing `message_start`, `content_block_start` with `{ "type":"text", "text":"" }`, `content_block_stop`, then `message_stop` (no deltas/tool calls) bypasses the preamble guard and renders an empty successful completion instead of remaining retryable.

Disproof attempted: classification of later frames in the same read, a decoder guard, or intentional empty-completion policy could rescue this. O `compat/mod.rs:199-214` excludes only thinking/redacted-thinking block starts; the text opener takes `_ => Served` at `:243`. The classifier returns immediately, even if message_stop is later in the same buffer. `hold_anthropic_preamble` releases Served at `:311-313`. Empty text_delta similarly passes the fallback (only thinking/signature deltas are excluded at `:215-224`). O `compat/claude.rs:205-230` only warns about no visible output and then emits Completed. The caller does use this preflight (`relay.rs:4503-4512`), so this is not dead code. O's comments at `compat/mod.rs:280-288` explicitly promise holding until text/tool output and retrying empty completions, not a documented opt-out for empty block starts.

Reference reread: A `src/server/responses/empty-completion-guard.ts:119-135` requires nonempty text or actual tool events. Important qualification missing from the lane: A's retry is **opt-in**, not the default (`:43-47,54-63`); by default A deliberately observes/logs empty success to avoid duplicate billable side effects. Therefore A is not unconditionally superior on automatic retries. However its semantic content test is superior when this guard is enabled, and O already deliberately enables precommit empty-turn failover. This finding survives as an implementation defect in O's chosen policy, not as proof that A's default always recovers.

**MINIMAL PATCH:** In `classify_anthropic_preamble`, extend the existing content_block_start pending condition to include text blocks whose text is absent/empty, and content_block_delta pending condition to include text_delta whose text is absent/empty:

```rust
// Additional start predicate:
|| (value["content_block"]["type"] == "text"
    && value["content_block"]["text"].as_str().unwrap_or("").is_empty())
// Additional delta predicate:
|| (value["delta"]["type"] == "text_delta"
    && value["delta"]["text"].as_str().unwrap_or("").is_empty())
```

Keep nonempty initial text, tool starts, cap, and deadline behavior unchanged. Multiline preflight framing is a separate issue; fixing it is not necessary to fix this concrete input.

**Tests to add:** `anthropic_empty_text_preamble_remains_retryable`: release the empty opener with an exact source gate, assert no preflight completion before releasing message_stop, then assert Err and next-account dispatch at the relay seam. Repeat with empty text_delta and rate_limit_error; assert resource_exhausted. `anthropic_nonempty_text_releases_preamble`: assert immediate release for nonempty initial text and nonempty text delta, without awaiting EOF.

## L5-05

**CONFIRMED -** At `now_ms = 0`, a non-Cline account receiving 429 with `Retry-After: Thu, 01 Jan 1970 00:10:00 GMT` and no reset headers is benched until 300000 ms rather than the instructed 600000 ms, making it eligible five minutes early.

Disproof attempted: parsing elsewhere, quota-body reset handling, or clamping might make the integer-only parse harmless. O `relay.rs:2173-2196` parses only i64 seconds, falls through to reset headers, then defaults to 300. `record_cooldown` takes that deadline at `:2413`; the ordinary non-Cline branch uses it unchanged at `:2460-2461` and benches at `:2477-2483`. Cline has a special body-reset override (`:2441-2457`), so this example explicitly excludes it. Existing nonnegative/max clamping (`:2162-2164`) does not fix discarded dates. A future production date relative to current now yields the same +300s-versus-+600s discrepancy.

References reread: A `src/combos/failover.ts:116-137` parses HTTP-date after numeric seconds and bounds the delay. A `src/oauth/generic-account-failover.ts:205-215` consumes that parser with Retry-After precedence before exhausted-quota/default fallback. A correctly obeys a server-specified valid delay and avoids premature retries; O has no advantage here.

**MINIMAL PATCH:** In `cooldown_deadline_from_headers`, add an HTTP-date fallback inside the Retry-After parse, before reset-header fallback. A small dependency-free-for-this-workspace starting point for the concrete IMF-fixdate case is `chrono::DateTime::parse_from_rfc2822(v.trim())` followed by `timestamp_millis().saturating_sub(now_ms).max(0).saturating_add(999) / 1000`, then existing `cooldown_deadline_ms`. For complete HTTP-date support (including obsolete RFC850/asctime forms), use `httpdate::parse_http_date` with an explicit gateway dependency instead; do not claim RFC2822 alone implements all HTTP-date syntax. Keep integer, reset, default, saturation, and clamp behavior intact. Fractional seconds are not required to repair HTTP-date support.

**Test to add:** `cooldown_retry_after_http_date_precedes_reset`: fixed now; assert future +600s gives +600000ms even with a conflicting reset header, past date gives immediate deadline under O's existing nonnegative policy, integer stays unchanged, malformed falls back to reset/default, and excessive future dates respect MAX_COOLDOWN_SECS. Include IMF-fixdate/RFC850/asctime if the full HTTP-date parser is selected.

## New high-severity defects

None added. Initial-byte/preflight hangs are related to L5-02 rather than an independent new finding. No runtime or compiler pass is claimed; all proposed tests and patches remain unexecuted.

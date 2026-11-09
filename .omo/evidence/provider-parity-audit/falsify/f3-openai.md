# Falsification review: OpenAI / Codex

Source-derived review; no cargo, tests, builds, or network calls. Only this report was written. O/A/B roots are those in the task. Each verdict below means a counterexample survived caller/normalization checks, not live reproduction of an upstream rejection.

Checked documented exceptions: O `docs/reference/provider-parity-deviations.json:1-24` covers catalog differences only; `docs/reference-parity.md:1-3` is an ownership pointer. Neither documents these eight behaviors. The deliberate token-budget omission in O `compat/request.rs:150-161` does not exempt unrelated native-envelope fields.

## L3-01 — CONFIRMED

Input: native Codex `/v1/responses` with `{"model":M,"input":[{"role":"user","content":"hi"}],"metadata":{"client":"x"},"stream":true,"store":false}` -> outbound ChatGPT body still contains unsupported `metadata`, producing a rejection rather than inference.

Disproof attempted: native routing might have a destination sanitizer. It does not: O `cp_routes.rs:818-831` selects Native; `relay.rs:3228-3236` copies bytes; `relay.rs:1632-1638` calls `body_with_model`, whose complete rewrite at `relay.rs:992-1005` changes model and **messages**, not input/envelope fields. The fast-mode step (`relay.rs:3331-3359`) only adds service_tier. Native success bypasses the normalized event parser (`relay.rs:4405-4418`), but that cannot repair a request. A `src/adapters/openai-responses.ts:1327-1341,2204-2212` explicitly removes metadata for forward endpoints; its comment records the strict allowlist rejection. B `internal/translator/codex/openai/responses/codex_openai-responses_request.go:24-36,67-76` independently normalizes string input, required flags, truncation, and system role; B does **not** establish the metadata claim itself. A is superior for this public-client input.

MINIMAL PATCH: in Codex target construction in `resolve_target` (the native Codex return at relay.rs:1632), normalize only the Responses endpoint and only Codex destinations; preserve `original_body`. Remove metadata, truncation and the locally documented unsupported max_output_tokens; pin store=false/stream=true; normalize string input and textual system messages without rebuilding function/reasoning items. Do not apply this to compact or generic public Responses providers. Test `native_codex_responses_normalizes_public_envelope`: capture target bytes and assert metadata/truncation absent, store=false, text and function/reasoning items preserved.

## L3-02 — CONFIRMED

Input stream: output_item.added(function_call, index=0, call_id=c, name=f), function_call_arguments.done(index=0, arguments=`{"x":1}`), response.completed -> Chat tool `c` succeeds with `arguments:""` instead of `{"x":1}`.

Disproof attempted: completed-item reconciliation or a different decoder might fill arguments. O `compat/events.rs:273-305,306-333` handles added/delta/completed, neither done event; `compat/mod.rs:419-422,462-464` actually selects this parser for Codex. O `compat/render.rs:371-400,505-525` initializes empty arguments and only appends ToolArgsDelta; Completed supplies usage only. B `internal/translator/codex/openai/chat-completions/codex_openai_response.go:338-362,402-429` has both done fallbacks and suppresses duplication. Native Responses passthrough is unaffected, but translated Chat is not. B is superior because clients receive executable arguments.

MINIMAL PATCH: make `SseParser::decode_codex` track per-output-index nonempty argument delivery; decode arguments.done and function output_item.done into ToolArgsDelta only if no arguments have already been emitted; mark delivery after fallback. Keep index-scoped state for parallel calls. Test `codex_done_only_arguments_survive_chat_rendering`: parse the three-event fixture, render both stream and aggregate, assert exact `{"x":1}`; include delta+done and two-interleaved-call cases to assert no duplication/cross-talk.

## L3-03 — CONFIRMED

Input stream: text delta `partial`, then response.incomplete with error=null, incomplete_details.reason=content_filter and usage={input_tokens:3,output_tokens:1} -> Chat emits upstream_error `content_filter` and no finish_reason/usage, rather than a content_filter terminal completion.

Disproof attempted: the max-output limit special case does exist, but O `compat/events.rs:236-246` matches only max_output_tokens; `:312-323` makes content_filter and max_tokens Failed. O `compat/render.rs:402-440,532-534` cannot recover reason/usage from Failed. B `internal/translator/codex/openai/chat-completions/codex_openai_response.go:205-221,275-288` preserves usage and maps content_filter and both limit spellings. This is not a request rejection or fatal response.failed event, so B's terminal semantics are superior.

MINIMAL PATCH: extend `SseParser::decode`'s successful-incomplete handling to max_tokens and content_filter while retaining its no-error guard. Add a normalized content-filter marker and renderer/aggregator finish-reason flag, analogous to OutputLimitReached; emit Completed with usage. Test `codex_incomplete_preserves_chat_reason_and_usage`: assert partial text, content_filter finish reason, usage 3/1 and exactly one DONE; repeat max_tokens expecting length in stream and aggregate.

## L3-04 — CONFIRMED

Input Chat history: assistant tool call id=`c` repeated 65 times, function=f, arguments=`{}`, followed by tool result with the identical tool_call_id -> Codex receives both 65-character call_id values and rejects its 64-character limit.

Disproof attempted: matching IDs are indeed preserved, but no shortening/validation is performed in O `compat/request.rs:273-280,309-319`; downstream Codex `body_with_model` (`relay.rs:992-1005,1632-1638`) does not inspect input items. B `internal/translator/codex/openai/chat-completions/codex_openai_request.go:326-355,171-179` explicitly repairs the limit collision-safely and pairs the result via pendingToolCalls. This is a destination restriction, not a mismatch between the caller's paired IDs. B is superior for valid Chat histories that use longer opaque identifiers.

MINIMAL PATCH: in `split_messages`, reserve all already-short call IDs, then build one request-local long-ID mapping before emitting calls/results. Use SHA-256-derived ASCII IDs <=64 bytes and collision retry; reuse the mapping at both cited emission sites. Do not independently truncate each occurrence. Test `codex_long_call_ids_keep_injective_pairing`: use two long IDs plus an existing short ID colliding with the first candidate; assert <=64, three distinct wire IDs and exact call/result pairing.

## L3-05 — CONFIRMED

Input `/v1/responses` routed to a Claude account with a tool declaration, `parallel_tool_calls:false` and `reasoning:{effort:"high"}` -> normalized Chat omits both controls, so Claude receives neither disable_parallel_tool_use=true nor the requested effort.

Disproof attempted: some generic adapters actually use original bytes, so the lane's blanket claim about all ordinary dispatch is too broad. Meta/Kimi/xAI and generic openai-responses bypass the lossy representation (`relay.rs:1358-1361,1381-1389,1416-1423,1491-1506`). That does not save Claude: O `compat/responses.rs:681-706` copies neither field; `relay.rs:1070-1100,1562-1584` uses the normalized body, with no restoration of original options. The marker-only override at `relay.rs:1008-1037` is not triggered by a top-level reasoning field. O's own Claude adapter `compat/claude.rs:899-909` would honor parallel=false if retained. A `src/adapters/openai-responses.ts:621-634,2180-2189` maps effort without dropping other reasoning fields and preserves the raw parallel flag. A is superior for this concrete translated path; do not claim all nonnative providers lose it.

MINIMAL PATCH: in `responses_to_openai`, before returning chat:
```rust
for key in ["reasoning", "parallel_tool_calls"] {
    if let Some(value) = obj.get(key) {
        chat[key] = value.clone();
    }
}
```
Test `responses_claude_controls_survive_translation`: responses_to_openai then openai_to_anthropic with a reasoning-capable Claude model; assert tool_choice.disable_parallel_tool_use=true and the configured high-effort mapping, plus normalized reasoning.effort==high.

## L3-06 — CONFIRMED

Input paired Responses function_call/function_call_output with output=[{type:input_text,text:"look"},{type:input_image,image_url:"data:image/png;base64,aGVsbG8=",detail:"high"}] routed to Claude -> tool output becomes one JSON text string, not an image-bearing result.

Disproof attempted: O has explicit multimodal Chat tool support (`compat/request.rs:349-387`), but `compat/responses.rs:477-486` calls val.to_string **before** that representation can reach any adapter. The preceding call-id checks (`:449-475`) accept the paired fixture; this is not an unmatched-output rejection. Native and raw-body generic paths are exempt, not the Claude conversion path (`relay.rs:1562-1584`). B `internal/translator/codex/openai/chat-completions/codex_openai_request.go:176-179,550-584` preserves text/image arrays, including detail. B is superior in retaining pixels rather than JSON prose.

MINIMAL PATCH: in `responses_to_openai` function_call_output handling, make output a Value: strings remain strings; arrays map input_text/output_text -> Chat text, input_image URL/detail -> Chat image_url; preserve ordering. Keep object/non-array fallback stringification. Reject malformed supported image parts using existing ResponsesError conventions rather than silently dropping them. Test `responses_tool_output_array_retains_image_order`: paired history through responses_to_openai and downstream adapter; assert [text,image,text] ordering, actual image source and high detail, not string content.

## L3-07 — CONFIRMED

Input non-Codex compact request with history ending `What is 2+2?`, tools and forced tool_choice -> ordinary provider inference is requested; if the provider returns a tool-only reply, O returns a successful compaction capsule whose unsealed summary is empty.

Disproof attempted: neither the relay mode nor capsule sealing creates a summarizer instruction. O `cp_routes.rs:902-918` changes only stream/store; `:935-957` extracts message text and seals even an empty string. O `compat/responses.rs:56-83` has no empty-summary guard. Responses translation (`compat/responses.rs:681-706`) forwards tools/tool_choice. A `src/adapters/openai-responses.ts:2045-2068,2284-2293` explicitly removes tools/forced choices/schema controls and appends COMPACT_PROMPT as the final user message. A is superior because this endpoint must summarize rather than continue the task.

MINIMAL PATCH: in `compact_via_summary`, remove tools/tool_choice/parallel_tool_calls/text; remove private compaction_trigger/additional_tools input items; normalize a string input into a message array if necessary; append an explicit final user instruction to summarize conversation state for continuation without answering/executing the pending task. Before sealing:
```rust
if summary.trim().is_empty() {
    return json_status(StatusCode::BAD_GATEWAY,
        json!({"error":{"message":"empty compaction summary","type":"server_error"}}));
}
```
Test `compact_requests_summary_and_rejects_tool_only_reply`: capture final summarizer instruction and absence of tools/schema controls; stub tool-only response and assert 502/no capsule, then text response and assert capsule unseals to that summary for the selected model. No claim that prose can be mechanically proven to be a good summary.

## L3-08 — CONFIRMED

Input Chat `{"model":M,"messages":[{"role":"user","content":"hi"}],"reasoning":{"effort":"high","summary":"auto"}}` -> translated Codex body contains reasoning.effort=high but no summary, silently disabling the caller's explicit summary request.

Disproof attempted: O `compat/request.rs:81-96` accepts the reasoning object; `:137-140` unconditionally reconstructs effort only. No later Codex request normalizer restores summary (`relay.rs:1632-1638`). A `src/adapters/openai-responses.ts:2180-2184,2284-2302` operates on the request body rather than replacing reasoning; its `:133-144` gate only strips stream_options.reasoning_summary_delivery, not reasoning.summary. B `internal/translator/codex/openai/chat-completions/codex_openai_request.go:69-72` documents opt-in intent, but **does not itself copy the nested summary there**: that citation alone is not proof of a B implementation advantage. A preserves this explicit intent; O's effort-default comments do not document dropping it.

MINIMAL PATCH: in `openai_to_codex_with_cache_key`, immediately after creating reasoning:
```rust
if let Some(summary) = obj.get("reasoning").and_then(|r| r.get("summary")) {
    out.get_mut("reasoning").unwrap()["summary"] = summary.clone();
}
```
Retain any established destination capability restrictions rather than enabling summaries by default. Test `codex_explicit_reasoning_summary_is_retained`: assert summary=auto with explicit effort and with a suffix; assert suffix still wins effort only and omission leaves summary absent.

## New high-severity defect (one)

**N3-01 — Generic openai-chat receives Responses request schema on /v1/chat/completions.** Input a valid nonnative `/v1/responses` request `{"model":M,"input":"hi"}` whose selected Generic adapter is openai-chat -> O `relay.rs:1510-1518` sends `plan.original_body` (input, no messages) directly to `/v1/chat/completions`, so a strict Chat provider rejects missing messages. `build_plan` did produce the correct normalized `openai_body` (`relay.rs:3295-3307`), but this branch ignores it. Moreover generic response passthrough at `relay.rs:4386-4402` would expose a Chat reply unchanged to the Responses client even after only fixing the request. This also bounds L3-05/L3-06: their normalization losses are not the immediate failure on this generic branch. Minimal correction needs both request selection from normalized Chat and protocol-appropriate Chat-to-Responses reply conversion; changing the request alone is insufficient. Test `responses_generic_chat_roundtrip`: capture messages=[{role:user,content:hi}], no input; stub a Chat completion and assert client object=response with message/output_text, for buffered and streaming modes.

## Bottom line

8 CONFIRMED; no documented-deviation/better/disproven verdicts. Two important lane qualifications: L3-05/L3-06 are provider-path-specific, and L3-08's B citation documents intent but does not implement the claimed nested-summary preservation. All examples and suggested tests are source-derived and unexecuted, as required.

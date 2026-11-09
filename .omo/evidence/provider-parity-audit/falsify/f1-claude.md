# F1 Claude falsification review

O = `/Volumes/T9-Mac/project/mahoquot/mahoquot-proxy`; A = `/Volumes/T9-Mac/project/opencodex`; B = `/tmp/opencode/7364b1ba363fb673fd/cliproxyapi`. Paths below are relative to these roots.

Method: independently re-read the lane, register, cited O/A/B implementations, O's deviations manifest and reference-parity pointer, and the live relay planning/rendering callers. Source-derived counterexamples only: no cargo, network, or upstream execution. Only this report was written. Eight findings survive; the explicit-cache finding is a documented deviation, not an undocumented defect.

## L1-01

**CONFIRMED** - Anthropic usage `{input_tokens:100,cache_read_input_tokens:40,cache_creation_input_tokens:60,output_tokens:10}` produces OpenAI `prompt_tokens:100,total_tokens:110`, rather than inclusive `200,210`.

Evidence: O `crates/gateway/src/compat/claude.rs:219-229,1192-1214` copies exclusive input into canonical prompt usage; O `compat/render.rs:18-32` renders it unchanged. A `src/adapters/anthropic.ts:569-584` explicitly sums all three input components; B `internal/translator/claude/openai/chat-completions/claude_openai_response.go:87-94` does likewise. Disproof attempted: cache detail fields are present, but neither the renderer nor the JSON conversion adds them back; late message_delta counts only overwrite raw counters. Relay JSON dispatch calls this converter at `relay.rs:4443-4450`.

**MINIMAL PATCH:** in `AnthropicDecoder::decode`'s message_stop branch compute `let prompt = self.input_tokens + self.cache_read_tokens + self.cache_write_tokens;` and use `prompt` / `prompt + self.output_tokens`. In `anthropic_json_to_openai`, compute prompt after reading cache fields: `let prompt_tokens = prompt_tokens + cache_read.unwrap_or(0) + cache_write.unwrap_or(0);`. Do not change raw decoder counters or cache detail fields.

Test `anthropic_cache_usage_is_inclusive_json_and_stream`: feed the fixture through JSON conversion and through message_start/message_delta/message_stop plus the chat renderer; assert prompt=200,total=210,cache read=40,write=60.

## L1-02

**CONFIRMED** - Canonical usage prompt=200,cache read=40,cache write=60,output=10 renders native Anthropic input=200 with both cache fields missing, instead of input=100,read=40,creation=60.

Evidence: O `compat/claude.rs:1326-1329,1623-1629`; buffered relay calls `messages_payload_full` with decoder usage directly at `relay.rs:4133-4186`. No caller subtracts caches. A `src/claude/outbound.ts:89-103` subtracts both cache components; B `internal/translator/openai/claude/openai_claude_response.go:919-932,1036-1072` emits cache fields and saturates subtraction. This matters even without a Claude upstream: OpenAI prompt_tokens are inclusive of cached reads.

**MINIMAL PATCH:** in `messages_payload_full` and `AnthropicStreamRenderer::render(Completed)`, build mutable usage JSON using `u.prompt_tokens.saturating_sub(u.cached_tokens).saturating_sub(u.cache_write_tokens)` and output count; insert `cache_read_input_tokens` iff `cached_tokens_known` and `cache_creation_input_tokens` iff `cache_write_tokens_known`. Apply the same correction to `render_anthropic_stream`'s message_start snapshot, which has the same bug.

Test `anthropic_render_usage_excludes_known_caches`: assert JSON usage and live SSE terminal usage have 100/10/40/60; assert measured zero survives and absent details remain absent. Cover the buffered stream snapshot too.

## L1-03

**CONFIRMED** - OpenAI chat `{stop:["END"]}` routed to Claude omits stop_sequences, and native Anthropic `{stop_sequences:["END"]}` routed to an OpenAI-chat backend omits stop.

Evidence: O `compat/claude.rs:463-478,916-924`; relay stores the original OpenAI-shaped request at `relay.rs:3321-3331` and clones it without stop normalization at `relay.rs:1008-1050,1075-1085`, then calls the converter at `relay.rs:1581-1584`. Native inbound conversion occurs at `relay.rs:3251`. A `src/chat/inbound.ts:312`, `src/claude/inbound.ts:517-519`, `src/adapters/anthropic.ts:916` preserve stops; B `internal/translator/claude/openai/chat-completions/claude_openai_request.go:130-143` and `internal/translator/openai/claude/openai_claude_request.go:59-72` map both ways. Choose a chat backend supporting stop, not Codex (which has its own restricted options contract).

**MINIMAL PATCH:** in `anthropic_to_openai`, copy `stop_sequences` to `out["stop"]`. In `openai_to_anthropic_with_capability`, map `body["stop"]` string to a one-element array and array to a clone under `result["stop_sequences"]`; preserve the existing stop_sequences fallback, and ignore null stop.

Test `claude_stop_controls_cross_map`: assert string and array OpenAI stops become arrays upstream, and native array stops become OpenAI stop, including relay target-body assertions.

## L1-04

**CONFIRMED** - OpenAI chat with `max_completion_tokens:64` and no max_tokens reaches Claude with max_tokens=32768 (with thinking omitted/disabled so no thinking-budget adjustment obscures the example).

Evidence: O `compat/claude.rs:852-861`; relay keeps the original cap spelling, as in L1-03. A `src/chat/inbound.ts:304-309` selects the modern cap and `src/adapters/anthropic.ts:898-903` renders it; B `internal/translator/claude/openai/chat-completions/claude_openai_request.go:118-123` accepts either spelling. O's generous-default comment applies to omission, not to an explicitly supplied modern cap. No normalization in the target-body caller repairs this.

**MINIMAL PATCH:** in `openai_to_anthropic_with_capability`, replace the max expression with `body.get("max_tokens").or_else(|| body.get("max_completion_tokens")).and_then(Value::as_u64).unwrap_or(32_768)`. This preserves existing max_tokens precedence when both appear, matching B.

Test `claude_honors_max_completion_tokens`: disabled-thinking fixture with cap=64; assert actual Claude target body max_tokens=64; include both-spellings precedence.

## L1-05

**CONFIRMED** - Native user content `[image(source:{type:"url",url:"https://example.org/a.png"}),text("describe")]` routed to an OpenAI-chat backend becomes text-only `"describe"`.

Evidence: O `compat/claude.rs:314-328,444-457` accepts only base64 and never retains the URL source. Native plan conversion at `relay.rs:3251` discards it before any target adapter can recover it. A `src/claude/inbound.ts:134-143` and B `internal/translator/openai/claude/openai_claude_request.go:575-606` convert URL images. O's outbound image validation at `compat/claude.rs:950-975` is the opposite direction, not a guard for this case.

**MINIMAL PATCH:** in `anthropic_to_openai`'s image branch add `else if source["type"] == "url" { image_parts.push(json!({"type":"image_url","image_url":{"url":source["url"]}})); }`. In the tool_result image mapper add the equivalent URL conversion instead of returning the original Anthropic image object. No new media abstraction needed.

Test `anthropic_url_images_survive_chat_translation`: assert URL and text survive user-content conversion and routed chat target construction; separately assert tool-result URL images use image_url rather than raw Anthropic source.

## L1-06

**CONFIRMED** - Valid native history assistant tool_use X, then user `[tool_result X,text("continue")]`, becomes OpenAI roles `[assistant,user,tool]`, violating required immediate tool-result adjacency.

Evidence: O `compat/claude.rs:444-459` pushes the ordinary user turn before extending tool_results. Relay `openai_body_with_model` at `relay.rs:1008-1050` removes internal markers only; no tool-order repair. A `src/claude/inbound.ts:319-356` preserves result-before-text order; B `internal/translator/openai/claude/openai_claude_request.go:299-302` explicitly emits tool results first. A Claude-native passthrough does not fail, but routing this valid request to a chat backend does.

**MINIMAL PATCH:** in `anthropic_to_openai`, move `out_messages.extend(tool_results);` to immediately before the assistant/text emission conditional and delete the now-unused `has_tool_results` binding. This puts all results ahead of accompanying user text, matching B, without a broader order-preserving rewrite.

Test `anthropic_tool_results_precede_user_followup`: assert roles `[assistant,tool,user]`, matching tool_call_id X, unchanged tool output and follow-up text, including routed target body.

## L1-07

**CONFIRMED** - Native request to a Codex target with `thinking:{type:"disabled"}` and no model suffix becomes `reasoning:{effort:"medium"}`; adaptive thinking with output_config.effort="high" also becomes medium.

Evidence: O `compat/claude.rs:463-526` never copies these top-level fields; O `compat/request.rs:509-556` cannot recover them. Native relay builds the Codex translation from the reduced object at `relay.rs:3251-3255`; O `compat/request.rs:72-99,136-140` defaults omission to medium. This is a concrete downstream result, not merely a missing intermediate field. A `src/claude/inbound.ts:556-575` preserves disabled/effort/budget; B `internal/translator/openai/claude/openai_claude_request.go:77-118` does the same. Suffix precedence does not help requests without a suffix.

**MINIMAL PATCH:** in `anthropic_to_openai`, set `reasoning_effort` to `"none"` for disabled; otherwise take output_config.effort when present, else a manual budget converted via `super::request::normalize_thinking_level(&budget.to_string())`, mapping its error into String, else `"auto"` for adaptive/enabled without a budget. This effort survives both the existing Codex resolver and other target thinking resolution. Leave target clamping and model-suffix precedence to existing code.

Test `anthropic_thinking_controls_survive_codex_plan`: assert disabled->none, adaptive/high->high, enabled/8192->medium in the final Codex reasoning.effort, rather than only checking the intermediate field.

## L1-08

**CONFIRMED** - A Responses assistant message carrying `reasoning_content:"r"`, genuine Claude `reasoning_signature:S`, followed by a function_call item, loses its entire thinking message on Claude dispatch; only tool_use survives.

Evidence: O `compat/responses.rs:527-539,650-655` accepts and retains those assistant reasoning fields. O `compat/claude.rs:783-824` reads only content/tool_calls and drops the empty assistant. Relay Responses planning stores this normalized history at `relay.rs:3302-3317`; the Claude path consumes it at `relay.rs:1581-1584`, not the original Responses body. A `src/adapters/anthropic.ts:697-715` restores signed thinking and redacted blocks; B `internal/translator/claude/openai/responses/claude_openai-responses_request.go:1129-1163` restores compatible signatures. Disproof attempted: dropping foreign/unsigned reasoning is defensible, but O drops genuine Claude signatures too. Standard standalone Responses `reasoning` + encrypted_content is also not explicitly handled by O's item switch (`compat/responses.rs:356-515`); do not claim the cited lines already decode that standard shape.

**MINIMAL PATCH:** in `openai_to_anthropic_with_capability`, before assistant text/tool blocks, prepend a thinking block from reasoning_content/reasoning_signature only when the signature passes Claude compatibility validation; prepend redacted_thinking with its opaque data for a known redacted marker. Preserve opaque bytes and never fabricate signatures. This repairs the concrete normalized-history case without changing general Responses parsing. A separate standard encrypted_content import is additional scope, not required for this counterexample.

Test `claude_replays_signed_assistant_reasoning_before_tools`: use a captured real Claude signature fixture, run Responses->chat->Claude and assert thinking/signature precedes the next tool_use and thinking-only assistant is retained. Add foreign/unsigned negative cases so the fix cannot regress into blindly forwarding arbitrary provider reasoning.

## L1-09

**RECLASSIFY-DEVIATION** - O explicitly documents that an already-marked request is left as sent at `crates/gateway/src/compat/claude.rs:1061-1067`; its early return implements that policy, and native relay invokes it before serialization at `relay.rs:1587-1592,1623`.

The five-marker counterexample is mechanically real: five explicit ephemeral text-block markers remain five. It does not survive the requested documented-deviation check. The lane itself quotes the implementing lines but overlooks the immediately preceding policy comment. A `src/adapters/anthropic.ts:1014-1023,189-218` and B `internal/runtime/executor/claude_executor_execute.go:162-176` enforce a limit. Those references provide better acceptance for an over-limit request, but five markers are not a valid Anthropic request and this does not make rewriting client cache policy strictly superior overall. A also changes explicit TTLs (`anthropic.ts:220-243`), contrary to the lane's suggestion that TTL normalization can preserve every explicit TTL. The manifest contains no Claude entry; the code comment is the deviation authority permitted by this review's rules.

No patch requested for this documented policy. If policy is later changed, enforce the count at the dispatch boundary for both native and translated requests, not solely inside the OAuth-only auto-placement branch.

## New high-severity defects

None promoted. This review establishes the concrete losses above, not speculative additional failures.

## Verification limits

All cited implementations and relevant relay callers were read from disk. No executable behavior, upstream rejection, tests, Rust compilation, or diagnostics are claimed. Proposed tests and patches are recommendations only; product source is unchanged.

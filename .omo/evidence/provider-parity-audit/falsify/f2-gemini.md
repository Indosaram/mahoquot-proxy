# Gemini falsification review

Source-only review; no cargo, network, or product-source edits. O = `/Volumes/T9-Mac/project/mahoquot/mahoquot-proxy`, A = `/Volumes/T9-Mac/project/opencodex`, B = `/tmp/opencode/7364b1ba363fb673fd/cliproxyapi`. All paths below are relative to those roots. Tests listed are proposals, not executed verification.

I re-read the lane, register, each cited implementation and reference, and the relevant callers. No entry in O `docs/reference/provider-parity-deviations.json:1-26` covers these differences; O `docs/reference-parity.md:1-3` only redirects ownership. All ten core findings survive, with qualifications below.

## L2-01

**CONFIRMED** - A Vertex service-account request with project `p`, location `europe-west4`, model `gemini-2.5-flash`, and no override produces `https://aiplatform.googleapis.com/v1/projects/p/locations/europe-west4/...` instead of the selected regional endpoint.

Disproof attempted: explicit overrides do work (O `url.rs:51`), but no default-host normalization exists. O `relay.rs:1331-1343` incorporates location only into the path; O `url.rs:43` hardcodes the global host. A `src/adapters/google.ts:954-959` selects the regional host for service-account requests; its separate global API-key fast path at 942-947 is not this credential path. Regional endpoint selection is superior for location-scoped routing. This is verified wrong endpoint selection, not a claim that every global-host/regional-path combination necessarily returns an HTTP error.

**MINIMAL PATCH:** In O `relay.rs::resolve_target`, Vertex branch, derive a fallback base before `build_provider_url`, retaining the override:

```rust
let vertex_base = if location == "global" {
    "https://aiplatform.googleapis.com".to_string()
} else {
    format!("https://{location}-aiplatform.googleapis.com")
};
// url field:
url: crate::url::build_provider_url(member.kind(), upstream.or(Some(vertex_base.as_str())), &path),
```

Test `vertex_target_uses_location_host_unless_overridden`: assert exact host/path for global, europe-west4, omitted location (us-central1), and explicit override.

## L2-02

**CONFIRMED** - `{"candidates":[{"finishReason":"SAFETY"}]}` converts to OpenAI `finish_reason:"stop"`; decoder completion re-rendered as Gemini fabricates `finishReason:"STOP"`, and substituting `MALFORMED_FUNCTION_CALL` also completes successfully.

Disproof attempted: O `GeminiDecoder::finish` rejects a missing finish reason, but `decode` sets `saw_finish_reason` for every string (O `compat/gemini.rs:1680-1695`). The error-envelope guard at 1544-1555 does not recognize candidate finish reasons. O buffered mapping at 1370-1379 and renderer at `compat/render.rs:210-225,598-607` lose classification. `relay.rs:4083-4152` rejects Failed events only, so it cannot rescue these candidates. A `src/adapters/google.ts:1457-1461` maps safety to content_filter; `google-truncation.ts:15-23` fails malformed empty turns. B `gemini_openai_response.go:637-680` likewise distinguishes content filtering (including malformed calls). Exposing filtering/failure rather than clean success is superior.

**MINIMAL PATCH:** In `gemini_json_to_openai_inner`, map safety/recitation/blocklist/prohibited/SPII (and image safety) to `content_filter`, and classify MALFORMED_FUNCTION_CALL as `content_filter` rather than stop (this converter returns Value, not Result). In `GeminiDecoder::decode`, emit Failed and set `completed=true` on MALFORMED_FUNCTION_CALL; retain the raw terminal reason in a narrowly added event consumed by `ChunkRenderer`, `GeminiChunkRenderer`, and `Aggregator`, so filtering reaches Chat and native reasons survive. Do not reinterpret every MAX_TOKENS response as failure: A only fails that case when tool calls have started. The existing event enum cannot preserve native reasons without a small cross-file event change.

Test `gemini_blocked_and_malformed_finish_is_not_success`: feed SAFETY/RECITATION and malformed-empty fixtures through buffered conversion and decoder/collector; assert content_filter/native reason for filtering, content_filter for buffered malformed and error for decoded malformed, never stop/STOP.

## L2-03

**CONFIRMED** - Usage `{promptTokenCount:16,candidatesTokenCount:5,thoughtsTokenCount:42,totalTokenCount:63,cachedContentTokenCount:7}` produces OpenAI completion_tokens=5 instead of 47, and the buffered converter omits cached_tokens=7 entirely.

Disproof attempted: O deliberately keeps native candidates separate in `compat/gemini.rs:1571-1597`, but OpenAI rendering in `compat/render.rs:18-28` never adds thoughts; buffered output at `gemini.rs:1382-1398,1440-1447` also does not. A `src/adapters/google.ts:471-477` has a separate native internal representation, not evidence that OpenAI completion excludes reasoning. B `gemini_openai_response.go:482-496,655-669` adds candidates+thoughts for OpenAI. Its zero-cache omission is worse than O's streaming presence tracking; keep O's superior zero fidelity. This does not disprove the positive-cache omission or accounting defect.

**MINIMAL PATCH:** Only at Gemini-to-OpenAI output boundaries, add candidates+thoughts in `gemini_json_to_openai_inner` and apply the same normalization before rendering Gemini-derived Chat usage. Do NOT unconditionally add thoughts in shared `usage_value`: other decoders may already supply inclusive completion counts. Carry a Gemini-origin usage flag or use the protocol-aware rendering boundary, retaining separate native candidate counts. In the buffered converter keep fallback total equal to prompt+inclusive completion, and conditionally insert prompt_tokens_details.cached_tokens using numeric presence (including zero).

Test `gemini_openai_usage_includes_thoughts_and_preserves_cache_presence`: assert Chat completion=47, reasoning=42, total=63, cache=7 (also 0/absent variants); native candidates remains 5 in streaming and buffered collection.

## L2-04

**CONFIRMED** - Chat request `{model:"gemini-2.5-flash",messages:[{role:"user",content:"hi"}],stop:"END"}` reaches all three translated Google-family routes without generationConfig.stopSequences.

Disproof attempted: O `relay.rs:1008-1050,3310-3320` retains the original OpenAI body, so Codex's separate option-dropping policy does not excuse this. O `compat/gemini.rs:1078-1100` never reads stop; Vertex's later `apply_vertex_thinking` (relay.rs:3135-3195) adds only format/thinking. Direct and Antigravity callers at 1455 and 1656-1658 do not add stop later. A `src/adapters/google.ts:806-811` and `google-wire-compiler.ts:127-131` preserve stop sequences; honoring explicit output termination is superior.

**MINIMAL PATCH:** Insert into `openai_to_gemini_inner`'s generation builder:

```rust
if let Some(stop) = body.get("stop") {
    let sequences = match stop {
        Value::String(s) => Some(vec![json!(s)]),
        Value::Array(items) => Some(items.clone()),
        _ => None,
    };
    if let Some(sequences) = sequences {
        generation.insert("stopSequences".into(), json!(sequences));
    }
}
```

Test `google_family_stop_sequences_reach_wire`: assert string END becomes [END], and [END,DONE] remains that array in Vertex, generic Google, and wrapped Antigravity target bodies.

## L2-05

**CONFIRMED** - Tool parameters `{"type":"object","$defs":{"Thing":{"type":"string"}},"properties":{"x":{"$ref":"#/$defs/Thing"}}}` leave `x.$ref` on the wire but delete its target.

Disproof attempted: the Claude-only preprocessor does not run on Gemini models: O `compat/gemini.rs:1157-1170,1276-1282`. `sanitize_gemini_schema` removes definitions before walking children (1188-1214); no ref resolver is called on the non-Claude path. A `google-tool-schema.ts:168-185` resolves local targets before constructing output. B request code at `gemini_openai_request.go:364-391` uses parametersJsonSchema, but also calls its cleaner; B `internal/util/gemini_schema.go:142-146` explicitly expands local refs before stripping definitions. Both references preserve concrete type information for this input; O does not. A's comment that Google supports some reference forms does not rescue a reference whose target O deleted.

**MINIMAL PATCH:** In `openai_to_gemini_inner`'s non-Claude parameter preparation, resolve local JSON pointers against an untouched schema before invoking `sanitize_gemini_schema`; enforce an active-ref set and bounded expansion, returning an explicit error on cycles/unresolved refs. Resolve pointer escaping through `Value::pointer`, not string splitting. No tool-name or schema-algebra refactor is needed. Merely deleting $ref avoids a rejection but silently loses semantics and is not the fix.

Test `gemini_tool_local_refs_resolve_before_defs_strip`: assert x.type=string and no dangling ref; add escaped-pointer case and explicit cycle error.

## L2-06

**CONFIRMED** - A valid OpenAI function named `9tool` is emitted as Gemini functionDeclarations[].name=`9tool` (and unchanged in historical calls/results and named choice), violating Gemini's leading letter/underscore requirement.

Disproof attempted: the lane's `mcp.server/tool` example is not itself a valid strict OpenAI function identifier; use `9tool` instead. O does have a name codec, but only inside `interactions_to_gemini` (`compat/gemini.rs:7-109`); the Chat route calls `openai_to_gemini_inner`, which copies names at 909,1124-1129,1167-1170. A `google-wire-compiler.ts:5,13-46,72-104,147-158` maps names and restores them through `google.ts:1437-1441`. B `antigravity_gemini_request.go:100-127` also maps declarations. O callers at `relay.rs:1345,1455,1656-1658` do not invoke the Interactions codec. Reversible normalization is superior to sending an invalid identifier; no need to restrict names more than the upstream actually requires.

**MINIMAL PATCH:** Add a request-scoped original/wire map at the Chat conversion boundary; map declarations, call/response history, and allowedFunctionNames consistently. Carry its inverse in the existing per-request protocol session and restore names in `gemini_json_to_openai_inner` / `GeminiDecoder` before client emission. Preserve already-legal names and collision-check generated names; no global mutable codec. A converter-only rename without response restoration is not a complete patch.

Test `gemini_chat_tool_name_codec_round_trips`: use 9tool and colliding cleaned names in declarations, historical call/result and named choice; assert legal distinct wire names and original response names.

## L2-07

**CONFIRMED** - Generic Google/Antigravity Chat input with model=gemini-2.5-flash and thinking_budget=2048 emits thinkingLevel=HIGH rather than thinkingBudget=2048; reasoning_effort=auto likewise emits HIGH rather than budget=-1.

Disproof attempted: O `compat/request.rs:98-112` validates the budget but does not rewrite the original body used by these branches (`relay.rs:1008-1050,3310-3320`). O `compat/gemini.rs:516-548` chooses the model-based level before the explicit-budget branch. Vertex repairs it later at `relay.rs:3164-3195`, but generic Google/Antigravity have no such call. B `gemini_openai_request.go:49-61` explicitly maps auto to -1; that range alone does NOT prove B handles O's thinking_budget extension. The verified O defect is loss of explicit intent in its own supported budget field. Correct budget selection is superior to unconditional HIGH, especially on a budget-based 2.5 model.

**MINIMAL PATCH:** In `build_thinking_config`, after explicit none handling and before default level selection, honor explicit_budget when no explicit effort overrides it; handle auto as thinkingBudget=-1. Use `compat::request::thinking_mode` for the existing effort/suffix precedence rather than inventing contradictory precedence. Keep Vertex capability validation unchanged; do not add shared thought tokens here.

Test `gemini_explicit_budget_and_auto_precede_model_default`: inspect generic and wrapped Antigravity bodies, assert budget=2048 / -1 and absence of thinkingLevel, plus unchanged Vertex result.

## L2-08

**CONFIRMED** - Generic Google or Antigravity Chat input with response_format={type:"json_object"} reaches upstream without responseMimeType=application/json; a json_schema format also loses its schema.

Disproof attempted: ingress accepts and separately translates this option for Codex (`compat/request.rs:163-202`), but `build_plan` keeps the original Chat body and the Google converter never reads it. O `relay.rs:3145-3163` is called only from the Vertex branch (1345-1346), not generic Google (1455) or Antigravity (1656-1658). B `gemini_openai_request.go:89-90,602-618` invokes the format mapper and preserves schema unchanged. A's compiler lacks this mapping too; the finding is valid against B, not a claim that A is superior here. Honoring the format contract is superior to silently returning unconstrained text.

**MINIMAL PATCH:** Move the existing format match from `apply_vertex_thinking` into `openai_to_gemini_inner`'s generation builder: json_object adds responseMimeType; json_schema also copies json_schema.schema to responseJsonSchema. Remove only the now-redundant Vertex match. Do not run the tool-schema sanitizer over output schemas.

Test `google_family_structured_output_reaches_wire`: all three target builders must emit application/json, and json_schema must preserve the exact schema.

## L2-09

**CONFIRMED** - Successful token exchange, loadCodeAssist={}, and onboardUser={done:false} persist project_id="" and mark login Completed; the next Chat request passes the Some("") check and sends project="".

Disproof attempted: O `management/oauth.rs:327-375` performs only one onboarding call and has no final nonempty guard before persistence at 438-442. O `crates/providers/src/antigravity.rs:247-260` deserializes empty projects without validation; O `account.rs:1145-1149` returns Some(""); `relay.rs:1643-1645` rejects only None. A `src/oauth/google-antigravity.ts:108-136,208-215` polls and rejects unresolved discovery before returning credentials; B `internal/auth/antigravity/auth.go:312-405` polls and returns an error on exhaustion. O's explicit loadCodeAssist 403 guard does not protect the successful asynchronous case. Waiting for a usable project or failing login is superior to reporting unusable authentication as complete.

**MINIMAL PATCH:** In `exchange_antigravity_code`, repeat onboarding within a fixed attempt/time bound until done plus a nonempty project (honor cancellation; handle hard failures explicitly). Immediately before credential construction reject an empty project. In `AccountMember::project_id`'s Antigravity arm return None for empty project as a use-boundary guard. Keep tests event-driven; a mocked sequence needs no actual polling sleep.

Test `antigravity_login_waits_for_project_before_persist`: scripted done:false then done:true/response/project yields exactly one persisted completed credential with that project; exhaustion/error persists nothing; empty imported project fails target resolution.

## L2-10

**CONFIRMED** - User content `[text("transcribe this"),input_audio({format:"wav",data:<nonempty base64 WAV>})]` becomes only `{text:"transcribe this"}` on the Google-family wire, silently changing the prompt.

Disproof attempted: O `compat/gemini.rs:1488-1509` recognizes only text/image_url. O `relay.rs:3107-3129` and document gating at 5976-5980 recognize file/document/input_file, not audio. The preliminary Codex translation does not replace the original Chat body (`relay.rs:3310-3320`), so this is the actual conversion path, not an unused helper. B's audio handling is at `gemini_openai_request.go:191-199` in this checkout (slightly later than the lane's cited 174-190), with MIME mapping at 577-599. Retaining audio bytes, or explicitly rejecting unsupported audio, is superior to silently transcribing a different prompt.

**MINIMAL PATCH:** Add input_audio handling to `openai_content_to_gemini_parts`: wav -> audio/wav, mp3 -> audio/mpeg, inlineData.data from input_audio.data. Change this converter to Result and propagate errors at its caller for missing bytes/unsupported formats instead of silently filtering them out; no file/video expansion is needed for this finding.

Test `google_family_input_audio_is_never_silently_dropped`: a real nonempty base64 WAV fixture plus text must preserve identical bytes/MIME in all three bodies; audio-only must remain a user turn; unsupported format must return an explicit error.

## New high-severity findings

None asserted. The scope contains other differences, but no additional high-severity counterexample was established to the same standard.

## Verification limits

All verdicts establish deterministic field/branch behavior from source, not a live provider acceptance claim. Product source was not changed, and no tests were run as instructed. In particular L2-01 is an endpoint-selection defect, and L2-06 uses the valid OpenAI identifier `9tool` rather than relying on the lane's invalid OpenAI slash example. Proposed patches for finish-reason propagation and reversible tool names necessarily touch existing event/session plumbing; changing only the converter would be incomplete.

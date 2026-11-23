# L2 Gemini repair

Owned source files only were edited; relay.rs and build_provider_url are unchanged.

| ID | Changed ranges (current lines) | Result |
|---|---|---|
| L2-01 helper | `crates/gateway/src/url.rs:5-12,122-139` | Added public vertex_base_url; empty/global use global-aiplatform host, regional locations select regional host. Root must wire helper in relay. |
| L2-02 | `crates/gateway/src/compat/gemini.rs:513-526,1614-1626,1940-1947` | Buffered blocked/malformed/OTHER -> content_filter; streaming -> Failed, no successful completion. MAX_TOKENS retains length/output-limit classification, even buffered tool calls. |
| L2-03 | `gemini.rs:1628-1647,1678-1701` | Buffered prompt includes numerically known cache, completion includes thoughts, reasoning details retained, total recomputed; cached zero remains distinguishable from absence. Streaming/native usage representation intentionally unchanged. |
| L2-04 | `gemini.rs:1254-1265` | Shared stop string/array or native stop_sequences -> stopSequences. |
| L2-05 | `gemini.rs:1389-1446` | Resolve local JSON pointers before definition stripping, preserve sibling keywords; cycle/depth/node-expansion guards; unresolved refs dropped as requested. |
| L2-06 | `gemini.rs:528-643,1021,1362-1364,1519-1546,1567,1774,1789-1793,1877` | Request-scoped collision-safe codec maps declarations/history/choice; public response-restoration entry points added. **Root wiring required:** derive `GeminiToolNames::from_body(original_chat_body)` and carry into `GeminiDecoder::new()/with_replay(...).with_tool_names(names)` and buffered `gemini_json_to_openai_with_tool_names(body, model, created, replay: Option<&ReplayScope>, &names)`. Existing response entry points remain backward-compatible and cannot restore without this request context. |
| L2-07 | `gemini.rs:658-671` | Explicit budget and auto (-1) beat model-default level; existing explicit non-auto effort handling remains. |
| L2-08 | `gemini.rs:1266-1274` | Shared json_object/json_schema -> responseMimeType/responseSchema, including generic Google and Antigravity. |
| L2-10 | `gemini.rs:1022-1034` | Reject input_audio explicitly for all message roles before content extraction. |

## Regression tests added

`url.rs:122-139`: vertex_location_selects_regional_host (also empty/global).

`gemini.rs:1975-2173`:
- gemini_finish_reason_safety_is_not_success
- gemini_buffered_usage_includes_thoughts_and_cache
- gemini_stop_sequences_are_forwarded
- gemini_local_ref_schema_is_resolved
- gemini_tool_name_codec_roundtrips
- gemini_explicit_budget_beats_model_default
- gemini_response_format_on_generic_and_antigravity
- gemini_input_audio_is_rejected

Existing test expectations updated: none; no existing owned test asserted the corrected buffered behavior. Tests use pure fixtures and no sleeps/provider requests.

Verification: rustfmt parsed/formatted both owned Rust files successfully; git diff --check passed. No cargo/build/test/typecheck commands ran, per lane rules. Root owns final verification and relay integration. No assigned ID was deliberately skipped; L2-06 end-to-end integration is incomplete until root wires the provided request-context APIs. Streaming OpenAI thought/cache usage correction outside this owned buffered scope remains a root/render concern if desired.

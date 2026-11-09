# L6 falsification review

Static source review only; no cargo, network, live-provider calls, or product-source edits. O/A/B are the roots in the lane report. Line numbers below refer to the files re-read in this review. Tests and patches are proposals, not executed results.

## L6-01 — CONFIRMED

Concrete input: `{"model":"gpt-4","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,<valid PNG>"}}]}]}` produces `Cursor requires a user message`, rather than an image-bearing Cursor request.

Disproof attempted: upstream normalization or an alternate image carrier. `relay.rs:1520-1533` calls this builder directly; `openai_body_with_model` (`relay.rs:1008-1047`) only removes internal markers, changes model/effort and normalizes developer roles. `cursor.rs:74-80,112-119,226-237` extracts text and rejects empty text; `cursor_proto.rs:97-106` has no selected-context field. A actually resolves active images (`src/adapters/cursor/live-transport.ts:621-635`), permits image-only user actions and attaches selected context (`protobuf-request.ts:1376-1406`; `images.ts:536-544`). A is superior for a vision client. No applicable documented exception.

MINIMAL PATCH: extend `cursor_proto::UserMessage` with the actual selected-context/image protobuf definitions from A's generated schema; in `openai_to_cursor_connect`, extract active-turn data images, attach bounded blob-backed selected images, and change the empty-text guard to `if user_text.is_empty() && selected_images.is_empty()`. Implement image blob hydration together with L6-02, not an accept-without-image shortcut. Test `cursor_image_only_encodes_selected_image`: decode the real Connect payload, assert a selected image resolves to the exact fixture PNG bytes; mixed text/image must preserve both.

## L6-02 — CONFIRMED

Concrete input: same-stream `SetBlobArgs { blob_id: [1], blob_data: [2,3] }` followed by `GetBlobArgs { blob_id: [1] }` receives a successful set acknowledgement and an absent get result.

Disproof attempted: external storage or intentional stateless rejection. `cursor.rs:449-460` discards both argument objects outright; `CursorDecoder` owns no blob map (`:239-248`). Production wiring supplies the reply channel (`relay.rs:1854-1856`; `compat/mod.rs:432-435`), so this is not merely a sender-less test seam. A's `native-exec.ts:648-695` reads the key, stores bytes, and returns a capacity error when admission fails. A is superior: success acknowledges retrievable state.

MINIMAL PATCH: add a decoder/request-owned `HashMap<Vec<u8>, Vec<u8>>` and retained-byte count; in `reply_to_kv`, get with `self.blobs.get(&args.blob_id).cloned()`. On set, account for replacement bytes, reject above a fixed byte/key cap using `SetBlobResult.error`, otherwise insert before acknowledging. Test `cursor_kv_set_then_get_preserves_bytes`: decode both replies and assert `error == None` then `blob_data == Some(vec![2,3])`; test rejected admission never returns success or replaces an existing value.

## L6-03 — RECLASSIFY-DEVIATION

O explicitly documents this exact default as deliberate in `compat/exec_bridge.rs:64-73`: “Standalone gateways execute exec requests on their own host,” followed by a once-only warning that unset `MAHOQUOT_EXEC_BRIDGE` runs exec on the gateway host. `cursor.rs:508-511` implements that policy, and `exec_ops.rs:152-159` performs the operations. Under the review's explicit code-comment exception, this cannot remain an *undocumented parity defect*.

This classification does **not** disprove the security consequence or claim O is better. With no exec root set, `cursor.rs:1178-1181` chooses `/`; a server WriteArgs to a writable absolute file reaches `std::fs::write` (`:1274-1290`) without owner opt-in. A's default really is off (`exec-policy.ts:9-20,40-44`), is passed by production transport (`live-transport.ts:507-510`), and rejects file/shell execution (`native-exec.ts:584-599`). A is safer for a shared gateway. The lane accurately described runtime behavior but missed the deliberate O code comment required by the verdict rules.

## L6-04 — CONFIRMED

Concrete input: user `look` → assistant tool call `a` → tool result `a` (text `done`, `is_error:true`, inline PNG) → user `next` produces historical `userInputMessage.content="done"` without `toolResults`, result status, call ID, or image.

Disproof attempted: normalization before current-turn selection. `kiro.rs:45-73` only pops a trailing contiguous tool suffix; `:98-135` sends every remaining non-assistant through the generic user branch. The relay's normalization (`relay.rs:1008-1047,1548-1551`) does not merge or pair these turns. A processes all results (`kiro.ts:645-673`), merges adjacent user carriers (`:585-593`), and serializes historical images/context (`:714-734`). A may merge the result into the newest user carrier rather than retain a separate historical turn; either way it preserves the result linkage that O loses. No applicable exception.

MINIMAL PATCH: in `openai_to_kiro_with_profile`, build user/tool carriers before popping the current user; reuse the existing result object construction for every tool message, retain `images`, and merge adjacent user carriers so the sequence remains alternating. This is confined to that builder, not relay changes. Test `kiro_prior_tool_result_survives_new_user`: assert result `a` is present exactly once across history/current context with `status="error"`, exact PNG bytes and newest text `next`; assert the assistant `toolUses` ID matches.

## L6-05 — CONFIRMED

Concrete input: an AWS `reasoningContentEvent` with payload `{"redactedContent":"opaque-1"}` followed by normal completion yields no opaque reasoning state, so replay cannot return `opaque-1`.

Disproof attempted: signature fallback or a renderer-side recovery. `kiro.rs:428-435` only recognizes `text` and `signature`; framing passes the JSON payload to that branch (`:347-354`). No event containing the redacted field is emitted, so a renderer cannot recover it. The builder accepts replay state (`:119-124`), but that is input-only. A parses the exact field (`kiro-events.ts:122-132`), emits its provider-owned event (`kiro.ts:1369-1374`), and rebuilds `reasoningContent.redactedContent` (`:714-719`). A is superior for signed continuation. No exception.

MINIMAL PATCH: add a distinct `CodexEvent::KiroRedactedReasoning(String)` (do not disguise it as plaintext or an unrelated provider signature); in `KiroDecoder::decode_json` emit it whenever `redactedContent` is a string, independently of a coexisting `text` field. Preserve it in the response accumulator and Responses opaque reasoning carrier, and map that carrier back to `kiroRedactedReasoning` in request normalization. Test `kiro_redacted_reasoning_roundtrips`: AWS frame → actual renderer → request parser → Kiro builder must preserve exact `opaque-1` in replay and never put it in text/reasoning-text output.

## L6-06 — CONFIRMED

Concrete input: `{"tokenUsage":{"uncachedInputTokens":10,"cacheReadInputTokens":3,"cacheWriteInputTokens":2,"outputTokens":4,"totalTokens":19},"stopReason":"end_turn"}` produces `Completed { usage: None }` instead of measured counts.

Disproof attempted: usage enrichment outside the decoder or metadata after completion. `kiro.rs:432-435,445-453` emits None on stop and clean EOF; no decoder usage field exists (`:266-274`). The common accumulator consumes Completed usage directly (`compat/mod.rs:937`). A explicitly parses input as 10+3+2, output 4, total 19, read 3, write 2 (`kiro-events.ts:70-91,157-173`) and retains authoritative usage (`kiro.ts:1341-1342`). A is superior for billing/accounting. No exception.

MINIMAL PATCH: add `usage: Option<Usage>` to `KiroDecoder`; parse `tokenUsage` before the existing mutually exclusive content/stop chain and set prompt=uncached+read+write, completion=output, total=total, cached=read, cache_write=write with known flags based on field presence. In both terminal sites use `usage: self.usage.take()`. Test `kiro_metadata_usage_survives_stop_and_eof`: assert `(prompt,completion,total,cached,cache_write)==(15,4,19,3,2)` for same-frame stop and metadata-then-clean-EOF; measured zeros remain known.

## L6-07 — CONFIRMED

Concrete input: default Command Code OAuth callback with a valid CLI credential followed by a chat request produces `/provider/v1/chat/completions` with a flat OpenAI body, not `/alpha/generate` with `params.messages` and CLI headers.

Disproof attempted: OAuth intentionally importing a Provider API credential, generic URL normalization, or adapter override. `management/oauth.rs:31-32,2061-2098` validates through `/alpha/whoami`, but `:2118-2127` persists `adapter="openai-chat"` and the Provider API base; generic fallback in `relay.rs:1509-1517` constructs chat completions. A explicitly separates OAuth and API-key entries (`providers/registry.ts:1299-1313,2126-2130,2161`); its OAuth adapter uses the proprietary wrapper, required CLI headers, and NDJSON (`adapters/command-code.ts:502-532,563-589`). O's own frozen snapshot agrees (`docs/reference/opencodex-registry-snapshot.json:105-116`). The declared `command-code` deviation only changes its **label**, not protocol. A is superior for the OAuth credential's actual endpoint; static evidence does not establish that every such credential is rejected by the separate API plan.

MINIMAL PATCH: in `exchange_command_code_callback` persist `adapter="command-code"` and canonical root base for default OAuth; keep `commandcode` key accounts untouched. Add a dedicated branch in `resolve_target` (`relay.rs:1129`) plus a `compat::command_code` wrapper/NDJSON decoder: `params` contains model/messages/tools/system/stream, outer config/memory/taste/skills/permissionMode/mode follow A, headers include User-Agent cli and x-command-code-version/x-cli-environment/x-session-id. Select a dedicated protocol and decode text/reasoning/tool/finish records. Test `command_code_oauth_uses_cli_wire`: callback fixture → real relay target must use `/alpha/generate`, nested messages, CLI headers, and emit a completed response from NDJSON; separate `commandcode_api_keeps_chat_wire` asserts the API-key path remains unchanged. Changing only the stored adapter is not a fix.

## L6-08 — CONFIRMED

Concrete input: a valid non-agent `{"model":"<model bound to the sole eligible Devin account>","input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]}]}` returns HTTP 503 `no eligible Interactions account` without contacting Devin.

Disproof attempted: B's Interactions being an unreachable intermediate-only parser, O model eligibility being the actual rejection, or the experimental-provider exception. O's predicate excludes Devin regardless of otherwise valid eligibility (`interactions.rs:285-291`), then returns 503 (`:311-319`). B selects Interactions as Devin's request format (`devin_executor.go:90-93`), accepts source Interactions without translation (`:415-429`), and parses `user_input` with images (`:1788-1801`); it also emits Interactions output (`:625-626`). Thus the cited parser is on a real executor path. O's experimental-provider documentation (authoritative parent `docs/reference-parity.md:44-50`) documents Devin's existence, not an intentional Interactions exclusion; O's module summary (`interactions.rs:1`) describes implemented providers but does not document a deliberate Devin exception. B is superior for a real Interactions client.

MINIMAL PATCH: `interactions::handle` needs a dedicated non-agent Devin branch, not just an eligibility edit: translate steps to the existing Devin OpenAI-shaped builder, use its Connect relay/decoder, and translate events to Interactions. Preserve caller/credential/model/session-scoped saved continuations; exclude Devin from agent requests. Do not send it through the current non-native Antigravity branch (`interactions.rs:412-455`). Test `devin_interactions_only_account_routes_connect`: sole Devin fixture + request above → captured Connect request containing `hi` and Interactions completion instead of 503; add exact image preservation and matching function-result continuation assertions.

## New high-severity defect missed by the lane

**Exec-root escape for new paths.** With `MAHOQUOT_EXEC_ROOT=/tmp/work` (an existing directory), existing `/tmp/outside`, and absent `/tmp/outside/new.txt`, `WriteArgs.path="/tmp/work/../outside/new.txt"` passes `exec_resolve` and writes outside the configured root. `cursor.rs:1192` uses `canonicalize().unwrap_or(joined)` for an absent target, and `:1195` then checks the unnormalized lexical path with `starts_with(root)`. `exec_write` creates parents and writes (`:1280-1290`), so no later containment guard fixes it. This is distinct from default-on execution: it breaks an explicitly configured file boundary even after opt-in. Proposed regression `cursor_new_file_cannot_escape_exec_root` must assert rejection and absence of the outside file; fix resolution using the canonical existing ancestor plus validated remainder before admitting a new target (also cover symlink ancestors), not lexical prefix alone.

## Result

Seven CONFIRMED; one RECLASSIFY-DEVIATION (L6-03, explicitly deliberate code comment, not a security endorsement). No DISPROVEN or RECLASSIFY-BETTER. All eight cited O and A/B paths were re-read; applicable deviation JSON and the redirected authoritative parity document were checked. No runtime verification claimed.

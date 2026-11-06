# mahoquot Claude subscription safety and cache plan

Status: plan only. No code in this file has been implemented.
Scope: the Claude subscription (OAuth) path in `mahoquot-gateway`. Codex, Antigravity, Cline, Kiro, Z-code are touched only where they share routing code.
Inputs:
- Code audit (C1): sections 2 and 3, every claim with `file:line` at HEAD `3b947b4`.
- Live evidence (C2): section 4.
- External research (C3): section 5.
- Room discussion (근카스와 AI 포모단, 10/23-10/30): section 6, claim ids `Cnn` refer to the room claim table kept with the evidence.

## 1. Goal and non-goals

Goal, in order of priority:
1. Stop mahoquot from doing the things that most plausibly get subscription accounts flagged: sending a flagged request to more accounts, regular machine-like probes on every account, many accounts behind one IP with one identical client fingerprint.
2. Stop breaking the Anthropic prompt cache by moving a live conversation between accounts (room: 48% of cost from 3.8% of turns, C28/C29).
3. Make account state that looks like enforcement (403 permission errors, `Request not allowed`, org disabled, refund refusals) visible and sticky instead of silently retried.

Non-goals:
- Guaranteeing that subscription OAuth use through a third-party gateway is allowed. It is not guaranteed; see section 8. This plan reduces risk and cost; it does not make the practice compliant.
- Building new cloaking tricks beyond what the gateway already sends. The plan prefers fewer, more consistent signals over more forged ones.

## 2. Current Claude path (C1, verified)

| Area | What the code does | Evidence |
|---|---|---|
| Wire headers | OAuth accounts (no `api_key`) all send the same static identity: `user-agent: @anthropic-ai/sdk/0.74.0`, `x-app: cli`, `x-stainless-lang: js`, `x-stainless-package-version: 0.74.0`, `x-stainless-os: macos`, `x-stainless-arch: arm64`, `x-stainless-runtime: node`, `anthropic-version: 2023-06-01`. | `crates/gateway/src/account.rs:891-920` |
| Beta flags | `anthropic-beta: claude-code-20250219,oauth-2025-04-20,prompt-caching-2024-07-31`, merged with any client-sent betas. | `crates/providers/src/claude.rs:24-25`, `crates/gateway/src/relay.rs:1139-1156` |
| System prompt | Prepends `You are a Claude agent, built on Anthropic's Claude Agent SDK.` to every OAuth request (not nekos, not relay key). | `crates/gateway/src/compat/claude.rs:612-643`, called at `relay.rs:999-1005` |
| Cloak switch | `disable-claude-cloak-mode` exists in settings (default at `:552`) but has no runtime consumer. The flag is dead. | `crates/gateway/src/management/settings.rs:425-426`, `:552` |
| `metadata.user_id` | Never written or read by the gateway. Affinity reads `metadata.thread_id/conversation_id/session_id` and a top-level `user` that starts with `session_`/`conv_`/`thread_`. | `relay.rs:3636-3657` |
| Cache breakpoints | Adds one `cache_control: ephemeral` to the last message block unless the request already has one. | `compat/claude.rs:657-697` |
| Affinity key | Header ids (`x-claude-code-session-id`, `session-id`, `thread-id`, `x-session-id`, `x-client-request-id`, conversation ids) -> body ids and `metadata` ids -> `DefaultHasher` of the first non-empty user text (`body-first-user-*`) -> hash of the first 2 KiB of bodies of 4 KiB or more (only reached when there is no user text). | `relay.rs:3841-3843`, `relay.rs:3577-3600`, `relay.rs:3620-3690`, `relay.rs:3696-3711` |
| Affinity store | In-memory map in the router, dropped on restart. Once it holds more than 1024 keys, entries idle for more than 10,000 selections are pruned. A binding is honored only while the bound account is healthy; otherwise the strategy picks a new account and the key is rebound to it for good. | `crates/router/src/lib.rs:61`, `lib.rs:93-110`, `lib.rs:113-131` |
| Affinity header list | Includes `x-client-request-id`, which many SDKs set per request, not per conversation. A client that sends it gets a new key every turn, so no pinning at all. | `relay.rs:3577-3600` |
| New-session placement | New Claude sessions go to the account whose 5-hour window resets soonest; nearly spent accounts yield. Deliberately spreads new sessions over the pool. | `crates/gateway/src/claude_reset_order.rs:28`, `:44`, `:99` |
| Failover | Up to `min(eligible, max_failover=3)` accounts per request, only before any downstream bytes. 500-504 fail over without touching health (`relay.rs:4319-4331`); 529 is not in that range and reaches the client as a client error. A 429 sets a cooldown and fails over (`relay.rs:4246-4258`). A 403 that is not a model rejection is recorded and the same request is retried on the next account (`relay.rs:4395-4407`). A second 401 marks the account `AuthFailed` and fails over (`relay.rs:4409-4420`). An early SSE `error` event is mapped to a string `permission_denied: <message>` for `permission_error` or `Request not allowed`, and also fails over (`compat/mod.rs:190-213`, commit `5b87609`). | as listed |
| Warmup | A loop every 15 s. For each idle Claude subscription account whose 5-hour window is not active, sends a 1-token request with the Agent SDK system line and user text `.`. Live config enables it for Claude (`warmup.providers.claude.enabled: true`, `idle_secs: 3600`, `min_interval_secs: 300`). | `crates/gateway/src/main.rs:321`, `warmup.rs:371-404`, `warmup.rs:407-458`, live `config.yaml` `warmup:` block |
| Egress | `proxy-url: http://127.0.0.1:3128` is set, but because a provider policy (`cline`) is enabled, `base_proxy_url` returns none and every provider without its own policy, Claude included, uses the **direct** client. All Claude accounts leave from the host's own IP. | `proxy_policy.rs:21-23`, `proxy_policy.rs:78-90`, `state.rs:873-884`; live `config.yaml` `proxy-providers.cline.enabled: true` |
| Live pool | 11 Claude, 5 Codex, 4 Antigravity, 67 Cline, plus Cursor, Zhipu, xAI credential files; strategy `fill_first`. | `~/.mahoquot/auth` listing, `mahoquot-gateway.service` ExecStart |

## 3. What is wrong with the current path

Each item names the evidence and the room claim it matches.

- **R1. A forbidden answer is spread across the pool.** A 403 that is not a model rejection is retried on up to two more subscription accounts (`relay.rs:4395-4407`). If the 403 is an abuse or policy signal, mahoquot hands the same request to more accounts. The room reports the owner added exactly this ("내 프록시에서 403뜨면 스위치하는걸로 구현함", C57), and that one 403 `permission_denied` was not a ban (C55/C56), so the response must be classified, not just switched.
- **R2. Conversations move between accounts.** Affinity is memory-only, lost on restart, and rebinds on any unhealthy moment (`router/src/lib.rs:95-131`). New sessions are spread by reset time (`claude_reset_order.rs:99`). Each move rewrites the whole prefix on a new account (room: 289 full rewrites of about 700k tokens at $2-3 each, 230 within 5 minutes, C28/C29; fix named by the room: pin per session at the gateway, C42).
- **R2b. A per-request header can defeat affinity.** `x-client-request-id` is accepted as an affinity key (`relay.rs:3577-3600`). If the calling harness sends a fresh value per request, every turn is a new session and is placed by `claude_reset_order` again. Whether omo or Claude Agent SDK clients send it must be checked against captured request headers (work item W2 test).
- **R3 (minor). Body keys are not restart-stable and can collide.** The normal header-less key is a `DefaultHasher` digest of the first user text (`relay.rs:3660-3685`); it is only stable for one build, so it cannot be persisted as is. The 2 KiB prefix fallback (`relay.rs:3702-3711`) is reached only for bodies without user text and can merge unrelated sessions that share a system prompt. Two different sessions that start with the same first user message (common for scripted subagents) also share one key; that is a load-spread issue, not a cache issue, and the plan accepts it.
- **R4. Probes look like a robot.** Every idle account gets a 1-token `.` request right after each 5-hour reset, around the clock (`warmup.rs:371-458`). Eleven accounts doing this from one IP is a pattern no human user produces.
- **R5. One fingerprint, one IP, many accounts.** All 11 accounts share the same `x-stainless` device tuple, which claims `macos`/`arm64` from a Linux host, and the same direct egress IP (`account.rs:891-920`, `proxy_policy.rs:78-90`). The room's 25-account case ended with every account banned (C59/C60); generic fraud signals named in the room include frequent IP changes (C66).
- **R6. The identity is internally inconsistent.** The headers say the JS SDK with `x-app: cli` and the Claude Code beta, the system line says Agent SDK, and there is no `metadata.user_id`. The room's diagnosis is that client identity is what gets judged ("이름이 문제다", C07; omo OAuth banned in about 3 hours vs Aside not, C06/C11).
- **R7. The cloak switch does nothing.** `disable-claude-cloak-mode` is dead (`settings.rs:425-426`), so an operator cannot turn cloaking off even when routing through a legitimate key.

## 4. Live evidence (C2)

Source: `~/.mahoquot/auth/request-history.sqlite`, table `usage_events`, opened with `sqlite3 -readonly` (`mode=ro`). Window: 2026-10-23 12:47 to 2026-10-30 12:46 KST, `provider='claude'`. Only counts were read; no account names or tokens were printed. `session_identifier` is filled on every Claude row (173,830 of 173,830).

| Measure | Value |
|---|---|
| Claude requests | 173,830 across 10 active accounts (11 credential files) |
| Status codes | 200: 173,259; 403: 201; 503: 188; 499: 87; 502: 64; 429: 25; 404: 6 |
| 401/403 by day (requests, distinct accounts) | 10-25: 79 (4); 10-26: 93 (3); 10-27: 13 (6); 10-30: 16 (4) |
| Sessions / follow-on turns | 7,043 sessions; 166,217 turns after the first |
| Mid-session account switches (successful turns whose previous turn in the same session used another account) | 3,009 (1.8% of follow-on turns) |
| Sessions that used more than one account | 1,179 of 7,043; among sessions with 10 or more requests, 964 of 2,227 (43%) |
| Switches less than 5 min after the previous turn (cache still warm, so the switch alone broke it) | 1,627 of 3,010 (54%) |
| Mean cache-write tokens per turn | same account: 7,782; after a switch: 92,328 (11.9x) |
| Cache writes as a share of cache traffic | same account: 4.8%; after a switch: 54.8% |
| Cache writes caused on switch turns | 277.9M of 1,705M total cache-write tokens (16%) from 1.8% of turns |

Reading: the room's pattern (C28, C29: a few percent of turns causing a large share of rewrites, most of them inside the 5-minute TTL) reproduces in mahoquot's own traffic. 403s arrive on several accounts on the same day, which is what R1 turns into multi-account spread. `estimated_cost_usd` is 0 for subscription rows, so the cost effect is given in tokens; at Opus list prices a cache write costs about 12.5 times a cache read, so the 254M excess write tokens on switch turns (3,010 x (92,328 - 7,782)) are the largest avoidable item.

Gaps: warmup probes are not written to `usage_events` (0 Haiku 1-token rows), so their volume cannot be counted from history; W6 adds a counter. The 403 bodies are not stored, so whether any of the 201 were enforcement-class cannot be told from this table; the gateway journal for the last 3 days has no `only authorized for use with Claude Code`, `account has been disabled` or `extra usage` lines (checked by the enforcement lane, section 5).

## 5. External research (C3)

All pages fetched 2026-10-30. "Confirmed" = Anthropic's own pages or staff statements quoted by outlets; "Reported" = first-hand user reports or code; "Rumor" = unsourced. The two quotes marked (re-fetched) were fetched again by the plan author to check the lane's report.

### 5.1 Anthropic policy and enforcement

Confirmed:
- Claude Code legal page (re-fetched): Anthropic does not permit third-party developers "to route requests through Free, Pro, or Max plan credentials on behalf of their users", and developers "may not collect, store, or intermediate Claude.ai credentials or session tokens". Agent SDK developers should use API keys. https://code.claude.com/docs/en/legal-and-compliance
- Consumer Terms (effective 2025-10-08): automated access is prohibited except via an API key or explicit permission; termination for breach can be without notice and without refund. https://www.anthropic.com/legal/consumer-terms
- 2026-01-09: Anthropic tightened safeguards against spoofing the Claude Code harness; staff said some resulting bans were mistakes and were being reversed. https://x.com/trq212/status/2009689809875591565, https://venturebeat.com/technology/anthropic-cracks-down-on-unauthorized-claude-usage-by-third-party-harnesses
- 2026-02-19/20: terms wording clarified; OpenCode removed Claude login after legal requests. https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access/
- 2026-04-04: third-party harnesses no longer draw on subscription limits and need paid extra usage. https://techcrunch.com/2026/04/04/anthropic-says-claude-code-subscribers-will-need-to-pay-extra-for-openclaw-support/
- 2026-06-15 pause and **2026-10-07: Max and Team plans include monthly API credits, used with an API key from the linked Console org.** https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan
- Staff on the Agent SDK docs change (late Feb 2026): "Nothing is changing about how you can use the Agent SDK and MAX subscriptions", while a business on the Agent SDK should use an API key; Anthropic told the outlet it would not cancel accounts over it. https://thenewstack.io/anthropic-agent-sdk-confusion/
- Staff: having several Max accounts is not itself a violation; sharing or reselling is. Quoted at https://metricnexus.ai/blog/anthropic-banning-multiple-claude-accounts
- Appeals: claude.ai appeal form. https://support.claude.com/en/articles/8241253-safeguards-warnings-and-appeals

Reported (not confirmed by Anthropic):
- Detection is request-level fingerprinting on `/v1/messages`: system prompt prefix and structure, Claude Code tool names, `x-app`/`claude-cli` user agent/`anthropic-beta`, `metadata.user_id` format, absence of `temperature`. https://news.ycombinator.com/item?id=46625918
- Since April 2026 a mismatch is mostly billed as extra usage rather than rejected; Claude Code now sends a billing header block as `system[0]` (re-fetched). https://github.com/router-for-me/CLIProxyAPI/issues/2599
- Ban reports with dates: oh-my-opencode 2025-12-21 (https://github.com/code-yeongyu/oh-my-opencode/issues/158), OpenCode 2026-01-05, often right after a plan upgrade (https://github.com/anomalyco/opencode/issues/6930), CLIProxyAPI 2026-03-18 "account has been disabled after an automatic review" (https://github.com/router-for-me/CLIProxyAPI/issues/2211). Multiple Max accounts on one IP or device locked in Feb 2026 (metricnexus, above).
- No public ban reports found for Cline (its subscription mode shells out to the real `claude` CLI, https://docs.cline.bot/provider-config/claude-code) or for Aside.

Rumor: "ban score" guides (IP change, virtual cards) have no primary source; the room reached the same verdict (C66).

### 5.2 Proxy projects

| Project | Claude handling | What mahoquot takes from it | Source |
|---|---|---|---|
| CLIProxyAPI (router-for-me) | Full Claude Code cloaking: billing header block in `system[0]`, tool-name fingerprint matching, TLS and wire alignment with a pinned Claude Code version (PR #4752, "align OAuth wire with Claude Code 2.1.220"), account and device identity from the OAuth credential, `/api/oauth/profile` calls; account selection, cooldown, per-account egress proxy, management UI | Per-account egress (W5) and a single fixed client identity (W4). Not taken: billing block and tool renaming (section 8). Its own issues show the cost: blocks after an automatic review (#2211), silent extra-usage billing when the cloak drifts (#2599), and breakage with each Claude Code release (#4983) | https://github.com/router-for-me/CLIProxyAPI/issues/2211, /issues/2599, /issues/4983; room C18, C19, C21 |
| opencodex | Passes the client's real headers through instead of inventing them; the Claude pool is off by default; no built-in TLS impersonation; binds affinity on session and thread ids and pins subagent fan-out to the parent thread | The pass-through stance supports wiring `disable-claude-cloak-mode` (W4). Its session-plus-thread affinity is already ported in mahoquot's header list (`relay.rs:3580-3584`) | room C20 (bot code review, 10/28); mahoquot comments citing opencodex at `relay.rs:3582` and `relay.rs:4377` |
| cliproxyapi-rs | Two different repos go by this name: `vayungodara/cliproxy-rs` (used for the room's review) and `ishaanko/cliproxyapi-rs` (0 stars, no commits since Oct 4) | Nothing beyond CLIProxyAPI; not a maintained alternative | room C22 |

No project claims to remove ban risk, and the room's own comparison says the same (C21, C23).

### 5.3 What the research changes in this plan

- mahoquot's own setup matches the case the legal page names: pooled subscription credentials, stored tokens, one gateway serving several clients. No code change makes that compliant (section 8).
- The current wire identity is internally inconsistent in a way the reported checks would see: JS SDK user agent instead of `claude-cli`, `x-stainless-os: macos`/`arm64` from a Linux host, an Agent SDK system line without the billing block, no `metadata.user_id` (`account.rs:891-920`, `relay.rs:1001-1005`). Under the post-April regime this most likely means extra-usage billing rather than an error; it cannot be confirmed without looking at the account's usage page.
- The 2026-10-07 API credits give a sanctioned path that did not exist when the room's proxies were built. It becomes work item W0.

## 6. Room findings mapped to changes

| Room finding (claim ids) | What mahoquot does today | Change | Priority |
|---|---|---|---|
| omo via subscription OAuth is banned in about 3 h; Aside on the same OAuth is not; "the name is the problem" (C03-C07, C11, C12) | Mixed identity: JS SDK headers claiming macOS on a Linux host, Claude Code beta, Agent SDK system line (R6) | W4: one consistent Agent SDK profile captured from the real SDK on this host; cloak switch wired | P1 |
| Cloaking as Claude Code is the room's remedy (C08, C09, C19) | Partial cloak, same for every account (R5, R6) | W4 single captured profile; W5 fixed egress per account; no TLS fingerprint forgery in this plan (section 8 explains why) | P1 |
| CLIProxyAPI vs opencodex vs cliproxyapi-rs: CLIProxyAPI best fit, none removes ban risk, blocks within days reported (C17-C23) | mahoquot already has selection, cooldown, management UI; lacks per-account egress and consistent identity | W4, W5 borrow the two missing pieces; section 5 records what each project does | P1 |
| Multi-account switching breaks the cache; 48% of cost from 3.8% of turns; pin account per session at the gateway (C27-C29, C34, C35, C42) | Affinity in memory, lost on restart, rebinds on any unhealthy moment; new sessions spread by reset time; a per-request id is accepted as a session key (R2, R2b, R3) | W2: durable, strict Claude pinning with better fallback keys; W7 measures the effect | P0 |
| 30 s debounce helps wake count, not cache breaks (C30, C33, C36-C44) | Not a gateway concern; batching happens in the agent harness | No gateway change. Recorded so nobody adds request delaying to mahoquot: it would add latency without fixing cache breaks | none |
| A 403 `permission_denied` looked like a ban but was not; owner made the proxy switch accounts on 403 (C55-C57) | Every non-model 403 is retried on more accounts (R1) | W1: classify first; transient fails over, enforcement benches and stops | P0 |
| Making about 25 accounts got all of them banned (C59, C60); frequent IP change and virtual cards are generic fraud signals (C66) | 11 Claude accounts, one egress, one fingerprint, regular probes (R4, R5) | W3 quieter warmup; W5 stable per-account egress; section 8 recommends fewer accounts | P1 |
| Support refused a refund citing terms violation (C67) | No record of enforcement-class responses | W1 persisted suspected list with reason and time; W6 console fields for it | P2 |
| ccapi closing; move to GLM via OpenRouter, stay model-agnostic (C01, C02, C26, C71) | mahoquot already routes Zhipu, xAI, Codex, Antigravity, Cline | No new code. Section 8 lists the API key and other-model alternatives | none |
| Heavy subscription users are loss-making, which explains limits and bans (C68-C70) | n/a | Section 8 risk statement | none |

## 7. Work items

**Execution order is strictly sequential: W0, W1, W2, W3, W4, W5, W6, W7, one commit each.** Several items touch `relay.rs` and `management/settings.rs`, so nothing runs in parallel. Every item applies only to members where `member.kind() == ProviderKind::Claude && !member.is_nekos_relay()`, called *Claude members* below; *Claude OAuth members* additionally have `relay_api_key().is_none()`. Codex, Antigravity, Cline, Kiro, Z-code and Devin paths must behave byte-for-byte as before. Each item's tests include a regression case that a Codex member hitting the same branch keeps today's behavior. All new tests use injected clocks (a `now_ms`/`now_unix` parameter on the new functions, as `due_at` already does with `Instant`), never sleeps. Gate for every item: `cargo test -p mahoquot-gateway` (W2 also `cargo test -p mahoquot-router`) and `cargo clippy -p mahoquot-gateway -- -D warnings` pass. Deploy (build plus `systemctl --user restart mahoquot-gateway`) is an operator step after each wave and is not part of this plan's research session.

### W0. Claude API-key accounts first (P0)

The sanctioned path from section 5.1: Max/Team monthly API credits, or a Console key, used with `x-api-key`. mahoquot already sends a Claude member with an `api_key` as `x-api-key` with no OAuth identity headers (`account.rs:891-893`) and skips cloak and cache injection for it (`relay.rs:1001-1003`).

- Write scope: `crates/gateway/src/management/settings.rs` (two new fields), `crates/gateway/src/relay.rs` (`eligible_indices`, after `reorder_claude_slots` at `:2676`), README section. No new endpoint: a Claude API-key account is a Claude credential file with `api_key` set (`crates/providers/src/claude.rs:75-76`), added through the existing `POST /auth-files/import` (`management/accounts.rs:921`).
- New setting `claude-route-mode`: `oauth_first` (default, today's order), `api_key_first` (Claude API-key members sorted before Claude OAuth members, stable within each group), `api_key_only` (Claude OAuth members removed from `eligible`). Validated as an enum at save.
- New setting `claude-api-key-budgets: {<account id>: <token limit per calendar month, UTC>}`. Decision: the budget is counted in **weighted token units, not USD**, because the existing price snapshot ignores `cache_write_tokens` (`request_history.rs:1556-1607`) and so would undercount Claude spend. Weighted token units (an operator-defined approximation, not Anthropic's billing formula) = `SUM(input_tokens + output_tokens + cache_write_tokens) + SUM(cached_input_tokens) / 10` from `usage_events` for that account since the first of the month UTC; the operator converts their dollar budget to units with the current price sheet (README table); the conversion depends on model and on the input/output/cache mix, so it is a rough threshold. Cached in memory for 60 s. When usage >= limit, the member is skipped in `eligible_indices`. If the history store cannot be read, the member is treated as **over** budget (fail closed) and a `warn` log names it. This is an approximate threshold: up to 60 s of traffic can overshoot it, which is accepted.
- Risk: API-key traffic costs money beyond the included credits. Mitigated by the budget; with `api_key_only`, an exhausted budget gives the client the existing `no available accounts` error rather than spilling to OAuth.
- Ordering is a preference: an existing router affinity binding can still pick an OAuth member under `api_key_first`; only `api_key_only` guarantees no OAuth use. Documented in the README section.
- Verify: unit tests for the three modes on a pool of two OAuth and two API-key Claude members plus one Codex member (Codex order unchanged); fail-closed test with an unreadable history path; unit test that a member at its token limit is skipped and one below is not, with a fixed clock on the first and last second of a month; the existing header test extended to assert an API-key member sends `x-api-key`, no `authorization`, no `x-app`, and no injected system line.

### W1. Classify Claude denials; never spread a denial across accounts (P0)

- Write scope: new `crates/gateway/src/claude_denial.rs`; `relay.rs` 403/401 branches (`:4373-4420`), the early-SSE failure path that consumes `AnthropicPreamble::Failed`, and the 500-504 branch (`:4319`); `management/settings.rs` (persisted list); `warmup.rs` `eligibility` (`:193`); `management/accounts.rs` (clear endpoint).
- `classify(status: u16, body_or_preamble: &str) -> Denial` for Claude members. Two call sites:
  - HTTP: called on the **first** upstream response, **before** the 401 refresh-and-replay block (`relay.rs:4019`). An `Enforcement` result skips refresh entirely.
  - Early SSE (upstream HTTP 200 with an `error` event, consumed at `relay.rs:4188-4232`): called with `status = 403` when the normalized code is `permission_denied` and with `status = 401` when it is `unauthenticated`, and the string after `<code>: ` as the message (`compat/mod.rs:194-213` keeps the upstream message). For these, the client receives the gateway's existing JSON failure body (`{"error":{"message":<upstream message>,"type":"upstream_error","code":<code>}}`) with the mapped status, as today; the original SSE frame is not replayed.
  Matching is case-insensitive on the message text:
  1. `ModelDenied`: the existing `is_account_scoped_model_rejection` check. Unchanged behavior.
  2. `Enforcement`: status 400, 401 or 403 (including the SSE mapping above) and the message contains any of `only authorized for use with claude code`, `has been disabled`, `suspended`, `terms of service`, `usage policy`, `organization has been`. Effect: the account is added to the persisted `claude-suspected-accounts: {<id>: {reason, at_unix}}` setting (written through the settings store, as `persist_scoped_usage` does at `relay.rs:482`), the request is **not** retried on another account, and the upstream status and body go to the client (HTTP path) or the JSON failure body above (SSE path).
  3. `Denied`: any other 403, including `permission_error` and a first `Request not allowed`. Effect: not retried on another account; upstream response returned to the client; an in-memory per-account counter records it. A second `Denied` on the same account within 600 s promotes it to `Enforcement` handling. This replaces today's 403 failover for Claude members only; the Codex/Antigravity 403 branch is unchanged.
  4. `Auth`: 401 after the existing refresh-and-replay. Unchanged (`AuthFailed`, failover).
  5. `Transient`: 500-504 (existing) **and 529** (new for Claude members), handled by the existing 500-504 branch.
- Suspected accounts are filtered out in `eligible_indices` and in warmup `eligibility` (new reason `suspected`). Nothing clears them automatically, including restarts, because the list is in settings. A new route `DELETE /accounts/{account_id}/suspected` in `account_management_routes` (`management/accounts.rs:909-933`) removes one entry through the settings store.
- Risk: a false match benches a healthy account until cleared. The reason and time are visible in the console (W6), and `Denied` needs two hits before benching.
- Verify: tests for the 401-with-enforcement-text path (refresh is not called; fake refresh endpoint receives 0 requests) and for the early-SSE path (fake upstream answers 200 with an `error` event carrying `permission_error` + `has been disabled`; second upstream gets 0 requests; client gets the JSON failure body); table-driven unit test over recorded bodies (one per class, including the section 5.1 strings and the C55 `permission_denied` body); relay test with two fake Claude upstreams: after an `Enforcement` 403 the second upstream gets 0 requests and the account appears in settings; after a 529 it gets 1; a Codex member's 403 still fails over; a restart test (rebuild state from the saved settings) keeps the account benched.

### W2. Durable, strict session pinning for Claude OAuth (P0)

- Write scope: new `crates/gateway/src/affinity_store.rs`; `relay.rs` (key functions `:3577-3711`, the binding lookup after `eligible_indices` at `:3795`, the attempt loop at `:3841-3880`); `state.rs` (store field); `main.rs` (load at start); `crates/router/src/lib.rs` (`select_forced` only).
- Key: the existing chain (`relay.rs:3841-3843`) is kept, except that for requests that can only be served by Claude members, `x-client-request-id` is skipped as a session header. The stored key is a **stable** 64-bit FNV-1a digest (implemented in the new file, no new dependency). For header and id keys it hashes the raw value; for the two body fallbacks it hashes the **original bytes** they read today (the trimmed first user text, and the first 2 KiB of the body), computed in parallel to the existing `DefaultHasher` strings, which stay unchanged for non-Claude routing.
- Store: `~/.mahoquot/auth/claude-affinity.json`, mode 0600, map `digest -> {account_id, last_used_unix}`. Written by a background task at most every 5 s when dirty (`tokio::fs`, never on the request path); loaded at start; entries older than 6 h since last use are dropped on load and on write; capped at 20,000 entries, oldest dropped first.
- `claude-route-mode: api_key_only` (W0) takes precedence: OAuth bindings are ignored entirely, including the short-cooldown 429 rule, and OAuth members never receive the request.
- Binding lookup happens **right after `eligible_indices` and before the empty-pool exits** (`relay.rs:3795-3841`), for any request whose resolved route includes Claude OAuth members, regardless of which member ranks first. The bound account must still pass the request's model and scoped-key restrictions (it must appear in the unfiltered candidate set that `eligible_indices` computes before health filtering); otherwise the binding is ignored for this request.
- The durable binding **overrides** the router's in-memory affinity for Claude OAuth: when a durable binding exists, the gateway calls the router with that member forced (new `Router::select_forced(member_id, affinity_key)` that updates `last_served` and writes `affinity_key -> member_id` into the router's session-keyed map, `router/src/lib.rs:45-49`), and after a successful turn writes the same pair again, so `select_index` (`relay.rs:2700-2775`) and `Router::select` (`router/src/lib.rs:93-110`) cannot pick a member rebound during an earlier one-off failover. Write scope adds `crates/router/src/lib.rs` (the one new method).
- Selection rules:
  - If the store has a binding and the bound member is in `eligible`, it is tried first; `reorder_claude_slots` does not move it.
  - If the bound member is not in `eligible` because of a **cooldown that ends within 120 s** (`Health::Cooldown { until_unix_ms }` from a plain 429), the gateway answers `429` with `retry-after` set to the remaining seconds and does not rebind. This happens before any upstream call, so nothing has been sent downstream.
  - In every other case (bound member removed, disabled, `AuthFailed`, suspected by W1, quota-exhausted, longer cooldown, or not serving this model) the request goes through today's selection, and the account that first answers 2xx becomes the new binding.
  - Within one request, 500-504/529 and transport failures still fail over to another account (W1 class 5) before any bytes are sent, but the binding is **not** moved by that one-off; the next turn tries the bound account again.
  - Nothing is retried after downstream headers are flushed (gateway rule in `crates/gateway/AGENTS.md`).
- Risk: a long-lived session on an account that is slowly running out keeps using it until it is exhausted. This is intended: one rewrite at exhaustion instead of many along the way.
- Verify:
  - unit: store round-trip (write, drop state, reload) keeps bindings; entries past 6 h are dropped; FNV digest of a fixed string equals a fixed constant;
  - relay tests with fake upstreams and a fixed clock: (a) turn 2 of a session goes to the turn-1 account even when `reorder_claude_slots` ranks another first; (b) bound account in a 60 s cooldown gives 429 with `retry-after` and zero upstream calls; (c) bound account in a 30 min cooldown rebinds; (d) a 503 on the bound account serves that turn elsewhere and turn 3 goes back to the bound account; (e) `x-client-request-id` alone does not create a binding for a Claude request but still does for a Codex request; (f) bound account cooling for 60 s while the only other candidates are API-key members still gives 429, not an API-key answer; (g) after turn 2 failed over once on a 503, the router's in-memory map does not override the durable binding on turn 3; (h) an upstream error after the first downstream byte is passed through and not retried;
  - live after deploy (read-only SQL from W7): share of follow-on Claude turns that switch account, baseline 1.8% (section 4), target below 0.3%; sessions with 10 or more requests on more than one account, baseline 43%, target below 10%.

### W3. Quieter Claude warmup (P1)

- Write scope: `crates/gateway/src/warmup.rs` (`due_at` `:371`, `warmup_request` Claude arm `:446-457`).
- Operator action first, no code: set `warmup.providers.claude.enabled: false` in the live settings (it is `true` today). Code changes below apply when an operator turns it back on.
- For Claude OAuth members, `due_at` additionally requires: local time not between 01:00 and 08:00; at least `jitter` seconds since the window's reset, where `jitter = 180 + fnv64(account_id + reset_unix) % 1320` (3 to 25 min, fixed per account and window so a restart gives the same answer); and no successful probe recorded for this `reset_unix` in memory. The anchor `reset_unix` is read directly from the usage snapshot (`usage.primary.reset_at_unix`, the value `is_quota_window_active` reads at `warmup.rs:306`), not from that function's return value, which drops past resets. Three cases: (a) `reset_unix > now`: the window is active and the existing check at `warmup.rs:403-404` already refuses; (b) `0 < reset_unix <= now`: a known, elapsed reset, so a probe is allowed from `reset_unix + jitter`; (c) missing or 0: unknown, so the Claude account is **skipped** until the quota poller reports one, with no synthetic anchor. After a successful probe the poller reports a new future reset, which moves the account back to case (a). After a restart, the first probe of a window can repeat once; accepted.
- Probe body: user text `Hi`, `max_tokens: 16`, otherwise as today.
- Verify: unit tests with a fixed clock and a stubbed usage snapshot: no probe when the reset is unknown (case c); no probe while it is in the future (case a); for an elapsed reset (case b) no probe before `reset_unix + jitter` and one probe at it; no probe at 03:00; none before the jitter; one at the jitter; none again in the same window; jitter differs between two accounts and stays within 180-1500 s; Codex and Antigravity probes unchanged (existing tests `codex_warmup_omits_parameters_the_upstream_rejects`, `antigravity_warmup_caps_output_to_one_token` still pass).

### W4. One honest client profile; a working cloak switch (P1)

- Write scope: `account.rs:891-920`, `compat/claude.rs:612-697`, `relay.rs:999-1005` and `:1139-1156`, new fixture `crates/gateway/tests/fixtures/agent_sdk_wire.json`.
- `disable-claude-cloak-mode: true` (`settings.rs:426`) makes Claude OAuth requests carry only `authorization`, `anthropic-version`, `anthropic-beta: oauth-2025-04-20` plus the client's own betas, and the client's own `user-agent`/`x-app`/`x-stainless-*` headers if present. No system line and no cache breakpoint are added. Model rewriting and protocol conversion still apply.
- With the cloak on (default), the gateway presents one client: the **Claude Agent SDK** (TypeScript), matching the system line it already injects. Reason: the legal page tells Agent SDK developers to use API keys, and staff said nothing changed for personal Agent SDK use with a Max subscription (The New Stack, section 5.1); copying the Claude Code harness instead (billing block, its tool set) is what the 2026-01-09 anti-spoofing safeguards target. This plan adds no billing block, no tool renaming and no `metadata.user_id`.
- Profile values come from two captures of the real current Agent SDK sending one request each to a local listener on this host. Headers equal in both captures (user agent, `x-stainless-*`, `x-app` if sent, betas) form the profile; headers that differ are per-session and are not copied; `authorization` is replaced by `<redacted>`. The fixture holds the stable set. `x-stainless-os`/`arch` therefore match this host instead of `macos`/`arm64`.
- Risk: the profile ages as the SDK updates; a stale version is itself a signal. The fixture records the SDK version, and the README gets a step to recapture on SDK upgrades.
- Verify: unit test that a Claude OAuth member's headers equal the fixture apart from `authorization`; test that with `disable-claude-cloak-mode=true`, the upstream body has no added system block and no added `cache_control`, and the identity headers (`user-agent`, `x-app`, `x-stainless-*`, `anthropic-beta`) are exactly the list above (content and transport headers are not compared); existing cloak tests in `compat/claude.rs:1880-1960` updated, not deleted.

### W5. Fixed per-account egress for Claude OAuth (P1)

- Write scope: `management/settings.rs` (new field and validation), `state.rs` (prebuilt client map and its rebuild on settings reload).
- New setting `claude-account-egress: {<account id>: <proxy URL>}` (http, https or socks5 URL; validated by building a client at save time). Claude-only; interface binding is out of scope.
- Clients are **prebuilt**: at settings save and at start, one `reqwest::Client` per entry is built and stored in `AppState` (new `claude_egress_clients` map, swapped atomically on reload). A URL that does not build is rejected at save, so dispatch never builds a client and `client_for_target_at` stays infallible. If a stored entry somehow failed to build at start (for example a settings file edited by hand), that account is marked `Disabled` with reason `egress invalid` instead of being given the direct client.
- Precedence in `client_for_target_at` (`state.rs:867-890`): a Claude member with a prebuilt egress client uses it; everything else is unchanged (today Claude goes out direct, section 2).
- A proxy that refuses the connection at request time is a transport error for that account (W1 class 5). It never falls back to the direct client, which would change the account's IP.
- Never rotates automatically.
- Verify: unit tests: two Claude members with different URLs get different clients; a hand-edited invalid entry at start disables only that account; a member without an entry gets the same client as before (direct when `cline` policy is enabled); an invalid URL is rejected at save; a Codex member is unaffected.

### W6. Visibility (P2)

- Write scope: `management/accounts.rs` (fields on the account JSON), `warmup.rs` (per-account 24 h probe counter), console source in the `quotio-rs` repo `crates/monitor-ui/frontend` synced into `ui/index.html` with `sync:proxy` as described in `README.md:107-116`.
- Per Claude member: suspected flag with reason and time (W1), current affinity bindings count (W2), probes in 24 h (W3), egress URL host only (W5), route mode (W0).
- Verify: `curl` of the account endpoint shows the fields on a test instance; console screenshot at desktop width.

### W7. Measure (P2)

- Write scope: new `scripts/claude_switch_cache.sql`, run with `sqlite3 -readonly` against `request-history.sqlite`.
- Reproduces section 4: within each `session_identifier`, successful Claude turns ordered by `occurred_at_ms`, then `id` as a tie-breaker; a turn is a *switch* if the previous turn's `account_identifier` differs. Outputs per class (first, same, switch): turns, `SUM(cache_write_tokens)`, `SUM(cached_input_tokens)`, write share = write / (write + read); plus sessions with 10 or more requests on more than one account.
- Windows: the 7 days before the W2 deploy and the 7 days after, passed as parameters.
- Verify: on the section 4 window the script reproduces the section 4 numbers within ±1% (the 3,009 vs 3,010 difference in section 4 comes from two queries run a minute apart on live data); write shares printed to 3 decimals.

## 8. Residual risk and terms

All statements here about Anthropic's rules come from section 5.1.


Stated plainly so nobody reads this plan as making the setup safe:

- **Using a Claude subscription through a third-party gateway is not what the subscription is for.** Anthropic's consumer terms and its Claude Code usage policy treat subscription OAuth as personal use through Anthropic's own clients. Routing it through a shared gateway, pooling several subscriptions, or presenting a different client as Claude Code all go against that. Nothing in W0-W7 changes this, W0 aside: API-key traffic is the allowed path. The citations are in section 5.
- **Cloaking cuts one signal and adds another.** W4 makes the identity consistent; it does not make it genuine. If Anthropic checks something mahoquot cannot reproduce (TLS fingerprint, request timing, client attestation, account-level usage shape), a consistent cloak still fails. This plan deliberately does not add TLS fingerprint forgery: it would deepen the misrepresentation without removing the account-level signals.
- **Pool size is the largest knob, and it belongs to the operator, not the code.** The room's only report of a whole-pool ban came from about 25 accounts (C59, C60). The live pool has 11 Claude accounts. Each extra account behind the same gateway raises the chance of linked enforcement.
- **Pinning reduces cost; it does not reduce ban risk on its own.** W2 is for cache cost and for fewer account switches. Fewer switches look more like one user per account, but this is a side effect, not a guarantee.
- **A ban takes the subscription with it.** Support refused a refund citing a terms violation (C67). Treat the subscription fee as money at risk.

Safer alternatives, from least to most change:
1. **Anthropic API key** (Console, pay as you go) through mahoquot's existing relay-key path. This path is the allowed one for third-party tools. It costs more per token, which makes cache hits worth more; API-key members already keep their in-memory affinity, and W2's durable pinning is limited to OAuth members.
2. **Claude on Bedrock or Vertex** under your own cloud account, also allowed for third-party clients.
3. **Other models** mahoquot already routes (Codex, Zhipu/GLM, xAI) for subagents and bulk work, keeping Claude for the sessions that need it.
4. If you keep subscription OAuth: one account per real person, used from that person's own machine and IP, with W1-W5 applied.

This plan recommends option 1 or 3 for automated agent fan-out and limits subscription OAuth to option 4.

# Reviewer gates

Every gate is an independent `oracle` subagent given the full diff, the register,
and the proof artifacts, and required to return `VERDICT: PASS|FAIL(n)` with one
numbered objection per defect. A gate's objections are only accepted when the
cited code contradicts the register or a reference; a gate opinion that our
implementation is the better one is not an objection.

## Gate 1 — first full review

Verdict: **FAIL(5)**.

| # | objection | disposition |
|---|---|---|
| 1 | L2-03 prompt accounting double-counted cached tokens | fixed: prompt excludes the cached subset, matching B |
| 2 | L3-03 `response.incomplete` was rendered as a transport error | fixed: length/content-filter map to a finish reason with usage |
| 3 | L4-07 refresh claim had no release on a definitive HTTP response | fixed: complete/release split by outcome |
| 4 | L5-01 EOF without a terminal event was accepted | fixed: frame-aware terminal scan, EOF now errors |
| 5 | `tests/t10_anthropic_compat` still expected the pre-L1-01 totals | fixed: expectation updated with the register row |

## Gate 2 — double-observation review

Verdict: **FAIL(1)**.

| # | objection | disposition |
|---|---|---|
| 1 | The preflight chunk was scanned for a terminal event and then scanned again inside the forwarded stream, corrupting a terminal frame split across the first two chunks | fixed: `terminated_raw_stream` is the single observation point; the preflight chunk is replayed through it. `double_observation_corrupts_a_split_terminal_frame` fails on the old shape |

## Gate 3 — claim lifecycle and test validity

Verdict: **FAIL(1)**, with 4 items verified correct.

| # | objection | disposition |
|---|---|---|
| 1 | L4-07: a retained receipt-less claim had no reconciliation or expiry, so one ambiguous transport failure wedges the account permanently | fixed: `CODEX_CLAIM_ABANDON_AFTER = 6h`; `CodexRefreshClaim::acquire` takes over an abandoned claim after that window and routes the account to the existing grant-rejection/re-auth path. `codex_abandoned_claim_reaches_an_actionable_state` backdates the claim, serves `invalid_grant`, and asserts the claim is gone |

Gate 3 also reported a test caveat: `raw_stream_observes_the_preflight_chunk_exactly_once`
sent no `Content-Type`, so it took the non-SSE bypass and never exercised the
helper it names. Fixed by adding `content-type: text/event-stream`. The test was
then proven load-bearing: making `terminated_raw_stream` skip observing the first
chunk fails it with `upstream SSE ended before a terminal event`.

Gate 3 independently ran `cargo test -p mahoquot-gateway --lib`: **908 passed,
0 failed**.

## Gate 4 — proof integrity and certification

Verdict: **FAIL(3)**.

| # | objection | disposition |
|---|---|---|
| 1 | The proof harness could republish a stale PROVEN row: a mutation that became a bad table edit, or a row repointed at a different test, kept its old verdict | fixed: `BAD_TABLE` invalidates the id, and the merge drops any row whose test name no longer matches the table |
| 2 | L1-09 was wrongly rejected — the four-breakpoint cap is a real defect, and the reference enforces it | fixed: cap reinstated in `compat/claude.rs`; `L1-09` proven red/green |
| 3 | `EQUAL` cells were claimed for Zcode and OpenCode, which neither reference implements end to end | fixed: verdict vocabulary extended with `EQUAL (shared)` and `NOT-CERTIFIED`; uncertified cells re-marked and excluded from every equal count |

## Gate 5 — serialization coverage and proof binding

Verdict: **FAIL(2)**.

| # | objection | disposition |
|---|---|---|
| 1 | The claim that the relay cap was the single serialization point for every Anthropic-shaped body was false: the Generic `anthropic` adapter returned earlier and forwarded all five client markers | fixed: the cap now runs inside `openai_to_anthropic`, so every translation inherits it; the relay cap stays for bodies that gain markers after translation. New row `L1-11` proven red/green — removing the translator cap fails `generic_anthropic_adapter_caps_client_breakpoints` with "saw 5" |
| 2 | Stale PROVEN rows could still survive a mutation-table edit: the ledger bound only `old`, `test`, and the fixed hash, so setting `new = old` or repointing `testFile` left every predicate satisfied while the row was unselected | fixed: the ledger now also binds the mutation hash, the target file, and the test file, and a full sweep fails unless every table row is represented in the ledger |

Gate 5 confirmed as correct: the register recounts to its stated totals, all
current proof hashes match, the twelve-block matrix has rows 1-8 exactly once,
and the deviation set is bidirectionally consistent.

## Gate 6 — citation truth and machine-checkable linkage

Verdict: **FAIL(3)**, all three dispositioned.

| # | objection | disposition |
|---|---|---|
| 1 | `mutations.json` omits register id `L6-03` and carries `L6-EXEC`, so the id set is not an exact mirror of the register's FIX rows | fixed structurally: `L6-EXEC` now declares `hardening: true`, and the harness asserts both directions on every run — every table id must name a register row (or be a declared hardening entry), and every register row whose outcome is `fixed` must have a table entry. The prose accounting in `defects.md` is therefore enforced, not merely asserted |
| 2 | `results.json` rows carried no explicit `status` and omitted `testFile` for the 48 `--lib` rows, so `PROVEN` lived only in `red-green.md` | fixed: every ledger row now writes `status: "PROVEN"` and an explicit `testFile: null` for library-target rows. The first attempt at this **regressed the ledger to 3 rows**: the writer normalized `testFile` to `null` while the invalidation predicate still compared raw values, so `null !== undefined` invalidated all 48 library rows. The predicate now normalizes both sides (`(row.testFile ?? null) !== (item.testFile ?? null)`); the regression was caught by the sweep's own `incomplete` list, not by a reviewer |
| 3 | Three of six sampled citations were stale: `claude.rs:1326-1329,1625-1629` (cache usage), `claude.rs:857-861` (max tokens), `gemini.rs:516-548` (thinking budget) | fixed: repointed to `claude.rs:1610-1623` (`anthropic_usage`), `claude.rs:908` (`max_tokens`), `gemini.rs:644-692` (`build_thinking_config`). A follow-up pass then found and fixed two more the gate had not sampled — `claude.rs:1333-1356` → `claude.rs:1495-1519` (`estimate_input_tokens`) and `gemini.rs:648-692` → `:644-692` — and confirmed every one of the 121 `crate:line` citations in the register and lanes is in-bounds |

Gate 6 confirmed as correct: all 51 proof hashes match the current table
(three recomputed independently), the breakpoint cap is load-bearing at both
`claude.rs:983` and `relay.rs:1776`, the mutation-residue grep is empty, the
deviation set is bidirectionally consistent at 13, and `final9.log` shows 111
result blocks with zero failures.

### Gate 6 follow-up — the citation set is now audited exhaustively

The gate sampled six citations and found three stale. Since a 50% miss rate in
a sample means the population cannot be trusted, every citation in the
register and the lanes was re-derived from the current source rather than
spot-checked:

- **11/11 register matrix citations** now resolve to the construct they name.
  Seven were wrong and were repointed: cache telemetry (`gemini.rs:1620-1660`
  → `:1845-1856`, where `cached_tokens_known` states the zero-vs-absent rule),
  upstream stall budget (`mod.rs:568-578` → `:595-601`), idle heartbeats
  (`:578-583` → `:743-749`), image detail (`request.rs:369-378` → `:422-424`),
  weighted-RR refusal (`router/src/lib.rs:80-100` → `management/scalar_table.rs:64-74`,
  the function that actually refuses the strategy), quota header projection
  (`relay.rs:4222-4260` → `:4230-4272`, `usage_header_prefix` + `capture_usage`),
  and estimate-tokens (`claude.rs:1333-1356` → `:1494-1519`).
- **199 lane citations** were machine-checked against the resolved source: none
  is out of range and every one starts on a non-blank line. Twelve started on a
  blank line by one or two and were shifted onto their anchor.
- This matters because a citation that points at a neighbouring block is
  indistinguishable from a citation invented after the fact.

The first rebuild sweep after the ledger schema change **regressed the ledger to
3 rows**: normalizing `testFile` to `null` in the writer without normalizing
the comparison made `null !== undefined` invalidate all 48 library rows. The
predicate now normalizes both sides. The regression was caught by the harness's
own `incomplete` list, and the rebuilt ledger is `51/51 PROVEN` with
`bad: []` and `incomplete: []`.

## Gate 7 — independent re-audit of the repaired evidence

Adversarial read-only oracle (no edits, no cargo). Verdict: **FAIL(1)**.

It confirmed all five targeted repairs, the register/proof correspondence by
executing the extracted guard block (including injected-failure cases), all 51
ledger hashes, and the absence of unproven FIX rows. It independently recounted
`final9.log` and flagged the correct detail that gateway-lib is 919 outer-suite
passes plus one nested child-process result, not 920 distinct tests.

Its one valid objection: the machine-readable deviation document still carried
the pre-repair citation.

| # | Objection | Disposition |
|---|-----------|-------------|
| 1 | `docs/reference/provider-parity-deviations.json:25` (L1-D1 `count_tokens`) cited `claude.rs:1333-1356`, a range that builds assistant tool calls | **Valid, fixed.** The deviation declaration now cites `claude.rs:1495-1519`. The gate was right that repairing the register and lanes while leaving the JSON declaration stale is exactly the class of half-fix this audit exists to catch. |

That finding also exposed a defect in my own repair script: shifting a citation
whose start line was blank moved the *end* line too, lengthening eleven ranges
by one. All eleven were restored to their true ends, and the end line of every
range is now checked alongside the start.

Current citation state, machine-verified against the settled tree:

- 199 lane citations and 11 register citations: none out of range, none
  starting or ending on a blank line.

## Gate 8 — second independent re-audit

Adversarial read-only oracle. Verdict: **FAIL(3)**, all three valid.

| # | Objection | Disposition |
|---|-----------|-------------|
| 1 | `lanes/l5-streaming.md` cited `render.rs:323-340` for the chat keepalive, but that range starts in `open()` and the tick/keepalive body is 337-345 | **Valid, fixed** → `render.rs:337-345` |
| 2 | `defects.md` and deviation L5-07 cited `compat/mod.rs:659-675`; dispatch is 661-667 and the end overran into `StreamingBodyParams` at 670 | **Valid, fixed** → `mod.rs:661-667` + `render.rs:337-345` in both places |
| 3 | The Claude quota row cited `relay.rs:4485`, a Retry-After test comment, not token counting | **Valid, fixed** → `routes.rs:610-612` calling `estimate_input_tokens` (`compat/claude.rs:1498-1519`) |

Gate 8 also independently confirmed the L1-D1 fix and all four standing axes.

The pattern behind these three: every citation had been *moved* to a nearby
range at some point, and a moved citation silently keeps pointing at a
plausible-looking neighbour. The verifier was therefore widened from "start line
exists" to a four-surface check over `defects.md`, the deviations JSON, and all
six lanes, asserting for **339** citations that each is in range and that
neither its first nor its last line is blank. Current state: zero problems.

## Gate 9 — third adversarial re-audit

Verdict: **FAIL(4)**, all four valid.

| # | Objection | Disposition |
|---|-----------|-------------|
| 1 | `compat/mod.rs:661-667` is `StreamRenderer::tick` (impl starts line 626), not `StreamingParser::tick` | **Valid, fixed** — the construct was misnamed in my repair note |
| 2 | `gemini.rs:1479-1539,1078-1100,1157-1214` does not substantiate stop forwarding, `$ref` resolution, or response formatting | **Valid, fixed** — repointed to `:1254-1262` (stop), `:1388-1436` (refs, invoked `:1344`), `:1479-1539` (entry points) |
| 3 | `antigravity.rs:334` is a unit-test assertion vector, not load-call evidence | **Valid, fixed** — constant `:23`, builder `:202`, applied at `quota.rs:925,971,1028` and `management/oauth.rs:184,384` |
| 4 | 48 of 51 ledger rows carried `testFile: null` | **Valid, fixed** — `testFile` now names a real file on all 51 rows |

Objection 4 was a design defect, not a typo. The field had been written
present-and-null on the reasoning that a `#[cfg(test)]` module has no separate
file; but a null location carries no binding, so a row could point at a test
that had been moved or renamed and the ledger would not care. `testFile` now
holds the actual containing file, and the cargo target moved to its own
`integrationTest` field, since conflating the two is what forced the null in the
first place. Four rows whose regression test sits in a sibling module were
backfilled by locating the test function rather than assuming the mutated file.

Gate 9 also confirmed three semantic claims independently (member-ID sequencing,
weekly-reset ordering, snapshot-bound OpenCode admission) and both cache-cap exits.

### Tamper-evidence check for the new binding (Gate 10 pre-emption)

Gate 9's objection was that `testFile: null` carried no binding. The fix is only
worth anything if the new field actually participates in invalidation, so I
tested it adversarially rather than reading the code and assuming.

Changed exactly one field on one row (`L1-03.testFile`, from
`crates/gateway/src/compat/claude.rs` to `crates/gateway/src/compat/gemini.rs`),
touched nothing else, and ran the harness with `--dry`, which performs the full
ledger rebuild and invalidation pass without executing cargo.

Result: `incomplete: ["L1-03"]`, the row was deleted from the ledger, and the
ledger dropped from 51 to 50 rows. A row cannot survive a repointed `testFile`.
The same expression derives the field on write and on check, so `integrationTest`
is covered by the identical comparison. The row was then restored and re-proved.

## Gate 10 — ledger binding audit

Verdict: **FAIL(1)**, valid.

| # | Objection | Disposition |
|---|-----------|-------------|
| 1 | For the three integration rows (L3-09, L1-10, L1-11) a truthy `integrationTest` fully overrode `item.testFile`, so editing only `testFile` left the row PROVEN | **Valid, fixed** — both fields are now bound independently |

This was the right objection and it exposed that my Gate 9 fix had only moved the
problem. The binding still *derived* `testFile` from `integrationTest`, so on
those three rows one of the two fields was decorative. A field that is computed
from another field cannot also be a binding on it.

The fix makes both fields explicit and required on every row, adds a load-time
check that they agree (`testFile` must equal `crates/gateway/tests/<integrationTest>.rs`
when `integrationTest` is present), and compares *both* against the ledger row.
Editing either one now invalidates the row.

### Load-time consistency check, verified by injection

The new binding is only meaningful if a table whose fields disagree cannot
run at all. Injecting one bad row (`L1-10.testFile = does/not/exist.rs`) makes
the harness abort before any test with:

```
error: mutation table row L1-10: testFile must be crates/gateway/tests/t12_provider_relay.rs
```

So a disagreement between the two fields is a hard error, and editing either
field on its own is an invalidation. Both were exercised; the table was then
restored and the affected rows re-proved.

### Self-audit of the binding chain (before Gate 11)

Gate 10's objection generalised to a question I should have asked myself: which
table fields actually participate in invalidation, and which are decorative?
Enumerating the fields written to `results.json` against the fields compared on
the next run found one more of exactly the same defect:

`package` selects which cargo test binary runs, but it was written to neither the
ledger nor the comparison, so editing it would not have invalidated anything. It
is now written and compared like every other binding field. The remaining field,
`old`, is compared (via `row.fixed !== hash(item.old)`) and deliberately not
stored separately, because `fixed` *is* its hash.

### `package` binding, verified by omission

Proving a field is bound is easiest by removing it. Sweep #56 ran with the
pre-fix script in memory, so its ledger rows lacked `package` entirely. Running
the fixed harness with `--dry` against that ledger invalidated **all 51 rows**,
which is the correct behaviour: a row that does not record which package runs
its test cannot be trusted to describe a proof. Sweep #57 then re-proved the
whole register under the complete binding.

### Claim-accuracy corrections found by self-audit (before Gate 11)

Gate 11 was asked to check every number in C001-C003 against the artifacts, so I
ran that check first and found one figure that did not reproduce.

- C003 said `103 test binaries / 111 result blocks`. The log contains **104**
  distinct binaries plus **3** doc-test blocks, which is what sums to 111. The
  binary count was wrong by one; the total and the 1760/0 figures reproduce
  exactly from `final9.log`. C003 now states the measured figures.
- C002 listed five bound fields; after the `package` and `testFile` fixes it binds
  **seven**. C002 now lists them, and the JSON deviation sites were pinned to line
  ranges so all 13 are machine-checkable.

## Gate 11 — evidence and binding audit

Verdict: **FAIL(5)**, all five valid.

| # | Objection | Disposition |
|---|-----------|-------------|
| 1 | Source validation checked only that `old` still occurs; a changed test body, a deleted test, or edits outside the fragment kept the row PROVEN | **Valid, fixed** — both the mutated file and the test file are now bound by content hash |
| 2 | The merge never checked `row.status`, so a retained row reading `NOT_GREEN` was republished and rendered as PROVEN | **Valid, fixed** — `row.status !== "PROVEN"` invalidates |
| 3 | C002 called `integrationTest` required (48/51 rows omit it) and said "seven" fields while omitting `test` | **Valid, fixed** — C002 now separates required `testFile` from optional `integrationTest` and lists all eight identity fields |
| 4 | C003's binary count did not match the log, and the log contained no `EXIT=` sentinel | **Valid, fixed** — a fresh log with the sentinel was produced, and C003 quotes its measured figures |
| 5 | C001 claimed every row carries both reference sites; L2-04 cited `gemini.rs:1078-1100` for a builder that is not there | **Valid, fixed** — C001 now describes governing-site coverage, and L2-04 points at the real stop branch |

Objections 1 and 2 were the serious ones. Both were reverse-direction defects:
the binding was strong for identity metadata and silent about whether the
evidence still existed. A row could keep a valid identity while its test was
weakened, and a retained row's own status was never re-read. The ledger now binds
`hash(source)` and `hash(testFile)` and requires `status == "PROVEN"` before a row
may be carried forward.

Objects 3-5 were claim-accuracy defects: the prose and the artifacts had drifted.
The fix in each case was to state what the artifact actually contains, not to
weaken the artifact to match the prose.

### Adversarial verification of the Gate 11 fixes

Both reverse-direction objections were tested by actually staging the attack,
not by reading the new comparison and assuming it works.

**Attack 1 (objection 1).** `L3-02` mutates `compat/events.rs` but is proved by a
test in `compat/render.rs`. I inserted an early `return` into that test's body in
`render.rs` and changed nothing in `events.rs`. A `--dry` pass dropped `L3-02`.
Before the content binding, the row would have been carried forward with a valid
identity while its proving test was inert. Restored and re-proved afterwards.

**Attack 2 (objection 2).** Confirmed by code path: `row.status !== "PROVEN"` is
the first clause of the invalidating condition, so a retained row reading
`NOT_GREEN`, or with no status at all, is deleted rather than republished.

The ledger now records `sourceHash` and `testFileHash` on every row, so a change
to either file's content invalidates the row that depends on it.

### Self-audit between Gate 11 and Gate 12

Recomputing the register from the lane tables by verdict cell exposed a
counting defect that no gate had caught: the summary table carried
`51 FIX-class / 50 repaired / 3 BETTER / 13 DEVIATION`, but the lane rows
themselves summed to **50 FIX / 1 BETTER / 10 deviation cells**. Three separate
drifts were behind it.

- `L6-03` was DEVIATION in the register and in `provider-parity-deviations.json`,
  but its lane row still said `FIX`. Corrected to `DEVIATION`.
- `L5-06`/`L5-07` used `SKIP-BETTER`/`SKIP-DEVIATION`; `L2-S1`/`L3-S1` used
  `SKIP-BETTER`. Normalized to the register's own vocabulary. The `SKIP-` prefix
  was a third label set that made the cells uncountable.
- The BETTER total of 3 came from `L2-S1`, `L3-S1` and `L5-06`, but the lane
  tables now classify `L2-S1` and `L3-S1` as deviation-class rows, leaving one
  BETTER.

At that moment the table and the prose both read 63 rows = 50 FIX + 1 BETTER
+ 10 deviation cells, with 13 JSON declarations because `L6` folded
`D2`/`D3`/`D4` into one cell. **That state did not survive Gate 12**, which
objected that the `L6` prose it pointed to did not exist, and the three
declarations became real rows: the current counts are 66 = 50 FIX + 3 BETTER +
13 DEVIATION, recorded in `defects.md`. The `L2-S1`/`L3-S1` reclassification
described above was likewise wrong and was reverted; they are BETTER.

The lesson is the same as Gate 11's: a count that was never recomputed from its
own rows is a claim, not a fact.

## Gate 12 — final confirmation audit

Verdict: **FAIL(3)**, all three valid. The binding work passed; the failures were
again in claims and citations, not in the proof mechanism.

| # | Objection | Disposition |
|---|-----------|-------------|
| 1 | C003 said "104 binaries + 3 doc-test = 111", but the log has 107 `Running` binaries and the 111th result is a nested child-process result inside the gateway-lib block | **Valid, fixed** — C003 now states 107 binaries (4 unit-test, 103 integration) + 3 doc-test = 110 top-level, plus 1 nested = 111 |
| 2 | Lane 3 `L3-S1` cited `request.rs:369-378` for `image_url.detail` forwarding, but that range only builds the message; the forwarding is at `:422-424` inside `user_parts` (`:410-435`) | **Valid, fixed** — repointed, with the caller noted as context |
| 3 | The register said `L6-D2`/`D3`/`D4` were documented in the L6 prose, but the lane file never mentioned them | **Valid, fixed** — they are now real L6 table rows, so the register has 13 deviation rows matching the 13 JSON declarations one-for-one |

Objection 3 is worth naming plainly. Rather than add three prose sentences to
make an existing count true, the three declarations became register rows. The
count and the artifact changed together, which is the only version that stays
true under re-counting.

Gate 12 independently confirmed the parts that matter most: all 51 ledger rows
matched their table identities, `status`, and current source/test hashes; both
file-content hashes are compared; and the runnable selectors are bound.

## Gate 13 — final confirmation audit

Verdict: **FAIL(2)**, both valid, both documentation-accuracy defects in text I
had written in the same session. The artifacts and the proof mechanism passed
every check for the third gate running.

| # | Objection | Disposition |
|---|-----------|-------------|
| 1 | C002 said `integrationTest` is "present only on the three integration-target rows and absent on the 48 lib rows" | **Valid, fixed** — the key is written on all 51 rows (`proofs/mutate-proof.mjs:139`), non-null on 3 and `null` on 48. C002 now says exactly that |
| 2 | The register described "twelve remaining deviations / a twelve-entry list", but the `## DEVIATION` list names 13 IDs including `L6-03` | **Valid, fixed** — recount gives 13 list IDs, of which 12 have no row and `L6-03` has one. Both sentences corrected |

Gate 13 independently reproduced the lane accounting (66 = 50 + 3 + 13), the
C003 log numbers (107 binaries, 3 doc-tests, 111 blocks, 1760 passed, 0 failed,
`EXIT=0`), the 44/7 `testFile` split, all six sampled citations, all 13 deviation
sites, and the ledger binding including 51 PROVEN rows verified against disk.

The pattern across Gates 11-13 is consistent and worth recording: the harness,
the source fixes and the citations have been stable since Gate 11, while each
gate found a false sentence in the audit's own prose. Prose that restates a
count is the fragile part of this deliverable, and the only durable fix is to
derive every number from the artifact at the moment of writing it.

### Full rebuild after the Gate 13 corrections

`bun run mutate-proof.mjs` over all 51 rows, with the register now at 66 rows
and the three new deviation rows present:

```json
{ "provenTotal": 51, "ran": 51, "bad": [], "incomplete": [] }
EXIT=0
```

`proofs/red-green.md` regenerated with 51 rows and `Proven total: 51`. The
ledger split is unchanged and matches the prose: 44 rows where `testFile` is the
mutated file, 7 where it differs, 3 with a non-null `integrationTest`.

## Gate 14 — final confirmation audit

Verdict: **FAIL(1)**, valid and fixed. Everything else was confirmed by
independent recomputation, including 102/102 SHA-256 hash matches against the
files on disk.

| # | Objection | Disposition |
|---|-----------|-------------|
| 1 | `lanes/l1-claude.md:24` cited `compat/claude.rs:737-750` for keeping later system/developer content in history, but that range only initializes state and starts the effort branch; the insertion is at `:774-779` | **Valid, fixed** — repointed to `:774-779` with the loop entry `:744-749` noted as context |

Confirmed in the same pass: the ten ledger keys, the eleven comparison clauses,
the 44/7 `testFile` split, the `integrationTest` presence claim, all six lane
counts, the 66 = 50 + 3 + 13 total, the C003 log decomposition including the
nested result at line 908 inside the 919-passed block at line 931, five other
sampled citations, and bidirectional table/register correspondence.

The citation defect is the same class as Gates 12 and 13: a range that resolves
and is adjacent to the right code, but does not itself contain the behavior. A
range check cannot catch it; only reading the cited lines can.

## Gate 15 — citation substance

Verdict: **FAIL(7)**, all seven valid, all the same class: a range that resolves
and sits next to the right code but does not itself contain the claimed
behavior. This is exactly the class a range validator cannot catch, and Gate 15
was told to hunt it. Every objection was checked against source before fixing.

| # | Row | Wrong range | Repointed to |
|---|-----|-------------|--------------|
| 1 | `L1-D2` | `claude.rs:690-703` (normalization + translator entry) | `claude.rs:719-729` (the reject) |
| 2 | `L3-08` | `request.rs:137-140,163-164` | `request.rs:143-144` (summary copy), effort at `:139-142` |
| 3 | `L4-03` | `refresh_exec.rs:15-24,150-162` | `refresh_exec.rs:49-56,63-78` (body-free Display/Debug) |
| 4 | `L6-02` | `cursor.rs:449-460` (tool events) | `cursor.rs:499-519` (Get/SetBlob) |
| 5 | `L6-03` | `cursor.rs:508-511` (blob capacity) | `cursor.rs:568-570` (bridge, then local fallback) |
| 6 | `L6-D2` | `compat/zcode.rs:151-200` (half test setup) | `compat/zcode.rs:151-169` + `providers/zcode.rs:37-39` (plan-only base) + `:87-95` (model normalization) |
| 7 | `L6-D4` | `meta.rs:13-16` cut before the UA value; `compat/meta.rs` has no protocol choice | `meta.rs:11-18` (full constants) + `compat/meta.rs:267-410` + `relay.rs:1464-1477` (`/responses`, `Protocol::Codex`) |

None of the seven changed a classification or a remedy: the behavior exists in
every case, and only the pointer was wrong. `L6-03`'s deviation stands on the
separately cited bridge range.

After the fixes: **273 citation ranges, 0 problems**, and every new range was
read to confirm it contains the claimed behavior, not merely that it resolves.

## Gate 16 — pointer verification

Verdict: **FAIL(6)**, all six valid. Six of the seven Gate 15 repoints were
confirmed correct; `L6-D2` was still incomplete because no cited range showed
host selection. The gate then sampled eight rows no earlier gate had read and
found five more inaccurate pointers.

| # | Row | Fix |
|---|-----|-----|
| 1 | `L6-D2` | host selection is `url.rs:42-45` (plan base and `upstream_override`); added it, kept `compat/zcode.rs:151-169` for identity shaping |
| 2 | `L1-03` | outbound stop mapping is `claude.rs:964-977`, not `:916-924` |
| 3 | `L1-05` | widened to `claude.rs:316-334` so the URL-source branch at `:329-334` is inside the range |
| 4 | `L3-04` | call/result emission is `request.rs:319-327,355-366`, with the paired normalised IDs at `:321`/`:363` |
| 5 | `L4-05` | existing-account merge is `oauth.rs:1260-1299`, invoked from `:1302-1326`; the old ranges held only the `disabled:false` construction |
| 6 | `L6-05` | redacted-reasoning decode is `kiro.rs:422-424`, not `:428-435` |

Four rows sampled by this gate were confirmed accurate without change:
`L1-02`, `L3-S2`, `L4-04`, and `L6-D4`'s repoint. After the fixes: **273
citation ranges, 0 problems**, every new range read for substance.

Two gates in a row have now found only pointers, never a wrong classification,
remedy, or proof. The remaining risk is concentrated in rows no gate has read,
which is what Gate 17 targets.

## Gate 17 — exhaustive pointer sweep

Verdict: **FAIL(22)**. This gate read every row no earlier gate had touched:
11 of 33 were accurate, 22 pointed at the wrong lines. All 22 were verified
against source and repointed. None changed a classification, remedy or proof;
the fixes and their red/green proofs were never in question.

Representative corrections: `L2-02` finish-reason -> `gemini.rs:513-524,1614-1625,1940-1967` +
`render.rs:220-226`; `L3-02` done-only arguments -> `events.rs:135-174`; `L5-01`
raw-SSE preflight -> `relay.rs:2932-2967,2970-3003,3006-3017`; `L5-02` upstream
polling -> `compat/mod.rs:751-778`; `L6-06` usage/completion -> `kiro.rs:396-420,465-469,481-490`.

All 22 in `lanes/*.md`; after them: **278 citation ranges, 0 problems**.

## Gate 18 — repoint confirmation (PASS)

Verdict: **PASS**. All 22 Gate 17 repoints were confirmed by reading the cited
source ranges; each contains the relevant behavior or its repaired
implementation.

Arithmetic independently recounted from lane verdict cells:

| Lane | Total | FIX | BETTER | DEVIATION |
|---|---:|---:|---:|---:|
| L1 | 14 | 11 | 0 | 3 |
| L2 | 12 | 10 | 1 | 1 |
| L3 | 11 | 9 | 1 | 1 |
| L4 | 10 | 8 | 0 | 2 |
| L5 | 7 | 5 | 1 | 1 |
| L6 | 12 | 7 | 0 | 5 |
| **Total** | **66** | **50** | **3** | **13** |

`defects.md` agrees by exact ID sets. Ledger: 51 PROVEN rows = 50 FIX IDs +
`L6-EXEC`, with no `L6-03` proof row. No real defect; no out-of-scope
classification objections.

**Final state:** 66 findings (50 FIX + 3 BETTER + 13 DEVIATION), 51 red/green
proofs all PROVEN, 278 citation ranges clean.


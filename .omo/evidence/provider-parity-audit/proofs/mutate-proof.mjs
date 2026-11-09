#!/usr/bin/env bun
// Red/green proof harness for the provider-parity FIX register (C002).
//
// For each registered item: replace the fixed code with its pre-fix text,
// run the item's named regression test, REQUIRE it to fail, restore the fix,
// and REQUIRE the test to pass. Any item that does not go red-then-green is
// reported as unproven and fails the harness.
//
// Usage: bun run mutate-proof.mjs [--only L1-02,L4-07] [--dry]
import { $ } from "bun";

const ROOT = "/Volumes/T9-Mac/project/mahoquot/mahoquot-proxy";
const OUT = `${ROOT}/.omo/evidence/provider-parity-audit/proofs`;
const args = process.argv.slice(2);
const onlyArg = args.includes("--only") ? args[args.indexOf("--only") + 1] : null;
const only = onlyArg ? new Set(onlyArg.split(",").map((s) => s.trim())) : null;
const dry = args.includes("--dry");

const invalidated = new Set();
const hash = (text) => new Bun.CryptoHasher("sha256").update(text).digest("hex");
const table = JSON.parse(await Bun.file(`${OUT}/mutations.json`).text());
const results = [];

// A row without these fields cannot be run at all; catching it here keeps a
// typo from being reported as a meaningful NOT_RED result.
for (const item of table) {
  for (const field of ["id", "package", "file", "test", "testFile", "old", "new"]) {
    if (typeof item[field] !== "string" || item[field] === "" && field !== "new") {
      throw new Error(`mutation table row ${item.id || "<no id>"} is missing ${field}`);
    }
  }
  // `testFile` is where the test lives; `integrationTest` is the cargo target,
  // and only an integration target has one. They are separate fields because a
  // lib test's file and its target are different kinds of thing. Both are
  // bound into the ledger independently below, so neither can be edited
  // without invalidating the row.
  if (item.integrationTest !== undefined && typeof item.integrationTest !== "string") {
    throw new Error(`mutation table row ${item.id} has a non-string integrationTest`);
  }
  if (item.integrationTest !== undefined && item.testFile !== `crates/gateway/tests/${item.integrationTest}.rs`) {
    throw new Error(`mutation table row ${item.id}: testFile must be crates/gateway/tests/${item.integrationTest}.rs`);
  }
}

for (const item of table) {
  if (only && !only.has(item.id)) continue;
  const path = `${ROOT}/${item.file}`;
  const original = await Bun.file(path).text();
  const occurrences = original.split(item.old).length - 1;
  if (occurrences !== 1) {
    invalidated.add(item.id);
    results.push({ id: item.id, status: "BAD_TABLE", detail: `old text occurs ${occurrences}x` });
    continue;
  }
  if (dry) {
    results.push({ id: item.id, status: "TABLE_OK", detail: item.test });
    continue;
  }
  const weakened = original.replace(item.old, item.new);
  if (weakened === original) {
    invalidated.add(item.id);
    results.push({ id: item.id, status: "BAD_TABLE", detail: "replacement identical" });
    continue;
  }
  const run = async () => {
    // A multi-word target must be spread as separate argv entries; Bun
    // interpolation of "a b" would arrive as one quoted argument.
    // `testFile` names where the test LIVES: the integration file for
    // integration targets, and the source file itself for a `#[cfg(test)]`
    // module. The cargo *target* is a different thing and only integration
    // files have one.
    const target = item.integrationTest ? ["--test", item.integrationTest] : ["--lib"];
    const r = await $`cd ${ROOT} && cargo test -p ${item.package} ${target} ${item.test} -- --exact`
      .quiet()
      .nothrow();
    const text = r.stdout.toString() + r.stderr.toString();
    // Require the named test to have actually run. A wrong module path makes
    // cargo report "0 passed; 0 failed" with exit 0, which proves nothing.
    const ran = /(^|\n)test .* \.\.\. (ok|FAILED)/.test(text);
    const passed = /test result: ok\. 1 passed/.test(text);
    const failed = ran && /test result: FAILED/.test(text);
    return { passed, failed, text };
  };
  try {
    await Bun.write(path, weakened);
    const red = await run();
    if (!red.failed) {
      invalidated.add(item.id);
      results.push({
        id: item.id,
        status: "NOT_RED",
        detail: red.passed ? "test passed on pre-fix code" : red.text.slice(-300),
      });
      continue;
    }
    await Bun.write(path, original);
    const green = await run();
    if (!green.passed) {
      invalidated.add(item.id);
      results.push({ id: item.id, status: "NOT_GREEN", detail: green.text.slice(-300) });
      continue;
    }
    results.push({ id: item.id, status: "PROVEN", detail: item.test, hash: hash(item.old) });
  } finally {
    await Bun.write(path, original);
  }
}

// Merge with any previous run so a lane-by-lane sweep accumulates one ledger.
//
// Every attempted item that did not go red-then-green is REMOVED from the
// accumulated ledger, not just the last one: a stale PROVEN row would
// otherwise survive a failed recheck and be republished as proven.
//
// A stored row is also bound to the fixed source it was proven against. When
// the mutation table's fixed text no longer matches the working tree the row
// is dropped, because the proof no longer describes this code.
const ledgerPath = `${OUT}/results.json`;
let ledger = {};
try { ledger = JSON.parse(await Bun.file(ledgerPath).text()); } catch {}
for (const id of invalidated) delete ledger[id];
for (const r of results) {
  if (r.status !== "PROVEN") continue;
  // Store every field that identifies the proof, not just the two the test
  // needs: the merge below re-derives all of them from the current table, so a
  // row can only be republished when its mutation, its target file, its test
  // file, and its test name are all still exactly what was proven.
  const item = table.find((entry) => entry.id === r.id);
  ledger[r.id] = {
    status: "PROVEN",
    test: r.detail,
    fixed: r.hash,
    mutation: hash(item.new),
    file: item.file,
    package: item.package,
    sourceHash: hash(await Bun.file(`${ROOT}/${item.file}`).text()),
    testFileHash: hash(await Bun.file(`${ROOT}/${item.testFile}`).text()),
    testFile: item.testFile,
    integrationTest: item.integrationTest ?? null,
  };
}
const tableById = new Map(table.map((item) => [item.id, item]));
// A row is bound to the fixed source AND to the test that proved it. Pointing
// an existing id at a different test leaves the old row describing a proof
// nobody ran, so the test name participates in the binding too.
for (const [id, row] of Object.entries(ledger)) {
  const item = tableById.get(id);
  if (!item) { delete ledger[id]; continue; }
  const source = await Bun.file(`${ROOT}/${item.file}`).text();
  const occurrences = source.split(item.old).length - 1;
  // A pathname is not evidence: the test can be weakened, deleted, or the
  // implementation changed outside the mutated fragment while every identity
  // field still matches. Both files are therefore bound by content.
  const testSource = await Bun.file(`${ROOT}/${item.testFile}`).text();
  if (
    row.status !== "PROVEN" ||
    occurrences !== 1 ||
    row.sourceHash !== hash(source) ||
    row.testFileHash !== hash(testSource) ||
    row.fixed !== hash(item.old) ||
    row.test !== item.test ||
    row.mutation !== hash(item.new) ||
    row.file !== item.file ||
    row.package !== item.package ||
    row.testFile !== item.testFile ||
    (row.integrationTest ?? null) !== (item.integrationTest ?? null)
  ) {
    delete ledger[id];
    invalidated.add(id);
  }
}
await Bun.write(ledgerPath, JSON.stringify(ledger, null, 2));
const proven = Object.keys(ledger).length;
const bad = results.filter((r) => r.status !== "PROVEN");
const lines = [
  "# Red/green mutation proofs (C002)",
  "",
  `Generated: ${new Date().toISOString()}`,
  "",
  `Each row reverts one fix to its pre-fix text, requires the named regression`,
  `test to fail, restores the fix, and requires it to pass.`,
  "",
  `| item | status | test / detail |`,
  `|---|---|---|`,
  ...Object.entries(ledger)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([id, row]) => `| ${id} | PROVEN | ${String(row && row.test ? row.test : row).slice(0, 160)} |`),
  "",
  `Proven total: ${proven}`,
];
await Bun.write(`${OUT}/red-green.md`, lines.join("\n") + "\n");
// Every table id must trace to a register row, and every register row whose
// outcome is "fixed" must have a table entry. This turns the prose accounting
// in defects.md into something the harness enforces on every run. Hardening
// entries (proven but not defects against either reference) declare
// `hardening: true` so they are allowed to carry an id the register rows do
// not use.
{
  const registerText = await Bun.file(
    `${ROOT}/.omo/evidence/provider-parity-audit/defects.md`,
  ).text();
  const registerRows = new Map(
    registerText
      .split("\n")
      .filter((line) => /^\| L[0-9]-[A-Z0-9]+ \|/.test(line))
      .map((line) => {
        const cells = line.split("|");
        return [cells[1].trim(), line];
      }),
  );
  const tableIds = new Set(table.map((item) => item.id));
  const untraceable = table
    .filter((item) => !registerRows.has(item.id) && !item.hardening)
    .map((item) => item.id);
  const unproven = [...registerRows]
    .filter(([id, line]) => /\| fixed /.test(line) && !tableIds.has(id))
    .map(([id]) => id);
  if (untraceable.length) {
    throw new Error(`table ids absent from the register: ${untraceable.join(", ")}`);
  }
  if (!only && unproven.length) {
    throw new Error(`fixed register rows with no proof: ${unproven.join(", ")}`);
  }
}
// A full sweep must account for every row in the table. Without this, an
// invalidated row could be dropped from the ledger while the run still exits
// zero, republishing a smaller proof set as if it were complete.
const incomplete = only ? [] : table.map((item) => item.id).filter((id) => !ledger[id]);
console.log(JSON.stringify({ provenTotal: proven, ran: results.length, bad, incomplete }, null, 2));
process.exit(bad.length === 0 && incomplete.length === 0 ? 0 : 1);

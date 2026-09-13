// One-shot CLI around the vendored traceless Aliyun captcha solver
// (captcha-solver.ts, opencodex PR #4437). The Rust gateway spawns this per
// challenge — process isolation replaces the reference's worker-thread host.
//
// Wire contract (consumed by crates/gateway/src/plan_captcha.rs, do not
// change without the Rust side): the solve request arrives as ONE argv item —
// `mahoquot-captcha-solver '{"scene":...,"region":...,"prefix":...,"timeoutMs":30000}'`
// — NOT stdin: tokio's ChildStdin drop does not deliver EOF to the child on
// macOS, which deadlocks any read-to-stdin sidecar. The result leaves as one
// JSON line on stdout:
//   {"ok":true,"param":"..."} | {"ok":false,"error":"..."}
//
// The fingerprint and polyfill values in captcha-solver.ts MUST stay
// deterministic across solves — Aliyun's risk engine flags per-solve
// randomization as F001.

import { solveTraceless } from "./captcha-solver";

interface SolveInput {
  scene?: string;
  region?: string;
  prefix?: string;
  timeoutMs?: number;
}

async function main(): Promise<void> {
  let input: SolveInput = {};
  try {
    const raw = Bun.argv[2] ?? "";
    if (raw.trim()) input = JSON.parse(raw) as SolveInput;
  } catch (err) {
    console.log(JSON.stringify({ ok: false, error: `invalid solve request: ${String(err)}` }));
    return;
  }
  try {
    const param = await solveTraceless({
      scene: input.scene,
      region: input.region,
      prefix: input.prefix,
      timeoutMs: input.timeoutMs ?? 30_000,
    });
    console.log(JSON.stringify({ ok: true, param }));
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    console.log(JSON.stringify({ ok: false, error: message }));
  }
}

await main();

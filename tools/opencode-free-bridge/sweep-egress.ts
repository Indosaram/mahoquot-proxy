// Sweeps every global-egress slot: exit IP + opencode free-tier verdict.
// Usage: bun sweep-egress.ts <fingerprint.json> <out.jsonl> [concurrency]
import { readFileSync, appendFileSync } from "node:fs";

const [fpPath, outPath, concText] = process.argv.slice(2);
const fp = JSON.parse(readFileSync(fpPath, "utf8"));
const concurrency = Number(concText ?? "3");
const control = process.env.EGRESS_CONTROL ?? "http://127.0.0.1:8099";
const proxyBase = process.env.EGRESS_PROXY ?? "127.0.0.1:3128";

const B62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
let n = 0;
const id = (p: string, desc: boolean) => {
  let v = BigInt(Date.now()) * 0x1000n + BigInt(++n & 0xfff);
  if (desc) v = ~v & 0xffffffffffffn;
  let t = "";
  for (let i = 0; i < 14; i++) t += B62[Math.floor(Math.random() * 62)];
  return `${p}_${v.toString(16).padStart(12, "0").slice(-12)}${t}`;
};
const body = JSON.stringify({
  model: "deepseek-v4.1-flash-free",
  stream: true,
  tools: fp.tools,
  messages: [
    { role: "system", content: `${fp.system_prompt}<env>\n  Working directory: /workspace\n  Workspace root folder: /\n  Is directory a git repo: no\n  Platform: linux\n  Today's date: ${new Date().toDateString()}\n</env>\n` },
    { role: "user", content: "Reply with exactly: pong" },
  ],
});

const slots: Array<{ id: string; city: string }> = (await (await fetch(`${control}/v1/slots`)).json()).slots;
console.error(`slots: ${slots.length}`);

async function probe(slot: { id: string; city: string }, attempt = 0): Promise<Record<string, unknown>> {
  const proxy = `http://${encodeURIComponent(`slot=${slot.id};sess=ocsweep-${slot.id}`)}:x@${proxyBase}`;
  const row: Record<string, unknown> = { slot: slot.id, city: slot.city };
  try {
    const ipRes = await fetch("https://api.ipify.org", { proxy, signal: AbortSignal.timeout(45_000) });
    const ipText = (await ipRes.text()).trim();
    if (!ipRes.ok) {
      if (attempt < 2) {
        await Bun.sleep(15_000);
        return probe(slot, attempt + 1);
      }
      row.result = `egress_${ipRes.status}`;
      row.detail = ipText.slice(0, 80);
      return row;
    }
    row.ip = ipText;
    const res = await fetch("https://opencode.ai/inference/openai/v1/chat/completions", {
      method: "POST",
      proxy,
      signal: AbortSignal.timeout(90_000),
      headers: {
        authorization: "Bearer public",
        "content-type": "application/json",
        "user-agent": fp.user_agent,
        "x-opencode-client": fp.client,
        "x-opencode-project": fp.project,
        "x-opencode-session": id("ses", true),
        "x-opencode-request": id("msg", false),
      },
      body,
    });
    const text = await res.text();
    row.status = res.status;
    row.retry_after = res.headers.get("retry-after");
    row.result = res.ok ? "ok" : text.includes("FreeUsageLimitError") ? "free_limit" : `http_${res.status}`;
    if (!res.ok && !text.includes("FreeUsageLimitError")) row.detail = text.slice(0, 120);
  } catch (err) {
    row.result = "error";
    row.detail = String(err).slice(0, 120);
  }
  return row;
}

const queue = [...slots];
await Promise.all(
  Array.from({ length: concurrency }, async () => {
    for (let slot = queue.shift(); slot; slot = queue.shift()) {
      const row = await probe(slot);
      appendFileSync(outPath, JSON.stringify(row) + "\n");
      console.error(JSON.stringify(row));
    }
  }),
);

// opencode.ai serves `*-free` models only to requests shaped like the opencode
// agent (stock system prompt + core tools). This records that shape from the
// real CLI so it tracks opencode releases instead of being copied by hand.
// Usage: bun extract-fingerprint.ts [--out fingerprint.json] [--model <id>]

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const args = process.argv.slice(2);
const flag = (name: string, fallback: string) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
};
const outPath = flag("--out", join(import.meta.dir, "fingerprint.json"));
const model = flag("--model", "deepseek-v4.1-flash-free");
const opencodeBin = flag("--opencode", "opencode");

const UPSTREAM = "https://opencode.ai/inference/openai";
const STABLE_PROMPT_END = "<env>";
const MCP_TOOL = /^[a-z0-9-]+_[a-z0-9_]+$/;

type Captured = { headers: Record<string, string>; body: string };
const captured: Captured[] = [];

const server = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(req) {
    const url = new URL(req.url);
    const body = await req.text();
    captured.push({ headers: Object.fromEntries(req.headers), body });
    const headers = new Headers(req.headers);
    headers.delete("host");
    headers.delete("accept-encoding");
    const upstream = await fetch(UPSTREAM + url.pathname.replace(/^\/cap/, "") + url.search, {
      method: req.method,
      headers,
      body: body || undefined,
    });
    return new Response(upstream.body, { status: upstream.status, headers: upstream.headers });
  },
});

const work = mkdtempSync(join(tmpdir(), "ocfp-"));
try {
  const proc = Bun.spawn([opencodeBin, "run", "-m", `opencode/${model}`, "reply with the word ok"], {
    cwd: work,
    env: {
      ...process.env,
      OPENCODE_CONFIG_CONTENT: JSON.stringify({
        provider: { opencode: { options: { baseURL: `http://127.0.0.1:${server.port}/cap/v1` } } },
      }),
    },
    stdout: "ignore",
    stderr: "inherit",
  });
  const timer = setTimeout(() => proc.kill(), 180_000);
  await proc.exited;
  clearTimeout(timer);

  const agent = captured.find((c) => c.body.includes('"tools"'));
  if (!agent) {
    throw new Error(`opencode sent no agent request (captured ${captured.length}); is opencode installed?`);
  }
  const body = JSON.parse(agent.body);
  const system: string = body.messages.find((m: { role: string }) => m.role === "system")?.content ?? "";
  const cut = system.indexOf(STABLE_PROMPT_END);
  if (cut < 0) throw new Error("system prompt has no <env> block; opencode prompt layout changed");

  const tools = (body.tools as Array<{ function: { name: string } }>).filter(
    (t) => !MCP_TOOL.test(t.function.name),
  );
  const toolsJson = JSON.stringify(tools).replaceAll(work, "/tmp/opencode").replaceAll(tmpdir(), "/tmp");

  const ua = agent.headers["user-agent"] ?? "";
  const fingerprint = {
    captured_at: new Date().toISOString(),
    opencode_version: ua.match(/^opencode\/([^ ]+)/)?.[1] ?? "unknown",
    user_agent: ua,
    client: agent.headers["x-opencode-client"] ?? "cli",
    project: agent.headers["x-opencode-project"] ?? "global",
    system_prompt: system.slice(0, cut),
    tools: JSON.parse(toolsJson),
  };
  writeFileSync(outPath, JSON.stringify(fingerprint, null, 2));
  console.log(
    `fingerprint written: ${outPath} (opencode ${fingerprint.opencode_version}, prompt ${fingerprint.system_prompt.length} chars, ${fingerprint.tools.length} tools)`,
  );
} finally {
  server.stop(true);
  rmSync(work, { recursive: true, force: true });
}

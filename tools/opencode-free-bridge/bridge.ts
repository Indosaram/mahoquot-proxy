// OpenAI-compatible bridge to opencode.ai free models (contract: README.md).
// The free tier only answers opencode-agent-shaped, Bun-TLS requests, and each
// exit IP has its own free quota, so requests are wrapped and rotated over exits.

import { readFileSync } from "node:fs";
import { connect } from "node:net";

type Exit = { name: string; proxy?: string; ip?: string; coolUntil: number; ok: number; limited: number; failStreak: number };
type Fingerprint = { user_agent: string; client: string; project: string; system_prompt: string; tools: unknown[] };
type ChatBody = {
  model: string;
  messages: Array<{ role: string; content: unknown; [k: string]: unknown }>;
  stream?: boolean;
  tools?: Array<{ function?: { name?: string } }>;
  tool_choice?: unknown;
  [k: string]: unknown;
};

const env = (key: string, fallback = "") => process.env[key] ?? fallback;
const LISTEN_HOST = env("BRIDGE_HOST", "127.0.0.1");
const LISTEN_PORT = Number(env("BRIDGE_PORT", "18891"));
const BRIDGE_KEY = env("BRIDGE_API_KEY");
const UPSTREAM = env("OPENCODE_UPSTREAM", "https://opencode.ai/inference/openai/v1");
const CATALOG_URL = env("OPENCODE_MODELS_URL", "https://models.dev/api.json");
const FINGERPRINT = JSON.parse(
  readFileSync(env("BRIDGE_FINGERPRINT", `${import.meta.dir}/fingerprint.json`), "utf8"),
) as Fingerprint;
const EXIT_SPECS = env("BRIDGE_EXITS", "direct");
const ENV_DIR = env("BRIDGE_ENV_DIR", "/workspace");
const LIMIT_FALLBACK_SECS = 300;
const MAX_COOL_SECS = 300;          // never bench an exit longer than 5 min
const REPROBE_INTERVAL_MS = 120_000; // re-probe cooled exits every 2 min

const log = (event: string, fields: Record<string, unknown> = {}) =>
  console.log(JSON.stringify({ ts: new Date().toISOString(), event, ...fields }));

function parseExits(spec: string): Exit[] {
  return spec
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean)
    .map((entry) => {
      const [name, proxy] = entry.includes("=") ? entry.split(/=(.*)/s, 2) : [entry, undefined];
      return { name, proxy: name === "direct" ? undefined : proxy, coolUntil: 0, ok: 0, limited: 0, failStreak: 0 };
    });
}
const exits = parseExits(EXIT_SPECS);
let cursor = 0;

const BASE62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
let idCounter = 0;
function opencodeId(prefix: "ses" | "msg", descending: boolean): string {
  let n = BigInt(Date.now()) * 0x1000n + BigInt(++idCounter & 0xfff);
  if (descending) n = ~n & 0xffffffffffffn;
  let tail = "";
  for (let i = 0; i < 14; i++) tail += BASE62[Math.floor(Math.random() * 62)];
  return `${prefix}_${n.toString(16).padStart(12, "0").slice(-12)}${tail}`;
}

function nextExit(now: number): Exit | undefined {
  for (let i = 0; i < exits.length; i++) {
    const exit = exits[(cursor + i) % exits.length];
    if (exit.coolUntil <= now) {
      cursor = (cursor + i + 1) % exits.length;
      return exit;
    }
  }
  return undefined;
}

async function probeIp(exit: Exit) {
  try {
    const res = await fetch("https://api.ipify.org", { proxy: exit.proxy, signal: AbortSignal.timeout(20_000) });
    exit.ip = (await res.text()).trim();
  } catch (err) {
    exit.ip = undefined;
    log("exit_probe_failed", { exit: exit.name, error: String(err) });
  }
}

function systemPrompt(): string {
  const env = `<env>\n  Working directory: ${ENV_DIR}\n  Workspace root folder: /\n  Is directory a git repo: no\n  Platform: linux\n  Today's date: ${new Date().toDateString()}\n</env>\n`;
  return FINGERPRINT.system_prompt + env;
}

function wrap(body: ChatBody, maxEffort: string | undefined): ChatBody {
  const clientTools = body.tools ?? [];
  const clientNames = new Set(clientTools.map((t) => t.function?.name));
  const agentTools = (FINGERPRINT.tools as Array<{ function: { name: string } }>).filter(
    (t) => !clientNames.has(t.function.name),
  );
  const hasClientTools = clientTools.length > 0;
  const { reasoning_effort: _clientEffort, reasoning: _clientReasoning, ...rest } = body;
  return {
    ...rest,
    ...(maxEffort ? { reasoning_effort: maxEffort } : {}),
    stream: true,
    stream_options: { include_usage: true },
    messages: [{ role: "system", content: systemPrompt() }, ...body.messages],
    tools: [...agentTools, ...clientTools],
    tool_choice: hasClientTools ? (body.tool_choice ?? "auto") : "none",
  };
}

function upstreamHeaders(): Record<string, string> {
  return {
    authorization: "Bearer public",
    "content-type": "application/json",
    "user-agent": FINGERPRINT.user_agent,
    "x-opencode-client": FINGERPRINT.client,
    "x-opencode-project": FINGERPRINT.project,
    "x-opencode-session": opencodeId("ses", true),
    "x-opencode-request": opencodeId("msg", false),
  };
}

const json = (status: number, payload: unknown, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(payload), { status, headers: { "content-type": "application/json", ...headers } });
const openaiError = (status: number, message: string, type = "upstream_error", extra: Record<string, string> = {}) =>
  json(status, { error: { message, type } }, extra);

function proxyReachable(proxy: string | undefined): Promise<boolean> {
  if (!proxy) return Promise.resolve(true);
  const { hostname, port } = new URL(proxy);
  return new Promise((resolve) => {
    const socket = connect({ host: hostname, port: Number(port) || 80, timeout: 5_000 });
    const done = (ok: boolean) => {
      socket.destroy();
      resolve(ok);
    };
    socket.once("connect", () => done(true));
    socket.once("timeout", () => done(false));
    socket.once("error", () => done(false));
  });
}

function benchUnreachable(exit: Exit, now: number) {
  exit.failStreak++;
  exit.coolUntil = now + Math.min(60_000 * 2 ** (exit.failStreak - 1), 30 * 60_000);
}

async function sendUpstream(payload: string, model: string): Promise<{ res: Response; exit: Exit } | Response> {
  let limitedUntil = Infinity;
  let lastError = "no exit available";
  for (let attempt = 0; attempt < exits.length; attempt++) {
    const now = Date.now();
    const exit = nextExit(now);
    if (!exit) break;
    if (!(await proxyReachable(exit.proxy))) {
      benchUnreachable(exit, now);
      lastError = `exit ${exit.name} unreachable`;
      log("exit_unreachable", { exit: exit.name, cool_for_s: Math.round((exit.coolUntil - now) / 1000) });
      continue;
    }
    let res: Response;
    try {
      res = await fetch(`${UPSTREAM}/chat/completions`, {
        method: "POST",
        headers: upstreamHeaders(),
        body: payload,
        proxy: exit.proxy,
        signal: AbortSignal.timeout(300_000),
      });
    } catch (err) {
      lastError = String(err);
      benchUnreachable(exit, now);
      log("upstream_transport_error", { exit: exit.name, ip: exit.ip, model, error: lastError });
      continue;
    }
    if (res.status === 429) {
      const text = await res.text();
      const rawRetry = Number(res.headers.get("retry-after")) || LIMIT_FALLBACK_SECS;
      const retryAfter = Math.min(rawRetry, MAX_COOL_SECS);
      exit.coolUntil = now + retryAfter * 1000;
      exit.limited++;
      limitedUntil = Math.min(limitedUntil, exit.coolUntil);
      lastError = text.slice(0, 300);
      log("exit_limited", { exit: exit.name, ip: exit.ip, model, retry_after_s: retryAfter, free_limit: text.includes("FreeUsageLimitError") });
      continue;
    }
    if (res.status >= 500) {
      lastError = `upstream ${res.status}: ${(await res.text()).slice(0, 300)}`;
      log("upstream_5xx", { exit: exit.name, ip: exit.ip, model, status: res.status });
      continue;
    }
    return { res, exit };
  }
  const soonest = Math.min(limitedUntil, ...exits.map((e) => e.coolUntil).filter((t) => t > Date.now()));
  const retryAfter = Number.isFinite(soonest) ? Math.max(1, Math.ceil((soonest - Date.now()) / 1000)) : 60;
  return openaiError(429, `all opencode exits are rate limited: ${lastError}`, "rate_limit_error", {
    "retry-after": String(retryAfter),
  });
}

async function collectStream(res: Response, model: string) {
  const text = await res.text();
  let id = "";
  let created = Math.floor(Date.now() / 1000);
  let content = "";
  let reasoning = "";
  let finish: string | null = null;
  let usage: unknown = undefined;
  const toolCalls: Array<{ id?: string; type: string; function: { name: string; arguments: string } }> = [];
  for (const line of text.split("\n")) {
    if (!line.startsWith("data: ") || line === "data: [DONE]") continue;
    let chunk: any;
    try {
      chunk = JSON.parse(line.slice(6));
    } catch {
      continue;
    }
    id ||= chunk.id ?? "";
    created = chunk.created ?? created;
    if (chunk.usage) usage = chunk.usage;
    for (const choice of chunk.choices ?? []) {
      const delta = choice.delta ?? {};
      if (typeof delta.content === "string") content += delta.content;
      if (typeof delta.reasoning_content === "string") reasoning += delta.reasoning_content;
      for (const call of delta.tool_calls ?? []) {
        const slot = (toolCalls[call.index ?? toolCalls.length] ??= {
          id: call.id,
          type: "function",
          function: { name: "", arguments: "" },
        });
        if (call.id) slot.id = call.id;
        if (call.function?.name) slot.function.name += call.function.name;
        if (call.function?.arguments) slot.function.arguments += call.function.arguments;
      }
      if (choice.finish_reason) finish = choice.finish_reason;
    }
  }
  const message: Record<string, unknown> = { role: "assistant", content };
  if (reasoning) message.reasoning_content = reasoning;
  if (toolCalls.length) message.tool_calls = toolCalls;
  return {
    id: id || `chatcmpl-${crypto.randomUUID()}`,
    object: "chat.completion",
    created,
    model,
    choices: [{ index: 0, message, finish_reason: finish ?? "stop" }],
    ...(usage ? { usage } : {}),
  };
}

type CatalogModel = { id: string; name?: string; limit?: unknown; maxEffort?: string };
let catalogCache: { at: number; models: CatalogModel[] } | undefined;
async function freeModels() {
  if (catalogCache && Date.now() - catalogCache.at < 3600_000) return catalogCache.models;
  const res = await fetch(CATALOG_URL, { signal: AbortSignal.timeout(20_000) });
  if (!res.ok) throw new Error(`catalog ${res.status}`);
  const catalog = (await res.json()) as Record<string, { models?: Record<string, any> }>;
  const models = Object.values(catalog.opencode?.models ?? {})
    .filter((m) => m?.cost?.input === 0 && m?.cost?.output === 0 && m?.provider?.npm !== "@ai-sdk/anthropic")
    .map((m) => {
      const efforts: string[] =
        (m.reasoning_options as Array<{ type: string; values?: string[] }> | undefined)?.find((o) => o.type === "effort")
          ?.values ?? [];
      return { id: m.id as string, name: m.name, limit: m.limit, maxEffort: efforts.at(-1) };
    });
  catalogCache = { at: Date.now(), models };
  return models;
}

function authorized(req: Request): boolean {
  if (!BRIDGE_KEY) return true;
  const header = req.headers.get("authorization") ?? "";
  return header === `Bearer ${BRIDGE_KEY}`;
}

async function chat(req: Request): Promise<Response> {
  let body: ChatBody;
  try {
    body = (await req.json()) as ChatBody;
  } catch {
    return openaiError(400, "request body is not valid JSON", "invalid_request_error");
  }
  if (typeof body.model !== "string" || !Array.isArray(body.messages)) {
    return openaiError(400, "model and messages are required", "invalid_request_error");
  }
  const model = body.model.replace(/^opencode\//, "");
  const clientStream = body.stream === true;
  const maxEffort = await freeModels()
    .then((models) => models.find((m) => m.id === model)?.maxEffort)
    .catch(() => undefined);
  const outcome = await sendUpstream(JSON.stringify(wrap({ ...body, model }, maxEffort)), model);
  if (outcome instanceof Response) return outcome;
  const { res, exit } = outcome;
  if (!res.ok) {
    const text = await res.text();
    log("upstream_error", { exit: exit.name, ip: exit.ip, model, status: res.status, body: text.slice(0, 300) });
    return new Response(text, { status: res.status, headers: { "content-type": "application/json" } });
  }
  exit.ok++;
  exit.failStreak = 0;
  log("upstream_ok", { exit: exit.name, ip: exit.ip, model, stream: clientStream });
  if (clientStream) {
    return new Response(res.body, {
      status: 200,
      headers: { "content-type": "text/event-stream", "cache-control": "no-cache", "x-bridge-exit": exit.name },
    });
  }
  return json(200, await collectStream(res, model), { "x-bridge-exit": exit.name });
}

const server = Bun.serve({
  hostname: LISTEN_HOST,
  port: LISTEN_PORT,
  idleTimeout: 255,
  async fetch(req) {
    const { pathname } = new URL(req.url);
    if (pathname === "/healthz") {
      return json(200, {
        exits: exits.map(({ name, ip, coolUntil, ok, limited }) => ({
          name,
          ip,
          ok,
          limited,
          cooling_for_s: Math.max(0, Math.ceil((coolUntil - Date.now()) / 1000)),
        })),
      });
    }
    if (!authorized(req)) return openaiError(401, "invalid bridge api key", "authentication_error");
    if (req.method === "GET" && pathname === "/v1/models") {
      try {
        const models = await freeModels();
        return json(200, {
          object: "list",
          data: models.map((m) => ({ id: m.id, object: "model", created: 0, owned_by: "opencode" })),
        });
      } catch (err) {
        return openaiError(502, `model catalog unavailable: ${err}`);
      }
    }
    if (req.method === "POST" && pathname === "/v1/chat/completions") return chat(req);
    return openaiError(404, `no route for ${req.method} ${pathname}`, "invalid_request_error");
  },
});

await Promise.all(exits.map(probeIp));
log("bridge_started", {
  listen: `${server.hostname}:${server.port}`,
  exits: exits.map((e) => ({ name: e.name, ip: e.ip })),
  fingerprint_tools: FINGERPRINT.tools.length,
});

// Background re-probe: test cooled exits with a lightweight upstream call and
// unblock them early when the IP quota has actually recovered.
async function reprobeExit(exit: Exit): Promise<boolean> {
  if (!(await proxyReachable(exit.proxy))) return false;
  const body = JSON.stringify(wrap(
    { model: "deepseek-v4.1-flash-free", messages: [{ role: "user", content: "hi" }] },
    "max",
  ));
  try {
    const res = await fetch(`${UPSTREAM}/chat/completions`, {
      method: "POST",
      headers: upstreamHeaders(),
      body,
      proxy: exit.proxy,
      signal: AbortSignal.timeout(30_000),
    });
    if (res.ok) {
      // drain body so connection is released
      await res.text();
      return true;
    }
    await res.text();
    return false;
  } catch {
    return false;
  }
}

setInterval(async () => {
  const now = Date.now();
  const cooled = exits.filter((e) => e.coolUntil > now);
  if (cooled.length === 0) return;
  for (const exit of cooled) {
    const ok = await reprobeExit(exit);
    if (ok) {
      log("exit_reprobe_recovered", { exit: exit.name, ip: exit.ip, was_cool_for_s: Math.round((exit.coolUntil - now) / 1000) });
      exit.coolUntil = 0;
      exit.failStreak = 0;
      exit.limited = 0;
    } else {
      log("exit_reprobe_still_limited", { exit: exit.name, ip: exit.ip, cool_for_s: Math.round((exit.coolUntil - now) / 1000) });
    }
  }
}, REPROBE_INTERVAL_MS);

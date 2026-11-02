# opencode-free-bridge

OpenAI-compatible sidecar that lets mahoquot-gateway serve opencode.ai's free
models (`*-free`, e.g. `deepseek-v4.1-flash-free`, `glm-5.3-flash-free`).

## Upstream contract (opencode 1.18.18, observed 2026-11-01)

`POST https://opencode.ai/inference/openai/v1/chat/completions` with bearer
`public` answers free models only when every condition holds; otherwise it
returns `429 FreeUsageLimitError`:

- opencode-format ids in `x-opencode-session` (`ses_` + descending time hex +
  base62) and `x-opencode-request` (`msg_` + ascending), plus the opencode
  `User-Agent`, `x-opencode-client`, `x-opencode-project` headers;
- the stock opencode agent system prompt followed by an `<env>` block, and the
  opencode core tool set (extra tools and extra system messages are tolerated);
- `stream: true`;
- Bun's TLS client (curl, Node undici and Python urllib are rejected with the
  same headers and body);
- an exit IP with free quota left. The quota is per public IP; NordVPN exits
  were already exhausted.

`extract-fingerprint.ts` captures the prompt and tools from the installed
`opencode` CLI, so re-run it after opencode upgrades.

## Run

```bash
bun install
bun extract-fingerprint.ts          # writes fingerprint.json
BRIDGE_API_KEY=... BRIDGE_EXITS='direct,mac=http://100.78.73.127:18890' bun bridge.ts
bun exit-node.ts --listen <tailnet-ip>:18890   # on each extra exit host
```

`BRIDGE_EXITS` is a comma list of `name=proxyURL` (`direct` uses the local IP).
Requests rotate round-robin across the exits. An exit that hits the free limit
is benched until its `Retry-After`. `GET /healthz` shows per-exit IP, success
count and cooldown. Models come from models.dev (`opencode` provider, zero
cost).

## Deployment (omarchy)

- `~/.config/systemd/user/opencode-free-bridge.service` on `127.0.0.1:18891`,
  env in `~/.config/opencode-free-bridge.env`
- gateway credential `~/.mahoquot/auth/generic-opencode-free.json`
  (`provider: opencode-free`, `adapter: openai-chat`)
- exits (in order): four global-egress sessions
  (`any=1;sess=ocb-a..d` on omarchy `127.0.0.1:3128`), omarchy direct, Mac
- Mac exit: LaunchAgent `ai.indo.opencode-exit-node` on `100.78.73.127:18890`

# Fair-Work Benchmark: mahoquot-gateway vs CLIProxyAPI (both translating OpenAI <-> Codex)

Rounds kept: 6 (round 0 discarded as warmup) · tier order randomized per round · paired within-round comparison · mock TTFT floor 40ms

## Equivalent Work Contract
- Upstream mock speaks the **Codex Responses** SSE protocol for every tier.
- Tier B: CLIProxyAPI `codex-api-key` provider with `base-url` pointed at the mock; it translates OpenAI -> Codex and Codex SSE -> `chat.completion.chunk`.
- Tier C: mahoquot-gateway `/v1/chat/completions` compat path; it performs the same two-way translation.
- Tier A: raw client straight to the mock Codex endpoint (protocol floor, no translation).
- Tier C production features ON: `API_KEYS=benchkey`, `AUTH_REFRESH=true`, metrics, 4-account pool with failover.

## Absolute Medians per Load Point (median of 6 kept rounds)

| Load Point | Tier | p50 (ms) | p99 (ms) | RPS | errors |
|---|---|---|---|---|---|
| 20 deltas @500 conc | A direct mock (floor) | 42.31 | 95.10 | 8764 | 0 |
| 20 deltas @500 conc | B CLIProxyAPI | 52.26 | 131.67 | 6717 | 0 |
| 20 deltas @500 conc | C mahoquot-gateway (authed) | 43.12 | 100.78 | 8492 | 0 |
| 20 deltas @500 conc | C mahoquot-gateway (no auth) | 43.54 | 104.72 | 8377 | 0 |
| 200 deltas @500 conc | A direct mock (floor) | 42.96 | 103.90 | 8347 | 0 |
| 200 deltas @500 conc | B CLIProxyAPI | 116.56 | 726.78 | 975 | 0 |
| 200 deltas @500 conc | C mahoquot-gateway (authed) | 51.15 | 125.17 | 5507 | 0 |
| 200 deltas @500 conc | C mahoquot-gateway (no auth) | 48.93 | 115.13 | 5844 | 0 |

## Paired Within-Round Deltas (median [min, max] and sign consistency)

| Load Point | Comparison | p50 delta (ms) | p99 delta (ms) | RPS delta | Sign consistency (p50 / p99 / RPS) |
|---|---|---|---|---|---|
| 20 deltas @500 conc | C - B | -9.16 [-22.51, -6.29] | -31.27 [-75.92, +17.74] | +1591.41 [+545.59, +2899.49] | p50 6/6 negative / p99 5/6 negative / RPS 6/6 positive |
| 20 deltas @500 conc | C - A | +1.02 [+0.38, +1.86] | +5.58 [-28.56, +41.35] | -213.41 [-961.23, +187.14] | p50 6/6 positive / p99 4/6 positive / RPS 3/6 negative |
| 200 deltas @500 conc | C - B | -64.99 [-74.39, -50.49] | -614.58 [-1210.11, -407.47] | +4630.28 [+4087.02, +5457.73] | p50 6/6 negative / p99 6/6 negative / RPS 6/6 positive |
| 200 deltas @500 conc | C - A | +8.18 [+3.83, +11.73] | +31.42 [-38.02, +39.24] | -2867.38 [-3291.53, -2714.18] | p50 6/6 positive / p99 5/6 positive / RPS 6/6 negative |

## Feature Cost: Authentication & Middleware Overhead

Quantification of Axum inbound authentication middleware + metrics recording overhead by comparing authenticated Tier C (`API_KEYS=benchkey`) vs unauthenticated Tier C (`API_KEYS` unset) in paired within-round runs:

| Load Point | Comparison | p50 overhead (ms) | p99 overhead (ms) | RPS delta | Sign consistency (p50 / p99 / RPS) |
|---|---|---|---|---|---|
| 20 deltas @500 conc | C (authed) - C (no auth) | -0.45 [-0.90, +0.59] | -5.36 [-15.12, +15.63] | -53.26 [-307.79, +417.24] | p50 4/6 negative / p99 4/6 negative / RPS 3/6 negative |
| 200 deltas @500 conc | C (authed) - C (no auth) | +0.43 [-1.46, +7.40] | +12.41 [-38.67, +23.79] | -267.03 [-612.24, +97.21] | p50 4/6 positive / p99 5/6 positive / RPS 5/6 negative |

## Findings & Analysis

1. **20 Chunks @ 500 Concurrency**: mahoquot-gateway achieves a median p50 of 43.12 ms vs CLIProxyAPI 52.26 ms (p50 delta: -9.16 [-22.51, -6.29] ms, faster in 6/6 rounds). On p99 TTFT, mahoquot-gateway achieves 100.78 ms vs CLIProxyAPI 131.67 ms (p99 delta: -31.27 [-75.92, +17.74] ms, faster in 5/6 rounds). Throughput is 8492 RPS vs 6717 RPS.
2. **200 Chunks @ 500 Concurrency**: Under extended streaming payloads, mahoquot-gateway translates at (51.15 ms p50, 125.17 ms p99, 5507 RPS), while CLIProxyAPI does the same translation at (116.56 ms p50, 726.78 ms p99, 975 RPS). Deltas: p50 -64.99 [-74.39, -50.49] ms (faster in 6/6 rounds), p99 -614.58 [-1210.11, -407.47] ms (faster in 6/6 rounds).
3. **Feature Overhead**: Enabling inbound token authentication middleware and metrics incurs an overhead of -0.45 ms p50 and -5.36 ms p99 at 20 deltas.

## Verdict

VERDICT: KEEP mahoquot-rs

## Cleanup Receipt

- listeners left on bench ports (18860 18861 18862 18863 18864 18865 18866 18867): 0
- live Quotio config md5 before / after: `68bff0409d1b420b4b3a30ebfdda1571` / `68bff0409d1b420b4b3a30ebfdda1571`
- temp workdir /tmp/qbench_fair removed: yes
- CLIProxyAPI binary under test: /Users/indo/Library/Application Support/Quotio/proxy/upstream/current/CLIProxyAPI

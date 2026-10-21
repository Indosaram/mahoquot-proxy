import { createHash, randomUUID } from 'node:crypto';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
export const canonicalSha = '3de4e248f6bb12fc7dfafb183404f3b248578922';
export const modes = ['none', 'request', 'worker', 'shared'];
export function balancedOrder(round, seed = 35) {
  const tiers = ['B', 'C'];
  return (round + seed) % 2 ? tiers.reverse() : tiers;
}
export function validateRounds(runs, rounds, series) {
  if (!Number.isInteger(rounds) || rounds < 2 || rounds % 2) throw new Error('ROUNDS must be positive even and >=2 for position balance');
  for (const key of series) for (let round = 1; round <= rounds; round++) for (const tier of ['B', 'C']) {
    const report = runs[`${key}/${round}/${tier}`];
    if (!report) throw new Error(`missing round ${key}/${round}/${tier}`);
    if (!report.correctness_passed || report.failed !== 0 || report.successful !== report.total || report.semantic_samples !== report.total) {
      throw new Error(`incorrect round ${key}/${round}/${tier}`);
    }
    for (const metric of ['p50', 'p99']) if (!Number.isFinite(report.ttft_ms?.[metric])) throw new Error('missing TTFT');
    for (const metric of ['ttfb_ms', 'completion_ms']) if (!Array.isArray(report[metric]) || report[metric].length !== report.total) throw new Error(`missing ${metric}`);
  }
  const threshold = rounds <= 2 ? rounds : rounds - 1;
  const paired = Object.fromEntries(series.map(key => [key, Object.fromEntries(['p50', 'p99'].map(metric => {
    const deltas = Array.from({ length: rounds }, (_, i) => runs[`${key}/${i + 1}/C`].ttft_ms[metric] - runs[`${key}/${i + 1}/B`].ttft_ms[metric]);
    return [metric, { deltas, wins: deltas.filter(d => d < 0).length }];
  }))]));
  return { rounds, threshold, paired, verdict: Object.values(paired).every(p => p.p50.wins >= threshold && p.p99.wins >= threshold) ? 'KEEP mahoquot-rs' : 'NO demonstrated advantage' };
}
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
async function command(argv, options = {}) {
  const child = Bun.spawn(argv, { cwd: root, stdout: 'pipe', stderr: 'pipe', ...options });
  const output = Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text()]);
  const code = await child.exited;
  const [stdout, stderr] = await output;
  if (code !== 0) throw new Error(`${argv.join(' ')} exit ${code}: ${stderr}`);
  return stdout.trim();
}
async function launch(argv, env, readyMatch, owned) {
  const child = Bun.spawn(argv, { cwd: root, env, stdout: 'pipe', stderr: 'pipe' });
  owned.push(child);
  let ready;
  let failed;
  const readiness = new Promise((yes, no) => { ready = yes; failed = no; });
  const logs = [];
  const consume = async stream => {
    const reader = stream.getReader();
    const decoder = new TextDecoder();
    let pending = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      pending += decoder.decode(value, { stream: true });
      const lines = pending.split('\n'); pending = lines.pop();
      for (const line of lines) { logs.push(line); if (readyMatch.test(line)) ready(); }
    }
  };
  const drains = Promise.all([consume(child.stdout), consume(child.stderr)]);
  child.exited.then(code => failed(new Error(`server exited before ready: ${code}`)));
  let timer;
  try { await Promise.race([readiness, new Promise((_, no) => { timer = setTimeout(() => no(new Error('server readiness timeout')), 20000); })]); }
  finally { clearTimeout(timer); }
  return { child, logs, drains };
}
export async function runFairTranslation(env = process.env) {
  const rounds = Number(env.ROUNDS ?? 6), seed = Number(env.SEED ?? 35);
  validateRounds({}, rounds, []);
  if (!Number.isInteger(seed)) throw new Error('SEED must be integer');
  const canonical = resolve(env.CANONICAL_DIR ?? '/Volumes/T9-Mac/project/mahoquot/tmp/cliproxyapi-audit-20261011');
  const cp = env.CPBIN;
  const bench = env.BENCH ?? join(root, 'target/release/bench');
  const gateway = env.GWBIN ?? join(root, 'target/release/mahoquot-gateway');
  if (!cp || !env.CP_BUILD_PROFILE || !env.GW_BUILD_PROFILE) throw new Error('CPBIN, CP_BUILD_PROFILE, GW_BUILD_PROFILE required; installed unknown binaries forbidden');
  if (cp.startsWith('/Applications/')) throw new Error('installed binary forbidden');
  if (await command(['git', '-C', canonical, 'rev-parse', 'HEAD']) !== canonicalSha) throw new Error('canonical checkout SHA mismatch');
  if (await command(['git', '-C', canonical, 'status', '--porcelain'])) throw new Error('canonical checkout must be clean');
  const goBuild = await command(['go', 'version', '-m', cp]);
  if (!goBuild.includes(`vcs.revision=${canonicalSha}`) || !goBuild.includes('vcs.modified=false') || !goBuild.includes('github.com/router-for-me/CLIProxyAPI')) throw new Error('canonical binary provenance mismatch');
  const destination = resolve(env.BENCH_EVIDENCE ?? join(root, '../.omo/evidence', `bench-core-${randomUUID()}`));
  if (!destination.startsWith(resolve(root, '../.omo/evidence') + '/')) throw new Error('BENCH_EVIDENCE must be under workspace .omo/evidence');
  await mkdir(destination, { recursive: true });
  const runtimeEnv = { ...env, HTTP_PROXY: '', HTTPS_PROXY: '', ALL_PROXY: '', http_proxy: '', https_proxy: '', all_proxy: '', NO_PROXY: '*', no_proxy: '*', MAHOQUOT_QA_LOOPBACK_ONLY: '1' };
  const gatewayBinding = env.GW_SOURCE_BINDING;
  if (!gatewayBinding) throw new Error('GW_SOURCE_BINDING required: SHA, binary SHA256, source closure SHA256, build command');
  const binding = (await readFile(gatewayBinding, 'utf8')).trimEnd().split('\n');
  const revision = await command(['git', 'rev-parse', 'HEAD']);
  if (binding.length !== 4 || binding[0] !== revision || binding[1] !== hash(await readFile(gateway)) || !/^[a-f0-9]{64}$/.test(binding[2]) || !binding[3].includes('cargo build')) throw new Error('gateway source binding mismatch');
  const { getSourceClosure } = await import('../../../../scripts/qa/cliproxy-audit.mjs');
  const closure = await getSourceClosure(root);
  if (closure.sourceHash !== binding[2]) throw new Error('gateway binary source closure stale');
  const provenance = {
    gateway_sha: await command(['git', 'rev-parse', 'HEAD']), gateway_dirty: await command(['git', 'status', '--porcelain']),
    gateway_diff_sha256: hash(await command(['git', 'diff', 'HEAD'])), canonical_sha: canonicalSha,
    binary_sha256: { gateway: hash(await readFile(gateway)), canonical: hash(await readFile(cp)), bench: hash(await readFile(bench)) },
    toolchain: { rust: await command(['rustc', '-Vv']), cargo: await command(['cargo', '-V']), go: await command(['go', 'version']), canonical_build: goBuild, bun: Bun.version },
    profiles: { gateway: env.GW_BUILD_PROFILE, canonical: env.CP_BUILD_PROFILE }, seed, rounds,
    gateway_source_binding: binding, gateway_source_sha256: closure.sourceHash,
    features: { retry: 0, auth: true, strategy: 'fill-first', accounts: 1, usage_statistics: false, session_modes: modes },
    config_sha256: {},
  };
  const owned = [], servers = [], runs = {}, series = [];
  const total = Number(env.TOTAL ?? 2000), concurrency = Number(env.CONCURRENCY ?? 500);
  if (!Number.isInteger(total) || total < 1 || !Number.isInteger(concurrency) || concurrency < 1) throw new Error('invalid load');
  try {
    for (const chunks of [20, 200]) {
      const mockPort = chunks === 20 ? 18860 : 18861;
      const cpPort = chunks === 20 ? 18862 : 18863;
      const gwPort = chunks === 20 ? 18864 : 18865;
      for (const port of [mockPort, cpPort, gwPort]) {
        const probe = Bun.listen({ hostname: '127.0.0.1', port, socket: { data() {} } }); probe.stop(true);
      }
      const dir = join(destination, String(chunks));
      await mkdir(join(dir, 'gw-auth'), { recursive: true }); await mkdir(join(dir, 'cp-auth'));
      const override = `http://127.0.0.1:${mockPort}`;
      servers.push(await launch([bench, 'mock', '--port', String(mockPort), '--protocol', 'codex', '--chunks', String(chunks), '--ttft-ms', '40'], runtimeEnv, /bench_mock_ready/, owned));
      const catalogResponse = await fetch(`${override}/v1/models`, { redirect: 'error', signal: AbortSignal.timeout(5000), proxy: null });
      if (!catalogResponse.ok) throw new Error('mock catalog unavailable');
      const catalog = await catalogResponse.json();
      const model = catalog.data?.[0]?.id;
      if (typeof model !== 'string' || !/^[a-zA-Z0-9._-]+$/.test(model)) throw new Error('invalid remote fixture model');
      const auth = { access_token: 'dummy', account_id: 'bench', refresh_token: 'dummy', type: 'plus', expired: '2099-01-01T00:00:00Z', upstream_override: override };
      const gwConfig = 'routing:\n  strategy: fill-first\nrequest-retry: 0\nmax-retry-credentials: 1\nusage-statistics-enabled: false\napi-keys:\n  - benchkey\n';
      const cpConfig = `host: 127.0.0.1\nport: ${cpPort}\nauth-dir: ${join(dir, 'cp-auth')}\nrouting:\n  strategy: fill-first\nusage-statistics-enabled: false\nrequest-retry: 0\napi-keys:\n  - benchkey\ncodex-api-key:\n  - api-key: dummy\n    base-url: ${override}\n    models:\n      - name: ${model}\n        alias: ${model}\n`;
      await writeFile(join(dir, 'gw-auth/codex-bench-plus.json'), JSON.stringify(auth));
      await writeFile(join(dir, 'gw-auth/config.yaml'), gwConfig);
      await writeFile(join(dir, 'cp.yaml'), cpConfig);
      provenance.config_sha256[chunks] = { gateway: hash(gwConfig), canonical: hash(cpConfig), credentials: hash(JSON.stringify(auth)) };
      servers.push(await launch([cp, '--local-model', '--config', join(dir, 'cp.yaml')], runtimeEnv, /API server started|server started successfully/i, owned));
      servers.push(await launch([gateway, 'serve', '--bind', '127.0.0.1', '--port', String(gwPort), '--auth-dir', join(dir, 'gw-auth'), '--auth-refresh', 'false', '--log-level', 'info'], { ...runtimeEnv, API_KEYS: 'benchkey', USAGE_POLL_SECS: '3600', MAX_FAILOVER: '1' }, /listening/, owned));
      for (const mode of modes) {
        const key = `${chunks}/${mode}`; series.push(key);
        for (let round = -1; round <= rounds; round++) {
          const order = balancedOrder(round, seed);
          for (const tier of order) {
            const filename = join(dir, `${mode}-${round}-${tier}.json`);
            const body = { model, messages: [{ role: 'user', content: 'bench' }], stream: true, stream_options: { include_usage: true }, bench_expect: { text: 'x'.repeat(chunks), input_tokens: 16, output_tokens: chunks } };
            await command([bench, 'run', '--url', `http://127.0.0.1:${tier === 'B' ? cpPort : gwPort}/v1/chat/completions`, '--concurrency', String(round === -1 ? 1 : concurrency), '--total', String(round === -1 ? 1 : total), '--body-json', JSON.stringify(body), '-H', 'Authorization: Bearer benchkey', '--out', filename], { env: { ...runtimeEnv, BENCH_SESSION_MODE: mode, BENCH_SESSION_PREFIX: `bench-${seed}-${chunks}-${mode}-${round}` } });
            const report = JSON.parse(await readFile(filename, 'utf8'));
            if (!report.correctness_passed) throw new Error('preflight correctness failed');
            if (round > 0) runs[`${key}/${round}/${tier}`] = report;
          }
        }
      }
    }
    const report = { provenance, ...validateRounds(runs, rounds, series), runs };
    await writeFile(join(destination, 'paired.json'), JSON.stringify(report, null, 2));
    return report;
  } finally {
    for (const child of owned) if (child.exitCode === null) child.kill('SIGTERM');
    await Promise.all(owned.map(async child => {
      let timer;
      try { await Promise.race([child.exited, new Promise(resolveExit => { timer = setTimeout(() => { child.kill('SIGKILL'); resolveExit(); }, 2000); })]); await child.exited; }
      finally { clearTimeout(timer); }
    }));
    await Promise.all(servers.map(async (server, i) => { await server.drains; await writeFile(join(destination, `server-${i}.log`), server.logs.join('\n')); }));
    for (const port of [18860,18861,18862,18863,18864,18865]) { const probe = Bun.listen({ hostname: '127.0.0.1', port, socket: { data() {} } }); probe.stop(true); }
  }
}
if (import.meta.main) runFairTranslation().then(r => console.log(r.verdict)).catch(error => { console.error(error); process.exitCode = 1; });

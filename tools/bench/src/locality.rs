use crate::mock::{fixture_frames, MockProtocol};
use crate::runner::{validate_bench_url, ExpectedOutput, SemanticGate};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalityBenchmarkReport {
    pub matrix: String,
    pub b3_history: B3HistoryResult,
    pub b4_alternating: B4AlternatingResult,
    pub b5_hierarchy: B5HierarchyResult,
    pub b8_egress: B8EgressResult,
    pub b9_antigravity: B9AntigravityResult,
    pub correctness_passed: bool,
    pub errors: BTreeMap<String, usize>,
}

pub async fn run_locality_benchmarks(
    base_url: &str,
    session: &str,
    turns: usize,
) -> Result<LocalityBenchmarkReport, String> {
    let b3_history = run_b3_benchmark(base_url, session, turns).await?;
    let b4_alternating = run_b4_benchmark(base_url, session, 4).await?;
    let b5_hierarchy = run_b5_benchmark(base_url, session, 3).await?;
    let b8_egress = run_b8_benchmark(base_url, 10).await?;
    let b9_antigravity = run_b9_benchmark(base_url, true, 2).await?;
    let mut errors = BTreeMap::new();
    if b3_history.raw_routing_headers_leaked > 0 {
        errors.insert(
            "routing_headers_leaked".into(),
            b3_history.raw_routing_headers_leaked,
        );
    }
    if b5_hierarchy.cross_tenant_collisions > 0 {
        errors.insert(
            "cross_tenant_collisions".into(),
            b5_hierarchy.cross_tenant_collisions,
        );
    }
    if b8_egress.direct_fallback_count > 0 {
        errors.insert("direct_fallbacks".into(), b8_egress.direct_fallback_count);
    }
    if b9_antigravity.cross_account_leaks > 0 {
        errors.insert(
            "cross_account_leaks".into(),
            b9_antigravity.cross_account_leaks,
        );
    }
    let correctness_passed = errors.is_empty()
        && b3_history.hops == turns
        && b3_history.signature_recall_success
        && b4_alternating.failover_recovery_ok
        && b5_hierarchy.parent_policy_honored
        && b8_egress.inflight_intact
        && b8_egress.next_request_correct_egress
        && b9_antigravity.peak_rss_bytes > 0
        && b9_antigravity.peak_fds > 0;
    Ok(LocalityBenchmarkReport {
        matrix: "B3-B5,B8-B9".into(),
        b3_history,
        b4_alternating,
        b5_hierarchy,
        b8_egress,
        b9_antigravity,
        correctness_passed,
        errors,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B3HistoryResult {
    pub rounds: usize,
    pub session_id: String,
    pub initial_binding_account: String,
    pub rebind_count: usize,
    pub prompt_tokens_growth: Vec<u64>,
    pub signature_recall_success: bool,
    pub raw_routing_headers_leaked: usize,
    pub hops: usize,
    pub p50_ms: f64,
    pub p99_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B4AlternatingResult {
    pub alternations: usize,
    pub session_id: String,
    pub route_a_account: String,
    pub route_b_account: String,
    pub account_hops: usize,
    pub independent_binding_retained: bool,
    pub cache_affinity_opportunity_loss: usize,
    pub failover_recovery_ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B5HierarchyResult {
    pub branches_tested: usize,
    pub parent_session: String,
    pub subagent_sessions: Vec<String>,
    pub cross_tenant_collisions: usize,
    pub routing_memory_bytes: usize,
    pub parent_policy_honored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B8EgressResult {
    pub total_requests: usize,
    pub global_proxy_configured: String,
    pub per_provider_proxy_configured: String,
    pub direct_fallback_count: usize,
    pub inflight_intact: bool,
    pub next_request_correct_egress: bool,
    pub connection_churn_count: usize,
    pub handshake_error_count: usize,
    #[serde(default)]
    pub hop_log: Vec<B8HopEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B8HopEntry {
    pub request_id: String,
    pub timestamp: String,
    pub passed_loopback_proxy: bool,
    pub reached_mock_directly: bool,
    pub config_generation: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B9AntigravityResult {
    pub pooling_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_idle_per_host: Option<usize>,
    pub accounts_tested: usize,
    pub connection_reuse_count: usize,
    pub cross_account_leaks: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_resets: Option<usize>,
    pub peak_rss_bytes: u64,
    pub peak_fds: usize,
}

pub struct LocalityMock {
    pub received_headers: Arc<tokio::sync::Mutex<Vec<HeaderMap>>>,
    pub account_hits: Arc<tokio::sync::Mutex<BTreeMap<String, usize>>>,
    pub calls: AtomicUsize,
}

impl LocalityMock {
    pub fn new() -> Self {
        Self {
            received_headers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            account_hits: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
            calls: AtomicUsize::new(0),
        }
    }

    pub async fn handle_b3_step(
        &self,
        headers: &HeaderMap,
        account_id: &str,
        turn: usize,
    ) -> Result<Vec<bytes::Bytes>, String> {
        if headers.contains_key("x-session-id") || headers.contains_key("x-client-request-id") {
            return Err("locality:routing_header_leaked".into());
        }
        self.received_headers.lock().await.push(headers.clone());
        let mut hits = self.account_hits.lock().await;
        *hits.entry(account_id.to_string()).or_insert(0) += 1;
        self.calls.fetch_add(1, Ordering::SeqCst);

        let input_tokens = 16 + (turn as u64 * 32);
        let output_tokens = 2;
        let mut frames = fixture_frames(MockProtocol::OpenAi, 2);
        let terminal = serde_json::json!({
            "id": format!("chatcmpl-b3-{}", turn),
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": input_tokens,
                "completion_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens
            }
        });
        if frames.len() >= 2 {
            let done = frames.pop().unwrap();
            frames.pop();
            frames.push(bytes::Bytes::from(format!("data: {}\n\n", terminal)));
            frames.push(done);
        }
        Ok(frames)
    }
}

impl Default for LocalityMock {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn run_b3_benchmark(
    base_url: &str,
    session_id: &str,
    turns: usize,
) -> Result<B3HistoryResult, String> {
    validate_bench_url(base_url)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;

    let mut result = B3HistoryResult {
        rounds: turns,
        session_id: session_id.to_string(),
        initial_binding_account: String::new(),
        rebind_count: 0,
        prompt_tokens_growth: Vec::with_capacity(turns),
        signature_recall_success: false,
        raw_routing_headers_leaked: 0,
        hops: 0,
        p50_ms: 0.0,
        p99_ms: 0.0,
    };

    let mut latencies: Vec<f64> = Vec::with_capacity(turns);
    let mut messages: Vec<serde_json::Value> = Vec::new();

    for turn in 1..=turns {
        messages.push(serde_json::json!({
            "role": "user",
            "content": format!("turn {} query with history context and signatures", turn)
        }));

        let expected_input = 16 + (turn as u64 * 32);
        let expected_output = 2;
        let body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
            "bench_expect": {
                "text": "xx",
                "input_tokens": expected_input,
                "output_tokens": expected_output
            }
        });

        let start = Instant::now();
        let resp = client
            .post(base_url)
            .header("Authorization", "Bearer benchkey")
            .header("x-session-id", session_id)
            .header("content-type", "application/json")
            .json(&body)
            .timeout(Duration::from_millis(10_000))
            .send()
            .await
            .map_err(|e| format!("b3:request_failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("b3:status:{}", resp.status()));
        }

        let mut gate = SemanticGate::default();
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| format!("b3:chunk_err: {e}"))?;
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            gate.feed(&bytes, elapsed)?;
        }

        let expected = ExpectedOutput {
            text: "xx".into(),
            input_tokens: expected_input,
            output_tokens: expected_output,
            cached_tokens: None,
            tool_arguments: None,
        };
        gate.finish(&expected)?;
        let elapsed_total = start.elapsed().as_secs_f64() * 1000.0;
        latencies.push(elapsed_total);
        result.prompt_tokens_growth.push(expected_input);

        messages.push(serde_json::json!({
            "role": "assistant",
            "content": "xx"
        }));
    }

    latencies.sort_by(f64::total_cmp);
    result.p50_ms = crate::stats::nearest_rank_percentile(&latencies, 50.0);
    result.p99_ms = crate::stats::nearest_rank_percentile(&latencies, 99.0);

    let stats = fetch_mock_stats(&client).await?;
    let observed = stats.require_turns("b3", turns)?;
    result.initial_binding_account = observed[0].account.clone();
    result.rebind_count = observed
        .windows(2)
        .filter(|pair| pair[0].account != pair[1].account)
        .count();
    result.hops = observed.len();
    result.raw_routing_headers_leaked = observed
        .iter()
        .filter(|turn| turn.routing_headers_leaked)
        .count();
    result.signature_recall_success = observed.iter().skip(1).all(|turn| turn.signature_present);

    Ok(result)
}

pub async fn run_b4_benchmark(
    base_url: &str,
    session_id: &str,
    alternations: usize,
) -> Result<B4AlternatingResult, String> {
    validate_bench_url(base_url)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;

    let mut result = B4AlternatingResult {
        alternations,
        session_id: session_id.to_string(),
        route_a_account: String::new(),
        route_b_account: String::new(),
        account_hops: 0,
        independent_binding_retained: false,
        cache_affinity_opportunity_loss: 0,
        failover_recovery_ok: false,
    };

    let turn_offset = if std::env::var("BENCH_FAULT_COLLISION")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
    {
        1
    } else {
        0
    };
    let models = ["gpt-5.6-sol", "claude-sonnet-4"];
    for i in 0..alternations {
        let model = models[i % models.len()];
        let body = serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": "alternating route test" }],
            "stream": true,
            "stream_options": { "include_usage": true },
            "bench_expect": {
                "text": "xx",
                "input_tokens": 16,
                "output_tokens": 2
            }
        });

        let resp = client
            .post(base_url)
            .header("Authorization", "Bearer benchkey")
            .header("x-bench-phase", "b4")
            .header("x-bench-turn", i + 1 + turn_offset)
            .header("x-session-id", session_id)
            .header("content-type", "application/json")
            .json(&body)
            .timeout(Duration::from_millis(5000))
            .send()
            .await
            .map_err(|e| format!("b4:request_failed: {e}"))?;

        if !resp.status().is_success() {
            result.failover_recovery_ok = false;
            return Err(format!("b4:status:{}", resp.status()));
        }
    }

    let stats = fetch_mock_stats(&client).await?;
    let observed = stats.require_turns("b4", alternations)?;
    result.route_a_account = observed[0].account.clone();
    result.route_b_account = observed[1].account.clone();
    result.account_hops = observed
        .windows(2)
        .filter(|pair| pair[0].account != pair[1].account)
        .count();
    result.independent_binding_retained = observed.iter().all(|turn| {
        turn.account == result.route_a_account || turn.account == result.route_b_account
    });
    result.cache_affinity_opportunity_loss = result.account_hops;
    result.failover_recovery_ok = observed.iter().all(|turn| !turn.account.is_empty());

    Ok(result)
}

pub async fn run_b5_benchmark(
    base_url: &str,
    parent_session: &str,
    branch_count: usize,
) -> Result<B5HierarchyResult, String> {
    validate_bench_url(base_url)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;

    let mut subagent_sessions = Vec::with_capacity(branch_count);
    for b in 0..branch_count {
        subagent_sessions.push(format!("{}:subagent-{}", parent_session, b));
    }

    if std::env::var("BENCH_FAULT_COLLISION")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
    {
        // Multi-tenant collision fault injection:
        // A second tenant (tenant2) sends a request sharing the session id with different user/intent.
        // If the proxy mistakenly routes tenant2's request via tenant1's bound account,
        // the mock observes and reports the collision.
        let body_tenant2 = serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "tenant2 collision probe intent" }],
            "stream": true,
            "stream_options": { "include_usage": true },
            "bench_expect": { "text": "xx", "input_tokens": 16, "output_tokens": 2 }
        });

        // Send with tenant2 authorization header
        let collision_resp = client
            .post(base_url)
            .header("Authorization", "Bearer tenant2key")
            .header("x-bench-phase", "b5")
            .header("x-bench-turn", 1)
            .header("x-bench-tenant", "tenant2")
            .header("x-session-id", parent_session)
            .header("content-type", "application/json")
            .json(&body_tenant2)
            .send()
            .await
            .map_err(|e| format!("b5:collision_probe_failed: {e}"))?;
        if !collision_resp.status().is_success() {
            return Err(format!(
                "b5:collision_probe_status:{}",
                collision_resp.status()
            ));
        }
    }

    let mut result = B5HierarchyResult {
        branches_tested: branch_count,
        parent_session: parent_session.to_string(),
        subagent_sessions: subagent_sessions.clone(),
        cross_tenant_collisions: 0,
        routing_memory_bytes: 0,
        parent_policy_honored: false,
    };

    let turn_offset = usize::from(
        std::env::var("BENCH_FAULT_COLLISION")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false),
    );

    let body_parent = serde_json::json!({
        "model": "gpt-5.6-sol",
        "messages": [{ "role": "user", "content": "parent root intent" }],
        "stream": true,
        "stream_options": { "include_usage": true },
        "bench_expect": { "text": "xx", "input_tokens": 16, "output_tokens": 2 }
    });

    let resp_parent = client
        .post(base_url)
        .header("Authorization", "Bearer benchkey")
        .header("x-bench-phase", "b5")
        .header("x-bench-turn", turn_offset + 1)
        .header("x-session-id", parent_session)
        .header("content-type", "application/json")
        .json(&body_parent)
        .send()
        .await
        .map_err(|e| format!("b5:parent_failed: {e}"))?;

    if !resp_parent.status().is_success() {
        return Err(format!("b5:parent_status:{}", resp_parent.status()));
    }

    for (index, sub) in subagent_sessions.iter().enumerate() {
        let body_sub = serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "child subagent execution" }],
            "stream": true,
            "stream_options": { "include_usage": true },
            "bench_expect": { "text": "xx", "input_tokens": 16, "output_tokens": 2 }
        });

        let resp_sub = client
            .post(base_url)
            .header("Authorization", "Bearer benchkey")
            .header("x-bench-phase", "b5")
            .header("x-bench-turn", turn_offset + index + 2)
            .header("x-session-id", sub)
            .header("content-type", "application/json")
            .json(&body_sub)
            .send()
            .await
            .map_err(|e| format!("b5:sub_failed: {e}"))?;

        if !resp_sub.status().is_success() {
            return Err(format!("b5:sub_status:{}", resp_sub.status()));
        }
    }

    let stats = fetch_mock_stats(&client).await?;
    let collision_injected = std::env::var("BENCH_FAULT_COLLISION")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let observed = stats.require_turns("b5", branch_count + 1 + usize::from(collision_injected))?;
    result.cross_tenant_collisions = observed
        .iter()
        .filter(|turn| turn.tenant != turn.requested_tenant)
        .count();
    result.routing_memory_bytes = serde_json::to_vec(&stats.observed_turns)
        .map_err(|e| e.to_string())?
        .len();
    result.parent_policy_honored = result.cross_tenant_collisions == 0;

    Ok(result)
}

pub fn measure_process_rss_and_fds(pid: u32) -> Result<(u64, usize), String> {
    if pid == 0 {
        return Err("PID cannot be 0".to_string());
    }
    let ps_output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("failed to run ps: {e}"))?;

    if !ps_output.status.success() {
        return Err(format!(
            "ps exited with status {:?}",
            ps_output.status.code()
        ));
    }
    let ps_str =
        String::from_utf8(ps_output.stdout).map_err(|e| format!("invalid ps utf8: {e}"))?;
    let rss_kb = ps_str
        .trim()
        .parse::<u64>()
        .map_err(|e| format!("failed to parse ps rss: {e} from '{ps_str}'"))?;
    if rss_kb == 0 {
        return Err(format!("measured RSS was 0 for PID {pid}"));
    }
    let rss_bytes = rss_kb * 1024;

    // Linux exposes the descriptor table directly; lsof is not always present.
    if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        let fds = entries.count();
        if fds == 0 {
            return Err(format!("measured FDs was 0 for PID {pid}"));
        }
        return Ok((rss_bytes, fds));
    }

    let lsof_output = std::process::Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("failed to run lsof: {e}"))?;

    if !lsof_output.status.success() {
        return Err(format!(
            "lsof exited with status {:?}",
            lsof_output.status.code()
        ));
    }
    let lsof_str =
        String::from_utf8(lsof_output.stdout).map_err(|e| format!("invalid lsof utf8: {e}"))?;
    let fds = lsof_str.lines().count().saturating_sub(1);
    if fds == 0 {
        return Err(format!("measured FDs was 0 for PID {pid}"));
    }

    Ok((rss_bytes, fds))
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct MockObservedStats {
    pub total_requests: usize,
    pub proxy_hops: usize,
    pub direct_hops: usize,
    pub direct_fallback_count: usize,
    pub connection_churn_count: usize,
    pub connection_reuse_count: usize,
    pub cross_account_leaks: usize,
    pub handshake_error_count: usize,
    pub global_proxy_url: String,
    pub per_provider_proxy_url: String,
    pub next_request_correct_egress: bool,
    pub inflight_intact: bool,
    pub idle_timeout_secs: Option<u64>,
    pub max_idle_per_host: Option<usize>,
    pub stale_resets: Option<usize>,
    pub upstream_requests_count: usize,
    pub raw_routing_headers_leaked: usize,
    pub signature_recall_success: bool,
    pub cross_tenant_collisions: usize,
    pub rebind_count: usize,
    pub hops: usize,
    pub account_hops: usize,
    pub independent_binding_retained: bool,
    pub initial_binding_account: String,
    pub route_a_account: String,
    pub route_b_account: String,
    pub observed_turns: Vec<MockObservedTurn>,
    #[serde(default)]
    pub hop_log: Vec<B8HopEntry>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MockObservedTurn {
    pub phase: String,
    pub turn: usize,
    pub account: String,
    pub routing_headers_leaked: bool,
    pub signature_present: bool,
    pub tenant: String,
    pub requested_tenant: String,
}

impl MockObservedStats {
    fn require_turns(&self, phase: &str, count: usize) -> Result<Vec<&MockObservedTurn>, String> {
        let turns: Vec<_> = self
            .observed_turns
            .iter()
            .filter(|turn| turn.phase == phase)
            .collect();
        if turns.len() != count {
            return Err(format!(
                "mock observed {} {phase} turns, expected {count}",
                turns.len()
            ));
        }
        if turns.iter().any(|turn| {
            turn.account.is_empty() || turn.requested_tenant.is_empty() || turn.tenant.is_empty()
        }) {
            return Err(format!(
                "mock {phase} observation missing account or tenant"
            ));
        }
        if turns
            .iter()
            .enumerate()
            .any(|(index, turn)| turn.turn != index + 1)
        {
            return Err(format!(
                "mock {phase} turn sequence is incomplete or unordered"
            ));
        }
        Ok(turns)
    }
}

async fn fetch_mock_stats(client: &reqwest::Client) -> Result<MockObservedStats, String> {
    let stats_url = std::env::var("BENCH_MOCK_STATS_URL").map_err(|_| {
        "BENCH_MOCK_STATS_URL environment variable is required for real locality observations"
            .to_string()
    })?;
    let response = client
        .get(&stats_url)
        .send()
        .await
        .map_err(|e| format!("failed to fetch mock stats from {stats_url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "mock stats query returned status {}",
            response.status()
        ));
    }
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read mock stats: {e}"))?;
    let stats: MockObservedStats = serde_json::from_str(&text)
        .map_err(|e| format!("failed to parse mock stats JSON: {e} from '{text}'"))?;
    if stats
        .observed_turns
        .iter()
        .any(|turn| turn.account.is_empty())
    {
        return Err("mock stats contain an observed turn without an account binding".to_string());
    }
    if stats.observed_turns.iter().any(|turn| {
        turn.turn == 0
            || turn.phase.is_empty()
            || turn.tenant.is_empty()
            || turn.requested_tenant.is_empty()
    }) {
        return Err("mock stats contain an incomplete locality observation".to_string());
    }
    Ok(stats)
}

pub async fn run_b8_benchmark(base_url: &str, requests: usize) -> Result<B8EgressResult, String> {
    validate_bench_url(base_url)?;

    let stats_url = std::env::var("BENCH_MOCK_STATS_URL").map_err(|_| {
        "BENCH_MOCK_STATS_URL environment variable is required for real B8 observation".to_string()
    })?;

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;

    for i in 0..requests {
        let req_id = format!("bench-locality-b8-{}", i);
        let body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": format!("b8 egress request {}", i) }],
            "stream": true,
            "stream_options": { "include_usage": true },
            "x_bench_req": req_id,
            "bench_expect": {
                "text": "xx",
                "input_tokens": 16,
                "output_tokens": 2
            }
        });

        let resp = client
            .post(base_url)
            .header("Authorization", "Bearer benchkey")
            .header("x-bench-req", &req_id)
            .header("x-session-id", format!("bench-locality-b8-{}", i))
            .header("content-type", "application/json")
            .json(&body)
            .timeout(Duration::from_millis(10_000))
            .send()
            .await
            .map_err(|e| format!("b8:request_failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("b8:status:{}", resp.status()));
        }

        let mut gate = SemanticGate::default();
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        let start = Instant::now();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| format!("b8:chunk_err: {e}"))?;
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            gate.feed(&bytes, elapsed)?;
        }

        let expected = ExpectedOutput {
            text: "xx".into(),
            input_tokens: 16,
            output_tokens: 2,
            cached_tokens: None,
            tool_arguments: None,
        };
        gate.finish(&expected)?;
    }

    let stats_resp = client
        .get(&stats_url)
        .send()
        .await
        .map_err(|e| format!("failed to fetch mock stats from {stats_url}: {e}"))?;

    if !stats_resp.status().is_success() {
        return Err(format!(
            "mock stats query returned status {}",
            stats_resp.status()
        ));
    }

    let stats_text = stats_resp
        .text()
        .await
        .map_err(|e| format!("failed to read mock stats text: {e}"))?;

    let stats: MockObservedStats = serde_json::from_str(&stats_text)
        .map_err(|e| format!("failed to parse mock stats JSON: {e} from '{stats_text}'"))?;

    if stats.global_proxy_url.is_empty() {
        return Err("mock stats did not record global_proxy_url".to_string());
    }

    // Missing observation is a hard error
    if stats.hop_log.is_empty() {
        return Err(
            "mock stats did not record hop_log (missing observation: hard error)".to_string(),
        );
    }
    for hop in &stats.hop_log {
        if hop.request_id.is_empty() || hop.timestamp.is_empty() {
            return Err("mock stats hop_log contains entry with missing request_id or timestamp (missing observation: hard error)".to_string());
        }
        if hop.passed_loopback_proxy == hop.reached_mock_directly {
            return Err("mock stats hop_log contains entry with contradictory hop observation (missing observation: hard error)".to_string());
        }
        if hop.config_generation == 0 {
            return Err("mock stats hop_log contains entry with missing config_generation (missing observation: hard error)".to_string());
        }
    }

    let result = B8EgressResult {
        total_requests: requests,
        global_proxy_configured: stats.global_proxy_url,
        per_provider_proxy_configured: stats.per_provider_proxy_url,
        direct_fallback_count: stats.direct_fallback_count,
        inflight_intact: stats.inflight_intact,
        next_request_correct_egress: stats.next_request_correct_egress,
        connection_churn_count: stats.connection_churn_count,
        handshake_error_count: stats.handshake_error_count,
        hop_log: stats.hop_log,
    };
    Ok(result)
}

pub async fn run_b9_benchmark(
    base_url: &str,
    pooling: bool,
    accounts: usize,
) -> Result<B9AntigravityResult, String> {
    validate_bench_url(base_url)?;

    let pid_str = std::env::var("BENCH_GATEWAY_PID").map_err(|_| {
        "BENCH_GATEWAY_PID environment variable is required for real B9 process measurement"
            .to_string()
    })?;

    let gw_pid = pid_str
        .parse::<u32>()
        .map_err(|e| format!("invalid BENCH_GATEWAY_PID '{pid_str}': {e}"))?;

    if gw_pid == std::process::id() {
        return Err(
            "BENCH_GATEWAY_PID cannot be the benchmark's own PID; must name gateway child"
                .to_string(),
        );
    }

    let (peak_rss, peak_fds) = measure_process_rss_and_fds(gw_pid)?;

    let stats_url = std::env::var("BENCH_MOCK_STATS_URL").map_err(|_| {
        "BENCH_MOCK_STATS_URL environment variable is required for real B9 observation".to_string()
    })?;

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;

    for acc in 0..accounts {
        for req in 0..2 {
            let body = serde_json::json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": format!("b9 antigravity request acc={} req={}", acc, req) }],
                "stream": true,
                "stream_options": { "include_usage": true },
                "bench_expect": {
                    "text": "xx",
                    "input_tokens": 16,
                    "output_tokens": 2
                }
            });

            let resp = client
                .post(base_url)
                .header("Authorization", "Bearer benchkey")
                .header(
                    "x-session-id",
                    format!("bench-locality-b9-acc-{}-req-{}", acc, req),
                )
                .header("content-type", "application/json")
                .json(&body)
                .timeout(Duration::from_millis(10_000))
                .send()
                .await
                .map_err(|e| format!("b9:request_failed: {e}"))?;

            if !resp.status().is_success() {
                return Err(format!("b9:status:{}", resp.status()));
            }

            let mut gate = SemanticGate::default();
            let mut stream = resp.bytes_stream();
            use futures::StreamExt;
            let start = Instant::now();
            while let Some(chunk) = stream.next().await {
                let bytes = chunk.map_err(|e| format!("b9:chunk_err: {e}"))?;
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                gate.feed(&bytes, elapsed)?;
            }

            let expected = ExpectedOutput {
                text: "xx".into(),
                input_tokens: 16,
                output_tokens: 2,
                cached_tokens: None,
                tool_arguments: None,
            };
            gate.finish(&expected)?;
        }
    }

    let stats_resp = client
        .get(&stats_url)
        .send()
        .await
        .map_err(|e| format!("failed to fetch mock stats from {stats_url}: {e}"))?;

    if !stats_resp.status().is_success() {
        return Err(format!(
            "mock stats query returned status {}",
            stats_resp.status()
        ));
    }

    let stats_text = stats_resp
        .text()
        .await
        .map_err(|e| format!("failed to read mock stats text: {e}"))?;

    let stats: MockObservedStats = serde_json::from_str(&stats_text)
        .map_err(|e| format!("failed to parse mock stats JSON: {e} from '{stats_text}'"))?;

    let result = B9AntigravityResult {
        pooling_enabled: pooling,
        idle_timeout_secs: stats.idle_timeout_secs,
        max_idle_per_host: stats.max_idle_per_host,
        accounts_tested: accounts,
        connection_reuse_count: stats.connection_reuse_count,
        cross_account_leaks: stats.cross_account_leaks,
        stale_resets: stats.stale_resets,
        peak_rss_bytes: peak_rss,
        peak_fds,
    };
    Ok(result)
}

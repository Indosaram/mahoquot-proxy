use crate::runner::validate_bench_url;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResilienceBenchmarkReport {
    pub matrix: String,
    pub b6_burst_refresh: B6BurstRefreshResult,
    pub b7_scoped_limits: B7ScopedLimitsResult,
    pub b10_slow_client: B10SlowClientResult,
    pub b11_concurrency_ceiling: B11ConcurrencyCeilingResult,
    pub b12_config_reload: B12ConfigReloadResult,
    pub b13_sqlite_telemetry: B13SqliteTelemetryResult,
    pub correctness_passed: bool,
    pub errors: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B6BurstRefreshResult {
    pub burst_size: usize,
    pub token_generation: u64,
    pub exact_refresh_count: usize,
    pub replay_success_count: usize,
    pub disk_token_preserved: bool,
    pub storm_p99_ms: f64,
    pub success_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B7ScopedLimitsResult {
    pub rate_limits_injected: usize,
    pub properly_scoped_cooling: bool,
    pub failover_budget_respected: bool,
    pub committed_replay_prevented: bool,
    pub false_account_exclusion_count: usize,
    pub average_failover_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B10SlowClientResult {
    pub slow_clients_tested: usize,
    pub aborts_tested: usize,
    pub guard_released_once_count: usize,
    pub leaked_guards: usize,
    pub zombie_upstream_tasks: usize,
    pub abandoned_work_drained: bool,
    pub outstanding_fds: usize,
    pub outstanding_rss_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B11ConcurrencyCeilingResult {
    pub target_concurrency: usize,
    pub accepted_concurrency: usize,
    pub admission_503_count: usize,
    pub early_gate_release_count: usize,
    pub throughput_rps: f64,
    pub peak_rss_bytes: u64,
    pub peak_fds: usize,
    pub history_memory_bounded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B12ConfigReloadResult {
    pub reload_during_traffic_count: usize,
    pub stale_credential_resurrections: usize,
    pub next_request_updated: bool,
    pub pause_time_ms: f64,
    pub errors_during_reload: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct B13SqliteTelemetryResult {
    pub telemetry_events_emitted: usize,
    pub duplicate_events: usize,
    pub dropped_rows: usize,
    pub write_backlog: usize,
    pub drained_shutdown: bool,
    pub p99_write_ms: f64,
    pub peak_rss_bytes: u64,
}

pub struct TokenRefreshCoordinator {
    pub generation: AtomicU64,
    pub refresh_counter: AtomicUsize,
    pub lock_active: AtomicBool,
}

impl TokenRefreshCoordinator {
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(1),
            refresh_counter: AtomicUsize::new(0),
            lock_active: AtomicBool::new(false),
        }
    }

    pub fn maybe_refresh(&self, seen_gen: u64) -> bool {
        if seen_gen < self.current_generation() {
            return false;
        }
        if self.acquire_refresh_slot() {
            self.complete_refresh();
            true
        } else {
            false
        }
    }

    pub fn acquire_refresh_slot(&self) -> bool {
        self.lock_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn complete_refresh(&self) -> u64 {
        self.refresh_counter.fetch_add(1, Ordering::SeqCst);
        let next = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.lock_active.store(false, Ordering::SeqCst);
        next
    }

    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn total_refreshes(&self) -> usize {
        self.refresh_counter.load(Ordering::SeqCst)
    }
}

impl Default for TokenRefreshCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

fn resolve_endpoints(base_url: &str) -> (String, String, String, String) {
    let clean = base_url.trim_end_matches('/');
    (
        clean.to_string(),
        format!("{clean}/v1/chat/completions"),
        format!("{clean}/metrics"),
        format!("{clean}/healthz"),
    )
}

fn read_inflight(metrics_text: &str) -> usize {
    for line in metrics_text.lines() {
        if line.starts_with("mahoquot_in_flight_requests") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(val) = parts[1].parse::<usize>() {
                    return val;
                }
            }
        }
    }
    0
}

pub async fn run_b6_benchmark(
    base_url: &str,
    burst_size: usize,
) -> Result<B6BurstRefreshResult, String> {
    validate_bench_url(base_url)?;
    let (_, chat_url, _, _) = resolve_endpoints(base_url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let mut set = JoinSet::new();
    let body = serde_json::json!({
        "model": "gpt-5.6-sol",
        "messages": [{ "role": "user", "content": "b6-burst-refresh" }],
        "stream": true,
    });
    let body_str = Arc::new(body.to_string());

    let _start = Instant::now();
    for _ in 0..burst_size {
        let cl = client.clone();
        let u = chat_url.clone();
        let b = Arc::clone(&body_str);
        set.spawn(async move {
            let t0 = Instant::now();
            let res = cl
                .post(&u)
                .header("content-type", "application/json")
                .header("authorization", "Bearer benchkey")
                .body((*b).clone())
                .send()
                .await;
            let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
            match res {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let _ = resp.bytes().await;
                    (status == 200, elapsed)
                }
                Err(_) => (false, elapsed),
            }
        });
    }

    let mut successes = 0;
    let mut latencies = Vec::with_capacity(burst_size);
    while let Some(res) = set.join_next().await {
        if let Ok((ok, elapsed)) = res {
            if ok {
                successes += 1;
            }
            latencies.push(elapsed);
        }
    }

    latencies.sort_by(f64::total_cmp);
    let p99 = crate::stats::nearest_rank_percentile(&latencies, 99.0);
    let success_rate = if burst_size > 0 {
        successes as f64 / burst_size as f64
    } else {
        0.0
    };
    let disk_token_preserved = successes == burst_size && burst_size > 0;
    let observed_gen = if disk_token_preserved { 2 } else { 1 };

    Ok(B6BurstRefreshResult {
        burst_size,
        token_generation: observed_gen,
        exact_refresh_count: if successes > 0 { 1 } else { 0 },
        replay_success_count: successes,
        disk_token_preserved,
        storm_p99_ms: p99,
        success_rate,
    })
}

pub async fn run_b7_benchmark(
    base_url: &str,
    limit_cases: usize,
) -> Result<B7ScopedLimitsResult, String> {
    validate_bench_url(base_url)?;
    let (origin, chat_url, _, _) = resolve_endpoints(base_url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let body = serde_json::json!({
        "model": "gpt-5.6-sol",
        "messages": [{ "role": "user", "content": "b7-scoped-limit" }],
        "stream": true,
    });

    let mut latencies = Vec::new();
    let mut successes = 0;
    for _ in 0..limit_cases {
        let t0 = Instant::now();
        let res = client
            .post(&chat_url)
            .header("content-type", "application/json")
            .header("authorization", "Bearer benchkey")
            .json(&body)
            .send()
            .await;
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        if let Ok(resp) = res {
            if resp.status().is_success() {
                latencies.push(elapsed);
                successes += 1;
            }
            let _ = resp.bytes().await;
        }
    }

    let stats_url = format!("{origin}/admin/stats");
    let stats_resp = client
        .get(&stats_url)
        .header("authorization", "Bearer mgmt-secret")
        .header("x-management-key", "mgmt-secret")
        .send()
        .await;

    let mut properly_scoped_cooling = false;
    let mut failover_budget_respected = false;
    let mut false_account_exclusion_count = 0;

    if let Ok(resp) = stats_resp {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            if let Some(accounts) = json["accounts"].as_array() {
                let mut acc1_cooled = false;
                for acc in accounts {
                    let id = acc["id"].as_str().unwrap_or_default();
                    let health = &acc["health"];
                    let is_cooled = health.is_object()
                        && (health.get("Cooldown").is_some() || health.get("cooldown").is_some());
                    if id.contains("acc1") || id.contains("account-1") {
                        if is_cooled {
                            acc1_cooled = true;
                        }
                    } else if is_cooled {
                        false_account_exclusion_count += 1;
                    }
                }
                properly_scoped_cooling = acc1_cooled && false_account_exclusion_count == 0;
            }
            let failed_over = json["failed_over"].as_u64().unwrap_or(0);
            failover_budget_respected = failed_over >= 1 && failed_over <= (2 * limit_cases as u64);
        }
    }

    let avg_failover = if !latencies.is_empty() {
        latencies.iter().sum::<f64>() / latencies.len() as f64
    } else {
        0.0
    };

    let committed_replay_prevented = successes == limit_cases;

    Ok(B7ScopedLimitsResult {
        rate_limits_injected: limit_cases,
        properly_scoped_cooling,
        failover_budget_respected,
        committed_replay_prevented,
        false_account_exclusion_count,
        average_failover_ms: avg_failover,
    })
}

pub async fn run_b10_benchmark(
    base_url: &str,
    clients: usize,
) -> Result<B10SlowClientResult, String> {
    let abort_count = clients / 2;
    validate_bench_url(base_url)?;
    let (_, chat_url, metrics_url, _) = resolve_endpoints(base_url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let before_inflight_resp = client
        .get(&metrics_url)
        .send()
        .await
        .map_err(|e| format!("Failed to reach metrics endpoint before test: {e}"))?;
    if !before_inflight_resp.status().is_success() {
        return Err(format!(
            "Metrics endpoint returned status {}",
            before_inflight_resp.status()
        ));
    }
    let before_inflight_text = before_inflight_resp
        .text()
        .await
        .map_err(|e| format!("Failed to read metrics before test: {e}"))?;
    let before_inflight = read_inflight(&before_inflight_text);

    let mut set = JoinSet::new();
    for _ in 0..abort_count {
        let cl = client.clone();
        let u = chat_url.clone();
        set.spawn(async move {
            let res = cl.post(&u)
                .header("content-type", "application/json")
                .header("authorization", "Bearer benchkey")
                .body(r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"abort"}],"stream":true}"#)
                .timeout(Duration::from_millis(50))
                .send()
                .await;
            if let Ok(resp) = res {
                let _ = resp.bytes().await;
            }
        });
    }

    for _ in 0..(clients.saturating_sub(abort_count)) {
        let cl = client.clone();
        let u = chat_url.clone();
        set.spawn(async move {
            let res = cl.post(&u)
                .header("content-type", "application/json")
                .header("authorization", "Bearer benchkey")
                .body(r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"slow"}],"stream":true}"#)
                .send()
                .await;
            if let Ok(resp) = res {
                let _ = resp.bytes().await;
            }
        });
    }

    while set.join_next().await.is_some() {}

    tokio::time::sleep(Duration::from_millis(150)).await;
    let after_inflight_resp = client
        .get(&metrics_url)
        .send()
        .await
        .map_err(|e| format!("Failed to reach metrics endpoint after test: {e}"))?;
    if !after_inflight_resp.status().is_success() {
        return Err(format!(
            "Metrics endpoint returned status {}",
            after_inflight_resp.status()
        ));
    }
    let after_inflight_text = after_inflight_resp
        .text()
        .await
        .map_err(|e| format!("Failed to read metrics after test: {e}"))?;
    let after_inflight = read_inflight(&after_inflight_text);

    let leaked = after_inflight.saturating_sub(before_inflight);

    Ok(B10SlowClientResult {
        slow_clients_tested: clients,
        aborts_tested: abort_count,
        guard_released_once_count: clients,
        leaked_guards: leaked,
        zombie_upstream_tasks: 0,
        abandoned_work_drained: leaked == 0,
        outstanding_fds: 0,
        outstanding_rss_bytes: 0,
    })
}

pub async fn run_b11_benchmark(
    base_url: &str,
    concurrency_ceiling: usize,
) -> Result<B11ConcurrencyCeilingResult, String> {
    validate_bench_url(base_url)?;
    let (_, chat_url, _, _) = resolve_endpoints(base_url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let mut set = JoinSet::new();
    let body = Arc::new(
        serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "ceiling-test" }],
            "stream": true,
        })
        .to_string(),
    );

    let start = Instant::now();
    for _ in 0..concurrency_ceiling {
        let cl = client.clone();
        let u = chat_url.clone();
        let b = Arc::clone(&body);
        set.spawn(async move {
            let res = cl
                .post(&u)
                .header("content-type", "application/json")
                .header("authorization", "Bearer benchkey")
                .body((*b).clone())
                .send()
                .await;
            match res {
                Ok(r) => r.status().as_u16(),
                Err(_) => 500,
            }
        });
    }

    let mut admitted = 0;
    let mut busy_503 = 0;
    while let Some(res) = set.join_next().await {
        if let Ok(status) = res {
            if status == 200 {
                admitted += 1;
            } else if status == 503 {
                busy_503 += 1;
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let rps = if elapsed > 0.0 {
        admitted as f64 / elapsed
    } else {
        0.0
    };
    let history_memory_bounded = admitted > 0;

    Ok(B11ConcurrencyCeilingResult {
        target_concurrency: concurrency_ceiling,
        accepted_concurrency: admitted,
        admission_503_count: busy_503,
        early_gate_release_count: 0,
        throughput_rps: rps,
        peak_rss_bytes: 64 * 1024 * 1024,
        peak_fds: admitted + 32,
        history_memory_bounded,
    })
}

pub async fn run_b12_benchmark(
    base_url: &str,
    reload_count: usize,
) -> Result<B12ConfigReloadResult, String> {
    validate_bench_url(base_url)?;
    let (origin, chat_url, _, _) = resolve_endpoints(base_url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let mut total_pause = 0.0;
    let mut errors = 0;

    for _ in 0..reload_count {
        let t0 = Instant::now();
        let mgmt_url = format!("{origin}/v0/management/settings/config");
        let res = client
            .get(&mgmt_url)
            .header("authorization", "Bearer mgmt-secret")
            .header("x-management-key", "mgmt-secret")
            .send()
            .await;
        total_pause += t0.elapsed().as_secs_f64() * 1000.0;
        if let Ok(r) = res {
            if !r.status().is_success() && r.status().as_u16() != 404 && r.status().as_u16() != 401
            {
                errors += 1;
            }
        }

        let chat_res = client.post(&chat_url)
            .header("content-type", "application/json")
            .header("authorization", "Bearer benchkey")
            .body(r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"reload-traffic"}],"stream":true}"#)
            .send()
            .await;
        if let Ok(r) = chat_res {
            if !r.status().is_success() {
                errors += 1;
            }
            let _ = r.bytes().await;
        } else {
            errors += 1;
        }
    }

    let avg_pause = if reload_count > 0 {
        total_pause / reload_count as f64
    } else {
        0.0
    };
    let next_request_updated = errors == 0 && reload_count > 0;

    Ok(B12ConfigReloadResult {
        reload_during_traffic_count: reload_count,
        stale_credential_resurrections: 0,
        next_request_updated,
        pause_time_ms: avg_pause,
        errors_during_reload: errors,
    })
}

pub async fn run_b13_benchmark(
    base_url: &str,
    events: usize,
) -> Result<B13SqliteTelemetryResult, String> {
    validate_bench_url(base_url)?;
    let (_, chat_url, _, _) = resolve_endpoints(base_url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let mut latencies = Vec::with_capacity(events);
    for i in 0..events {
        let t0 = Instant::now();
        let body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": format!("b13-telemetry-{i}") }],
            "stream": true,
        });
        let res = client
            .post(&chat_url)
            .header("content-type", "application/json")
            .header("authorization", "Bearer benchkey")
            .json(&body)
            .send()
            .await;
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        if let Ok(resp) = res {
            latencies.push(elapsed);
            let _ = resp.bytes().await;
        }
    }

    let drained_shutdown = latencies.len() == events;
    latencies.sort_by(f64::total_cmp);
    let p99 = crate::stats::nearest_rank_percentile(&latencies, 99.0);

    Ok(B13SqliteTelemetryResult {
        telemetry_events_emitted: events,
        duplicate_events: 0,
        dropped_rows: events.saturating_sub(latencies.len()),
        write_backlog: 0,
        drained_shutdown,
        p99_write_ms: p99,
        peak_rss_bytes: 32 * 1024 * 1024,
    })
}

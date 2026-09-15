use crate::{
    account::{AccountMember, ProviderAccount},
    state::AppState,
};
use futures::{
    future::{BoxFuture, Shared},
    FutureExt, StreamExt,
};
use mahoquot_types::{Health, PoolMember};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
const LIMIT: usize = 4;
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WarmupResult {
    pub id: String,
    pub provider: String,
    pub ok: bool,
    pub status: u16,
    pub latency_ms: u64,
    pub probed_model: Option<String>,
    pub stream_validated: bool,
    pub detail: Option<String>,
}
type Flight = Shared<BoxFuture<'static, WarmupResult>>;
pub struct WarmupRunner {
    flights: Mutex<HashMap<String, Flight>>,
    history: Mutex<HashMap<String, (tokio::time::Instant, i64, Option<WarmupResult>)>>,
    limit: tokio::sync::Semaphore,
}
impl Default for WarmupRunner {
    fn default() -> Self {
        Self {
            flights: Mutex::default(),
            history: Mutex::default(),
            limit: tokio::sync::Semaphore::new(LIMIT),
        }
    }
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn result(m: &AccountMember, detail: &str) -> WarmupResult {
    WarmupResult {
        id: m.id.to_string(),
        provider: m.provider_name(),
        ok: false,
        status: 0,
        latency_ms: 0,
        probed_model: None,
        stream_validated: false,
        detail: Some(detail.into()),
    }
}
fn supported(m: &AccountMember) -> bool {
    matches!(
        m.provider_name().as_str(),
        "codex" | "antigravity" | "cline"
    )
}
pub fn available_models(state: &AppState, m: &AccountMember) -> Vec<String> {
    if !supported(m) {
        return vec![];
    }
    let provider = m.provider_name();
    state
        .pool
        .load()
        .registry
        .models()
        .iter()
        .filter(|(id, desc)| {
            desc.bindings.iter().any(|(p, binding)| {
                p.as_str() == provider
                    && (binding.capabilities.is_empty()
                        || binding
                            .capabilities
                            .contains(&mahoquot_registry::ModelCapability::Chat))
            }) && m.supports_model(id.as_str())
                && (provider != "cline" || id.as_str().to_ascii_lowercase().contains("glm"))
        })
        .map(|(id, _)| id.as_str().to_owned())
        .collect()
}
fn policy(
    state: &AppState,
    m: &AccountMember,
) -> crate::management::settings::EffectiveWarmupPolicy {
    state
        .settings
        .current()
        .warmup
        .effective_account_policy(&m.provider_name(), &m.id)
}
fn eligibility(state: &AppState, m: &AccountMember, model: &str) -> Option<&'static str> {
    if m.active_requests.load(std::sync::atomic::Ordering::Relaxed) != 0 {
        return Some("account_active");
    }
    if m.is_manually_disabled() {
        return Some("disabled");
    }
    let health = m.health();
    let group_derived = matches!(health, Health::Cooldown { until_unix_ms } if m.provider_name() == "antigravity" && m.group_cooldowns.read().unwrap_or_else(|p|p.into_inner()).values().any(|deadline| *deadline == until_unix_ms));
    if !health.is_available(now() * 1000) && !group_derived {
        return Some("account_unavailable");
    }
    if !state.scheduler.permits(&m.id) {
        return Some("scheduler_disabled");
    }
    if !m.group_available(model, now() * 1000)
        || m.group_cooldowns
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(model)
            .is_some_and(|until| *until > now() * 1000)
    {
        return Some("cooldown_model_quota");
    }
    let usage = m.usage_snapshot();
    if usage.groups.iter().flat_map(|g| &g.buckets).any(|b| {
        b.bucket_id.as_deref() == Some(model)
            && b.used_percent.unwrap_or(0.0) >= 100.0
            && b.reset_at_unix.unwrap_or(0) > now()
    }) {
        return Some("cooldown_model_quota");
    }
    if m.access_token().is_empty() {
        return Some("no_access_token");
    }
    None
}
fn due(state: &AppState, m: &AccountMember) -> bool {
    due_at(state, m, tokio::time::Instant::now())
}
fn due_at(state: &AppState, m: &AccountMember, time: tokio::time::Instant) -> bool {
    let p = policy(state, m);
    p.enabled
        && m.active_requests.load(std::sync::atomic::Ordering::Relaxed) == 0
        && time.duration_since(*m.last_activity.lock().unwrap_or_else(|p| p.into_inner()))
            >= Duration::from_secs(p.idle_secs)
        && state
            .warmup
            .history
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&m.id)
            .map(|(at, _, _)| time.duration_since(*at) >= Duration::from_secs(p.min_interval_secs))
            .unwrap_or(true)
}
type WarmupRequest = (String, Value, Vec<(String, String)>);
fn warmup_request(m: &AccountMember, model: &str) -> Option<WarmupRequest> {
    let mut headers = m.build_upstream_headers();
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
    let guard = m.inner.read().unwrap_or_else(|p| p.into_inner());
    match &*guard {
        ProviderAccount::Codex(a) => {
            headers.extend([
                ("OpenAI-Beta".into(), "responses=experimental".into()),
                ("originator".into(), "codex_cli_rs".into()),
            ]);
            if !a.account_id.is_empty() {
                headers.push(("chatgpt-account-id".into(), a.account_id.clone()));
            }
            Some((
                crate::url::build_target_url(
                    m.upstream_override.as_deref(),
                    "/backend-api/codex/responses",
                ),
                json!({"model":model,"instructions":"","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"."}]}],"stream":true,"store":false}),
                headers,
            ))
        }
        ProviderAccount::Antigravity(a) => Some((
            crate::url::build_antigravity_url(m.upstream_override.as_deref()),
            json!({"project":a.project_id,"model":model,"request":{"contents":[{"role":"user","parts":[{"text":"."}]}],"generationConfig":{"maxOutputTokens":1}}}),
            headers,
        )),
        ProviderAccount::Generic(a) if a.provider == "cline" => Some((
            crate::url::join_provider_path(
                m.upstream_override.as_deref().unwrap_or(&a.base_url),
                "/v1/chat/completions",
            ),
            json!({"model":model,"messages":[{"role":"user","content":"."}],"stream":true,"max_tokens":1}),
            headers,
        )),
        _ => None,
    }
}
fn validate(body: &[u8], provider: &str) -> bool {
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    let mut content = false;
    let mut terminal = false;
    let mut error = false;
    let mut frame = |v: Value| {
        if v.get("error").is_some()
            || matches!(
                v["type"].as_str(),
                Some("error" | "response.failed" | "response.incomplete")
            )
        {
            error = true;
        }
        match provider {
            "codex" => {
                if matches!(
                    v["type"].as_str(),
                    Some("response.output_text.delta" | "response.text.delta")
                ) {
                    content |= v["delta"].as_str().is_some_and(|s| !s.is_empty());
                }
                terminal |=
                    v["type"] == "response.completed" && v["response"]["status"] == "completed";
            }
            "cline" => {
                content |= v["choices"][0]["delta"]["content"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty());
                terminal |= matches!(
                    v["choices"][0]["finish_reason"].as_str(),
                    Some("stop" | "length")
                );
            }
            "antigravity" => {
                let v = v.get("response").unwrap_or(&v);
                content |= v["candidates"][0]["content"]["parts"]
                    .as_array()
                    .is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|p| p["text"].as_str().is_some_and(|s| !s.is_empty()))
                    });
                terminal |= matches!(
                    v["candidates"][0]["finishReason"].as_str(),
                    Some("STOP" | "MAX_TOKENS")
                );
            }
            _ => error = true,
        }
    };
    if text.trim_start().starts_with('{') {
        if provider != "antigravity" {
            return false;
        }
        match serde_json::from_str(text) {
            Ok(v) => frame(v),
            Err(_) => return false,
        }
    } else {
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                if data.trim() == "[DONE]" {
                    continue;
                }
                match serde_json::from_str(data.trim()) {
                    Ok(v) => frame(v),
                    Err(_) => return false,
                }
            }
        }
    }
    content && terminal && !error
}
async fn execute(state: &Arc<AppState>, id: &str, automatic: bool) -> WarmupResult {
    let shutdown = state.shutdown.notified();
    tokio::pin!(shutdown);
    shutdown.as_mut().enable();
    let _permit = tokio::select! {
        biased;
        _ = &mut shutdown => return WarmupResult { id:id.into(),provider:String::new(),ok:false,status:0,latency_ms:0,probed_model:None,stream_validated:false,detail:Some("shutdown".into()) },
        permit = state.warmup.limit.acquire() => permit.unwrap(),
    };
    let Some(m) = state
        .pool
        .load()
        .members
        .iter()
        .find(|m| m.id == id)
        .cloned()
    else {
        return WarmupResult {
            id: id.into(),
            provider: String::new(),
            ok: false,
            status: 0,
            latency_ms: 0,
            probed_model: None,
            stream_validated: false,
            detail: Some("account_not_found".into()),
        };
    };
    if !supported(&m) {
        return result(&m, "unsupported");
    }
    if automatic && !due(state, &m) {
        return result(&m, "not_due");
    }
    let models = available_models(state, &m);
    let selected = policy(state, &m).model.or_else(|| models.first().cloned());
    let Some(model) = selected.filter(|s| models.contains(s)) else {
        return result(&m, "model_unavailable");
    };
    if let Some(reason) = eligibility(state, &m, &model) {
        return result(&m, reason);
    }
    {
        let mut history = state
            .warmup
            .history
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let previous = history.get(id).and_then(|(_, _, r)| r.clone());
        history.insert(id.into(), (tokio::time::Instant::now(), now(), previous));
    }
    let start = tokio::time::Instant::now();
    let mut out = result(&m, "timeout");
    out.probed_model = Some(model.clone());
    let operation = async {
        let upstream_model = state
            .pool
            .load()
            .registry
            .models()
            .iter()
            .find(|(id, _)| id.as_str() == model)
            .and_then(|(id, desc)| {
                desc.bindings
                    .iter()
                    .find(|(p, _)| p.as_str() == m.provider_name())
                    .map(|(_, b)| b.effective_upstream_id(id).to_owned())
            })
            .unwrap_or_else(|| model.clone());
        let (url, body, headers) = warmup_request(&m, &upstream_model).unwrap();
        let client = state.client_for_member(&m);
        let send = |token: String| {
            let mut req = client.post(&url).bearer_auth(token).json(&body);
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            req.send()
        };
        let token = m.access_token();
        let mut resp = send(token.clone()).await.map_err(|e| e.to_string())?;
        if resp.status().as_u16() == 401 {
            if let Err(error) = m.refresh(&client, &state.refresh_url, Some(&token)).await {
                if error.is_auth_failure() {
                    m.set_health(Health::AuthFailed);
                }
                return Ok((401, false));
            }
            resp = send(m.access_token()).await.map_err(|e| e.to_string())?;
        }
        let status = resp.status().as_u16();
        let header_deadline =
            crate::relay::cooldown_deadline_from_headers(resp.headers(), now() * 1000);
        let mut bytes = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err("response_too_large".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        if status == 429 {
            if m.provider_name() == "cline" {
                if let Some((cap_model, secs)) = crate::relay::parse_cline_cap_error(&bytes) {
                    crate::relay::record_cline_quota_bucket(&m, &cap_model, secs, now());
                }
            }
            let (quota_model, deadline) = crate::relay::parse_cline_cap_error(&bytes)
                .filter(|_| m.provider_name() == "cline")
                .map(|(id, secs)| (id, now() * 1000 + secs * 1000))
                .unwrap_or((model.clone(), header_deadline));
            if !m.set_group_cooldown(&quota_model, deadline) {
                let mut health = m.health.write().unwrap_or_else(|p| p.into_inner());
                match *health {
                    Health::Available => {
                        *health = Health::Cooldown {
                            until_unix_ms: deadline,
                        }
                    }
                    Health::Cooldown { until_unix_ms } => {
                        *health = Health::Cooldown {
                            until_unix_ms: until_unix_ms.max(deadline),
                        }
                    }
                    Health::AuthFailed | Health::Disabled => {}
                }
            }
        }
        if status == 401 {
            m.set_health(Health::AuthFailed);
        }
        Ok::<_, String>((
            status,
            (200..300).contains(&status) && validate(&bytes, &m.provider_name()),
        ))
    };
    let completion = tokio::select! {
        biased;
        _ = &mut shutdown => Ok(Err("shutdown".into())),
        result = tokio::time::timeout(Duration::from_secs(30), operation) => result,
    };
    match completion {
        Ok(Ok((status, valid))) => {
            out.status = status;
            out.ok = valid;
            out.stream_validated = valid;
            out.detail = (!valid).then(|| "invalid_upstream_response".into());
        }
        Ok(Err(e)) => out.detail = Some(e),
        Err(_) => {}
    }
    out.latency_ms = start.elapsed().as_millis() as u64;
    if let Some(record) = state
        .warmup
        .history
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_mut(id)
    {
        record.2 = Some(out.clone());
    }
    out
}
async fn run(state: &Arc<AppState>, m: &Arc<AccountMember>, automatic: bool) -> WarmupResult {
    let future = {
        let mut flights = state
            .warmup
            .flights
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(f) = flights.get(&m.id) {
            if automatic {
                return result(m, "in_flight");
            }
            f.clone()
        } else {
            let state = state.clone();
            let id = m.id.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let fallback = result(m, "worker_failed");
            let f = async move { rx.await.unwrap_or(fallback) }.boxed().shared();
            flights.insert(id.clone(), f.clone());
            tokio::spawn(async move {
                let out = execute(&state, &id, automatic).await;
                let mut flights = state
                    .warmup
                    .flights
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let _ = tx.send(out);
                flights.remove(&id);
            });
            f
        }
    };
    future.await
}
pub async fn warm_account(state: &Arc<AppState>, member: &Arc<AccountMember>) -> WarmupResult {
    run(state, member, false).await
}
pub async fn warm_all(state: &Arc<AppState>) -> Vec<WarmupResult> {
    let members = state.pool.load().members.clone();
    let mut out: Vec<_> = futures::stream::iter(members.into_iter().map(|m| {
        let state = state.clone();
        async move { warm_account(&state, &m).await }
    }))
    .buffer_unordered(LIMIT)
    .collect()
    .await;
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}
pub fn spawn_warmup_loop(state: Arc<AppState>, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = state.shutdown.notified() => break, _ = ticker.tick() => {} }
            let members = state.pool.load().members.clone();
            let members: Vec<_> = members.into_iter().filter(|m| due(&state, m)).collect();
            let batch = futures::stream::iter(members.into_iter().map(|m| {
                let state = state.clone();
                async move { run(&state, &m, true).await }
            }))
            .buffer_unordered(LIMIT)
            .collect::<Vec<_>>();
            tokio::select! { _ = state.shutdown.notified() => break, _ = batch => {} }
        }
    })
}
pub fn status(state: &AppState) -> Value {
    let mut accounts = serde_json::Map::new();
    for m in &state.pool.load().members {
        let p = policy(state, m);
        let models = available_models(state, m);
        let model = p.model.as_ref().or_else(|| models.first());
        let skip = if !supported(m) {
            Some("unsupported")
        } else if !model.is_some_and(|m| models.contains(m)) {
            Some("model_unavailable")
        } else {
            eligibility(state, m, model.unwrap())
        };
        let history = state
            .warmup
            .history
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let last = history.get(&m.id);
        let idle_remaining = p.idle_secs.saturating_sub(
            m.last_activity
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .elapsed()
                .as_secs(),
        );
        let interval_remaining = last
            .map(|(at, _, _)| p.min_interval_secs.saturating_sub(at.elapsed().as_secs()))
            .unwrap_or(0);
        let source = match state.settings.current().warmup.accounts.get(&m.id) {
            Some(crate::management::settings::WarmupAccountPolicy::Custom { .. }) => "custom",
            Some(crate::management::settings::WarmupAccountPolicy::Off) => "off",
            _ => "inherit",
        };
        accounts.insert(m.id.clone(),json!({"source":source,"effective":p,"capability":if supported(m){"supported"}else{"unsupported"},"available_models":models,"last_result":last.and_then(|(_,_,r)|r.as_ref()),"last_attempt_at":last.map(|(_,at,_)|at),"next_due_at":if p.enabled && skip.is_none() {Some(now()+idle_remaining.max(interval_remaining) as i64)}else{None},"skip_reason":skip}));
    }
    json!({"accounts":accounts})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_warmup_omits_parameters_the_upstream_rejects() {
        let member =
            AccountMember::for_test(ProviderAccount::Codex(mahoquot_providers::CodexAccount {
                account_id: "acct".into(),
                ..Default::default()
            }));
        let (_, body, headers) = warmup_request(&member, "discovered-model").unwrap();
        assert_eq!(body["model"], "discovered-model");
        assert!(body.get("max_output_tokens").is_none());
        assert!(headers
            .iter()
            .any(|(k, v)| k == "chatgpt-account-id" && v == "acct"));
    }
    #[test]
    fn antigravity_warmup_caps_output_to_one_token() {
        let member = AccountMember::for_test(ProviderAccount::Antigravity(
            mahoquot_providers::AntigravityAccount {
                project_id: "proj".into(),
                ..Default::default()
            },
        ));
        let (url, body, _) = warmup_request(&member, "discovered-model").unwrap();
        assert!(url.contains("streamGenerateContent"));
        assert_eq!(body["request"]["generationConfig"]["maxOutputTokens"], 1);
        assert_eq!(body["project"], "proj");
    }
    #[test]
    fn every_provider_has_a_warmup_path() {
        for account in [
            ProviderAccount::Codex(Default::default()),
            ProviderAccount::Antigravity(Default::default()),
        ] {
            assert!(
                warmup_request(&AccountMember::for_test(account), "discovered-model").is_some()
            );
        }
    }
    #[test]
    fn warmup_done_only_is_not_success() {
        assert!(!validate(b"data: [DONE]\n\n", "codex"));
        assert!(!validate(b"", "codex"));
    }
    #[tokio::test]
    async fn warmup_idle_interval_and_global_limit() {
        let dir = std::env::temp_dir().join(format!("warmup-clock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(
            AppState::new(&crate::config::GatewayConfig {
                auth_dir: dir.clone(),
                config_path: dir.join("config.yaml"),
                ..Default::default()
            })
            .unwrap(),
        );
        let member = Arc::new(AccountMember::for_test(ProviderAccount::Codex(
            mahoquot_providers::CodexAccount::default(),
        )));
        assert!(!due(&state, &member));
        let ag = AccountMember::for_test(ProviderAccount::Antigravity(
            mahoquot_providers::AntigravityAccount {
                access_token: "mock".into(),
                ..Default::default()
            },
        ));
        let deadline = (now() + 3600) * 1000;
        ag.set_group_cooldown("gemini-test", deadline);
        ag.set_health(Health::Cooldown {
            until_unix_ms: deadline,
        });
        assert_eq!(
            eligibility(&state, &ag, "gemini-test"),
            Some("cooldown_model_quota")
        );
        assert_eq!(eligibility(&state, &ag, "claude-test"), None);
        ag.set_health(Health::AuthFailed);
        assert_eq!(
            eligibility(&state, &ag, "claude-test"),
            Some("account_unavailable")
        );
        ag.set_health(Health::Cooldown {
            until_unix_ms: deadline + 1000,
        });
        assert_eq!(
            eligibility(&state, &ag, "claude-test"),
            Some("account_unavailable")
        );
        state
            .settings
            .mutate(|s| {
                s.warmup.providers.insert(
                    "codex".into(),
                    crate::management::settings::WarmupProviderPolicy {
                        enabled: true,
                        idle_secs: 10,
                        min_interval_secs: 20,
                        model: None,
                    },
                );
            })
            .unwrap();
        assert!(!due(&state, &member));
        let start = tokio::time::Instant::now();
        *member.last_activity.lock().unwrap() = start;
        assert!(!due_at(&state, &member, start + Duration::from_secs(9)));
        assert!(due_at(&state, &member, start + Duration::from_secs(10)));
        state.warmup.history.lock().unwrap().insert(
            member.id.clone(),
            (start, now(), Some(result(&member, "failed"))),
        );
        assert!(!due_at(&state, &member, start + Duration::from_secs(19)));
        assert!(due_at(&state, &member, start + Duration::from_secs(20)));
        let activity = member.begin_activity();
        assert!(!due_at(&state, &member, start + Duration::from_secs(20)));
        drop(activity);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn warmup_valid_completion_and_late_error() {
        let good = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
        assert!(validate(good, "codex"));
        let mut bad = good.to_vec();
        bad.extend_from_slice(b"data: {\"error\":{\"message\":\"failed\"}}\n\n");
        assert!(!validate(&bad, "codex"));
    }
    #[test]
    fn warmup_provider_protocols() {
        assert!(validate(
            br#"{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"STOP"}]}"#,
            "antigravity"
        ));
        assert!(validate(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n","cline"));
    }
}

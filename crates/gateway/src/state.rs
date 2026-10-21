#[cfg(test)]
pub(crate) mod task2_proxy_consumers {
    use super::*;
    use axum::response::IntoResponse;
    use prost::Message;
    use serde_json::json;
    use std::future::Future;
    use std::time::Duration;
    use tokio::sync::{mpsc, Semaphore};

    pub(crate) async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("Task2 event deadline")
    }

    pub(crate) struct Peer {
        pub(crate) url: String,
        pub(crate) seen: mpsc::UnboundedReceiver<String>,
        release: Arc<Semaphore>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Peer {
        pub(crate) async fn start(label: &'static str) -> Self {
            let mut listener = None;
            for port in 18840..=18899 {
                if let Ok(bound) =
                    tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
                {
                    listener = Some(bound);
                    break;
                }
            }
            let listener = listener.expect("reserved loopback port");
            let url = format!("http://{}", listener.local_addr().expect("peer address"));
            let (tx, seen) = mpsc::unbounded_channel();
            let release = Arc::new(Semaphore::new(0));
            let gate = Arc::clone(&release);
            let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
                let tx = tx.clone();
                let gate = Arc::clone(&gate);
                async move {
                    let path = uri.path().to_string();
                    tx.send(path.clone()).expect("capture receiver");
                    if path == "/held" {
                        bounded(gate.acquire()).await.expect("release held request").forget();
                    }
                    match path.as_str() {
                        "/backend-api/wham/usage" => axum::Json(json!({"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"reset_at":1999999999,"limit_window_seconds":18000}}})).into_response(),
                        "/backend-api/codex/responses" => (
                            [("content-type", "text/event-stream")],
                            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"task2\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"task2\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"task2\",\"status\":\"completed\"}}\n\n",
                        ).into_response(),
                        "/refresh" | "/token" => axum::Json(json!({"access_token":format!("token-{label}"),"refresh_token":"dummy-refresh","expires_in":3600,"email":"task2@example.invalid"})).into_response(),
                        "/v1/models" => axum::Json(json!({"data":[{"id":format!("model-{label}")}]})).into_response(),
                        "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs" => {
                            let payload = crate::compat::devin_proto::GetCascadeModelConfigsResponse {
                                client_model_configs: vec![crate::compat::devin_proto::ClientModelConfig {
                                    model_uid: Some(format!("model-{label}")), label: Some(format!("Model {label}")), ..Default::default()
                                }],
                            }.encode_to_vec();
                            ([("content-type", "application/proto")], payload).into_response()
                        }
                        _ => label.into_response(),
                    }
                }
            });
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.expect("peer serve");
            });
            Self {
                url,
                seen,
                release,
                task,
            }
        }

        pub(crate) async fn captured(&mut self, path: &str) {
            assert_eq!(bounded(self.seen.recv()).await.as_deref(), Some(path));
        }

        pub(crate) async fn close(mut self) {
            self.release.add_permits(16);
            self.task.abort();
            // The listener is owned by the aborted task's future; a cancelled
            // join proves that future (and its socket) was dropped. Re-binding
            // the port here raced with concurrent tests claiming the same
            // reserved port from 18840..=18899.
            assert!(bounded(&mut self.task)
                .await
                .expect_err("aborted peer")
                .is_cancelled());
        }
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            self.release.add_permits(16);
            self.task.abort();
        }
    }

    pub(crate) struct Fixture {
        pub(crate) state: Arc<AppState>,
        pub(crate) dir: std::path::PathBuf,
    }

    impl Fixture {
        pub(crate) fn new(target: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("task2-consumers-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("auth dir");
            std::fs::write(dir.join("codex-task2-codex.json"), json!({
                "type":"plus","identity_slug":"task2-codex","account_id":"dummy-account",
                "access_token":"dummy-token","refresh_token":"dummy-refresh","id_token":"dummy-id",
                "email":"task2@example.invalid","expired":"2099-01-01T00:00:00Z","upstream_override":target,"last_refresh":""
            }).to_string()).expect("credential");
            let state = Arc::new(
                AppState::new(&crate::config::GatewayConfig {
                    auth_dir: dir.clone(),
                    config_path: dir.join("config.yaml"),
                    strategy: Strategy::FillFirst,
                    refresh_url: format!("{target}/refresh"),
                    auth_refresh_enabled: false,
                    ..Default::default()
                })
                .expect("state"),
            );
            Self { state, dir }
        }

        pub(crate) fn proxy(&self, url: &str) {
            self.state
                .settings
                .mutate(|settings| settings.proxy_url = url.to_string())
                .expect("publish proxy");
        }

        pub(crate) fn member(&self) -> Arc<AccountMember> {
            self.state
                .find_member("task2-codex")
                .expect("fixture member")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.dir) {
                eprintln!("Task2 fixture cleanup: {error}");
            }
        }
    }

    async fn text(client: reqwest::Client, url: String) -> String {
        let response = bounded(client.get(url).send())
            .await
            .expect("client request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        bounded(response.text()).await.expect("response body")
    }

    #[tokio::test]
    async fn publication_routes_new_families_while_old_client_finishes_on_old_proxy() {
        // Given: capture is subscribed before dispatching a held old-client request.
        let mut a = Peer::start("a").await;
        let mut b = Peer::start("b").await;
        let mut target = Peer::start("target").await;
        let fixture = Fixture::new(&target.url);
        fixture.proxy(&a.url);
        let old = text(fixture.state.http_client(), format!("{}/held", target.url));
        tokio::pin!(old);
        bounded(async {
            tokio::select! {
                result = &mut old => panic!("old completed before release: {result}"),
                event = a.seen.recv() => assert_eq!(event.as_deref(), Some("/held")),
            }
        })
        .await;
        // When: one settings publication changes the global proxy.
        fixture.proxy(&b.url);
        // Then: default/standard/Devin use B, direct uses target, old finishes A.
        for client in [
            fixture.state.http_client(),
            fixture
                .state
                .try_client_for_member(&fixture.member())
                .expect("member"),
            fixture.state.devin_default_client(),
            fixture
                .state
                .devin_client_for_target("dummy-devin")
                .expect("Devin"),
        ] {
            assert_eq!(text(client, format!("{}/probe", target.url)).await, "b");
            b.captured("/probe").await;
        }
        for client in [
            fixture.state.direct_client(),
            fixture.state.devin_direct_client(),
        ] {
            assert_eq!(
                text(client, format!("{}/probe", target.url)).await,
                "target"
            );
            target.captured("/probe").await;
        }
        a.release.add_permits(1);
        assert_eq!(bounded(&mut old).await, "a");
        assert!(a.seen.try_recv().is_err());
        a.close().await;
        b.close().await;
        target.close().await;
    }

    #[tokio::test]
    async fn provider_reload_credential_direct_and_invalid_refusal_keep_wire_policy() {
        // Given: a cached provider A client and a different global B proxy.
        let mut a = Peer::start("a").await;
        let mut b = Peer::start("b").await;
        let mut target = Peer::start("target").await;
        let fixture = Fixture::new(&target.url);
        fixture
            .state
            .settings
            .mutate(|settings| {
                settings.proxy_url = b.url.clone();
                settings.proxy_providers.insert(
                    "codex".into(),
                    crate::management::settings::ProviderProxyPolicy {
                        enabled: true,
                        sticky: false,
                        ttl_secs: 0,
                        url: a.url.clone(),
                    },
                );
            })
            .expect("provider A");
        assert_eq!(
            text(
                fixture
                    .state
                    .try_client_for_member(&fixture.member())
                    .expect("account"),
                format!("{}/probe", target.url)
            )
            .await,
            "a"
        );
        a.captured("/probe").await;
        let held = text(
            fixture
                .state
                .try_client_for_member(&fixture.member())
                .expect("old provider"),
            format!("{}/held", target.url),
        );
        tokio::pin!(held);
        bounded(async {
            tokio::select! {
                result = &mut held => panic!("provider completed before release: {result}"),
                event = a.seen.recv() => assert_eq!(event.as_deref(), Some("/held")),
            }
        })
        .await;
        // When: provider-only settings publication invalidates A.
        fixture
            .state
            .settings
            .mutate(|settings| {
                settings
                    .proxy_providers
                    .get_mut("codex")
                    .expect("policy")
                    .url = b.url.clone()
            })
            .expect("provider B");
        // Then: member uses B, an unselected provider and credential directives bypass.
        assert_eq!(
            text(
                fixture
                    .state
                    .try_client_for_member(&fixture.member())
                    .expect("account"),
                format!("{}/probe", target.url)
            )
            .await,
            "b"
        );
        b.captured("/probe").await;
        a.release.add_permits(1);
        assert_eq!(bounded(&mut held).await, "a");
        assert_eq!(
            text(
                fixture
                    .state
                    .try_client_for_target("unselected", "dummy")
                    .expect("allowlist"),
                format!("{}/probe", target.url)
            )
            .await,
            "target"
        );
        target.captured("/probe").await;
        for directive in ["direct", "none"] {
            let mut member = AccountMember::for_test_with_id(
                "direct",
                crate::account::ProviderAccount::Codex(mahoquot_providers::CodexAccount::default()),
            );
            member.proxy_url = Some(directive.into());
            assert_eq!(
                text(
                    fixture
                        .state
                        .try_client_for_member(&member)
                        .expect("credential direct"),
                    format!("{}/probe", target.url)
                )
                .await,
                "target"
            );
            target.captured("/probe").await;
        }
        let bytes = std::fs::read(fixture.state.settings.path()).expect("settings bytes");
        let snapshot = fixture.state.proxy.load_full();
        assert!(fixture
            .state
            .settings
            .mutate(|settings| settings
                .proxy_providers
                .get_mut("codex")
                .expect("policy")
                .url = "ftp://invalid".into())
            .is_err());
        assert_eq!(
            std::fs::read(fixture.state.settings.path()).expect("settings bytes"),
            bytes
        );
        assert!(Arc::ptr_eq(&snapshot, &fixture.state.proxy.load_full()));
        assert_eq!(
            text(
                fixture
                    .state
                    .try_client_for_member(&fixture.member())
                    .expect("unchanged"),
                format!("{}/probe", target.url)
            )
            .await,
            "b"
        );
        b.captured("/probe").await;
        a.close().await;
        b.close().await;
        target.close().await;
    }

    #[tokio::test]
    async fn quota_warmup_and_member_refresh_follow_publication_and_fail_before_direct_dispatch() {
        let mut a = Peer::start("a").await;
        let mut b = Peer::start("b").await;
        let mut target = Peer::start("target").await;
        let fixture = Fixture::new(&target.url);
        for (proxy, peer, token) in [
            (a.url.clone(), &mut a, "token-a"),
            (b.url.clone(), &mut b, "token-b"),
            ("direct".to_string(), &mut target, "token-target"),
        ] {
            fixture.proxy(&proxy);
            let member = fixture.member();
            bounded(crate::quota::refresh_account_usage(&fixture.state, &member))
                .await
                .expect("quota consumer");
            peer.captured("/backend-api/wham/usage").await;
            assert_eq!(
                member
                    .usage
                    .read()
                    .expect("usage")
                    .refresh_status
                    .as_deref(),
                Some("ok")
            );
            let warm = bounded(crate::warmup::warm_account(&fixture.state, &member)).await;
            assert!(warm.ok && warm.stream_validated, "warmup: {warm:?}");
            peer.captured("/backend-api/codex/responses").await;
            peer.captured("/backend-api/wham/usage").await;
            let previous = member.access_token();
            assert!(
                bounded(fixture.state.refresh_member(&member, Some(&previous)))
                    .await
                    .expect("refresh consumer")
            );
            peer.captured("/refresh").await;
            assert_eq!(member.access_token(), token);
        }
        let path = fixture.dir.join("codex-task2-codex.json");
        let mut credential: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("credential bytes"))
                .expect("credential JSON");
        credential["proxy_url"] = json!("ftp://invalid");
        std::fs::write(path, credential.to_string()).expect("invalid credential proxy");
        fixture.state.rescan_pool().expect("reload credential");
        let member = fixture.member();
        assert!(
            matches!(bounded(crate::quota::refresh_account_usage(&fixture.state, &member)).await,
            Err(crate::quota::QuotaError::Upstream(message)) if message.starts_with("proxy client construction failed:"))
        );
        let warm = bounded(crate::warmup::warm_account(&fixture.state, &member)).await;
        assert!(!warm.ok);
        assert!(warm
            .detail
            .as_deref()
            .expect("warmup construction error")
            .starts_with("proxy client construction failed:"));
        assert!(
            matches!(bounded(fixture.state.refresh_member(&member, Some(&member.access_token()))).await,
            Err(mahoquot_providers::refresh_exec::RefreshError::Parse(message)) if message.starts_with("proxy client construction failed:"))
        );
        assert!(a.seen.try_recv().is_err());
        assert!(b.seen.try_recv().is_err());
        assert!(target.seen.try_recv().is_err());
        a.close().await;
        b.close().await;
        target.close().await;
    }

    #[tokio::test]
    async fn direct_none_and_clear_clients_ignore_a_poisoned_environment_proxy() {
        let mut poisoned = Peer::start("environment").await;
        let mut target = Peer::start("target").await;
        let dir = std::env::temp_dir().join(format!("task2-env-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("subprocess directory");
        let mut child =
            tokio::process::Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "state::task2_proxy_consumers::environment_proxy_child",
                    "--nocapture",
                ])
                .env("TASK2_DIRECT_TARGET", &target.url)
                .env("TASK2_DIRECT_AUTH", &dir)
                .env("HTTP_PROXY", &poisoned.url)
                .env("HTTPS_PROXY", &poisoned.url)
                .env("http_proxy", &poisoned.url)
                .env("https_proxy", &poisoned.url)
                .env("ALL_PROXY", &poisoned.url)
                .env("all_proxy", &poisoned.url)
                .env("NO_PROXY", "")
                .env("no_proxy", "")
                .kill_on_drop(true)
                .spawn()
                .expect("isolated environment child");
        let status = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
            Ok(result) => result.expect("child exit"),
            Err(error) => {
                child.start_kill().expect("kill timed out child");
                bounded(child.wait()).await.expect("reap timed out child");
                panic!("environment child deadline: {error}");
            }
        };
        assert!(status.success(), "environment child status: {status}");
        for _ in 0..18 {
            target.captured("/probe").await;
        }
        assert!(poisoned.seen.try_recv().is_err());
        poisoned.close().await;
        target.close().await;
        std::fs::remove_dir_all(dir).expect("child fixture cleanup");
    }

    #[tokio::test]
    async fn environment_proxy_child() {
        let Ok(target) = std::env::var("TASK2_DIRECT_TARGET") else {
            return;
        };
        let dir = std::path::PathBuf::from(std::env::var("TASK2_DIRECT_AUTH").expect("child auth"));
        let state = AppState::new(&crate::config::GatewayConfig {
            auth_dir: dir.clone(),
            config_path: dir.join("config.yaml"),
            strategy: Strategy::FillFirst,
            auth_refresh_enabled: false,
            ..Default::default()
        })
        .expect("child state");
        for directive in ["direct", "none", ""] {
            state
                .settings
                .mutate(|settings| settings.proxy_url = directive.into())
                .expect("directive");
            for client in [
                state.http_client(),
                state.direct_client(),
                state.devin_default_client(),
                state.devin_direct_client(),
                state
                    .try_client_for_target("codex", "dummy")
                    .expect("standard"),
                state.devin_client_for_target("dummy").expect("Devin"),
            ] {
                assert_eq!(text(client, format!("{target}/probe")).await, "target");
            }
        }
    }
}

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mahoquot_router::Router;
use mahoquot_types::{Health, PoolMember, Strategy};

use crate::account::{load_account_members, AccountMember};
use crate::config::GatewayConfig;
use crate::inbound::ApiKeys;
use crate::management::observability::LogTail;
use crate::management::settings::ScopedApiKey;
use crate::management::store::SettingsStore;
use crate::metrics::{AdminStatsResponse, GatewayMetrics};
use crate::monitor::MonitorState;
pub use crate::runtime_state::{
    PoolSnapshot, RefreshCoordinator, RuntimeComposition, RuntimeState, UnifiedRuntimeState,
};
use crate::telemetry::TelemetryStore;

/// One scoped key's live state: the immutable published definition plus the
/// counter that moves on every request.
///
/// Token usage is a plain atomic rather than a field inside the settings
/// document because the relay updates it per request; routing every increment
/// through `SettingsStore::mutate` would put a disk write and a global mutex on
/// the hot path.
#[derive(Debug)]
pub struct ScopedKeyEntry {
    pub key: Arc<ScopedApiKey>,
    token_used: Arc<AtomicU64>,
}

impl ScopedKeyEntry {
    fn new(key: ScopedApiKey) -> Self {
        let token_used = Arc::new(AtomicU64::new(key.token_used));
        Self {
            key: Arc::new(key),
            token_used,
        }
    }

    pub fn token_used(&self) -> u64 {
        self.token_used.load(Ordering::Relaxed)
    }

    pub fn token_limit(&self) -> u64 {
        self.key.token_limit
    }

    /// A zero limit means unlimited, matching the settings default for a key
    /// minted without a cap.
    pub fn is_exhausted(&self) -> bool {
        self.key.token_limit > 0 && self.token_used() >= self.key.token_limit
    }

    /// Charge `tokens` against the key and report the new total. Saturating so a
    /// pathological usage report cannot wrap the counter back under the limit.
    pub fn consume(&self, tokens: u64) -> u64 {
        let mut current = self.token_used.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(tokens);
            match self.token_used.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next,
                Err(observed) => current = observed,
            }
        }
    }

    pub fn is_usable_at(&self, now_ms: i64) -> bool {
        self.key.is_usable_at(now_ms)
    }
}

/// Lock-free index of scoped inbound keys, keyed by the one-way key identifier.
///
/// Lookups on the request path are a single `ArcSwap` load plus a hash probe;
/// writers publish a whole new map, so an authenticating request never blocks
/// behind a key edit. Counters survive a republish: an identifier still present
/// after reconciliation keeps its live `token_used`.
#[derive(Debug, Default)]
pub struct ScopedKeyTracker {
    entries: arc_swap::ArcSwap<std::collections::HashMap<String, Arc<ScopedKeyEntry>>>,
}

impl ScopedKeyTracker {
    pub fn new(keys: &[ScopedApiKey]) -> Self {
        let tracker = Self::default();
        tracker.reconcile(keys);
        tracker
    }

    /// Republish the index from the settings document, carrying live usage
    /// counters across for keys that still exist. A key whose persisted
    /// `token_used` has moved ahead of the in-memory counter (an operator reset
    /// or an external edit) adopts the larger of the two, so a restart or a
    /// manual bump can never hand back already-spent allowance.
    pub fn reconcile(&self, keys: &[ScopedApiKey]) {
        let previous = self.entries.load();
        let mut next = std::collections::HashMap::with_capacity(keys.len());
        for key in keys {
            let mut entry = ScopedKeyEntry::new(key.clone());
            if let Some(existing) = previous.values().find(|entry| entry.key.id == key.id) {
                entry.token_used = Arc::clone(&existing.token_used);
                entry
                    .token_used
                    .fetch_max(key.token_used, Ordering::Relaxed);
            }
            next.insert(key.key_identifier.clone(), Arc::new(entry));
        }
        self.entries.store(Arc::new(next));
    }

    /// O(1) lookup by the presented key's stable identifier.
    pub fn get(&self, key_identifier: &str) -> Option<Arc<ScopedKeyEntry>> {
        self.entries.load().get(key_identifier).cloned()
    }

    /// Resolve a raw presented key to its scoped entry, if any.
    pub fn lookup_raw(&self, presented: &str) -> Option<Arc<ScopedKeyEntry>> {
        self.get(&crate::request_history::stable_key_identifier(presented))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.load().is_empty()
    }

    /// Charge a request's tokens to a scoped key and report the key's new
    /// total. `None` when the identifier belongs to a master key (or to no key
    /// at all), which is the common case and costs one hash probe.
    pub fn record_usage(&self, key_identifier: Option<&str>, tokens: u64) -> Option<u64> {
        let entry = key_identifier.and_then(|id| self.get(id))?;
        if tokens == 0 {
            return Some(entry.token_used());
        }
        Some(entry.consume(tokens))
    }

    /// Snapshot of live counters, for reporting and for persisting usage back
    /// into the settings document.
    pub fn usage_snapshot(&self) -> Vec<(String, u64)> {
        self.entries
            .load()
            .iter()
            .map(|(identifier, entry)| (identifier.clone(), entry.token_used()))
            .collect()
    }
}

/// Default ceiling on concurrent upstream inference requests. The permit is
/// owned by the downstream response body, so it is released when the body
/// finishes loading or the client disconnects — never at the headers.
///
/// This is an overload ceiling (immediate retryable 503 past it), not a
/// request-size limit: the 512 MiB body cap is untouched. It sits deliberately
/// far above real concurrency, so an ordinary burst of a hundred concurrent
/// requests is normal traffic rather than overload. Override it with the
/// MAX_CONCURRENT_INFERENCE environment variable when a deployment needs a
/// different ceiling.
pub const MAX_CONCURRENT_INFERENCE_REQUESTS: usize = 512;

/// Guarded per-account usage-poll backoff.
///
/// The live-id allowlist and the backoff deadlines share one lock, so a
/// `set_poll_backoff` for an account that already left the pool is dropped
/// instead of resurrecting its entry after a reconcile. `lock()` exposes the
/// small subset of `HashMap` operations the quota poller uses, so the existing
/// `quota.rs` call sites keep compiling against the new field type.
#[derive(Debug, Default)]
pub struct PollBackoffStore {
    inner: std::sync::Mutex<PollBackoffInner>,
}

#[derive(Debug, Default)]
struct PollBackoffInner {
    /// Live pool ids; `None` until `retain_accounts` activates the allowlist,
    /// so a standalone store accepts every write until membership is explicit.
    active: Option<std::collections::BTreeSet<String>>,
    entries: std::collections::HashMap<String, i64>,
}

#[derive(Debug)]
pub struct PollBackoffGuard<'a> {
    inner: std::sync::MutexGuard<'a, PollBackoffInner>,
}

impl PollBackoffGuard<'_> {
    pub fn set(&mut self, account: &str, until_unix: i64) -> bool {
        if self
            .inner
            .active
            .as_ref()
            .is_some_and(|active| !active.contains(account))
        {
            return false;
        }
        self.inner.entries.insert(account.to_string(), until_unix);
        true
    }

    pub fn get(&self, account: &str) -> Option<&i64> {
        self.inner.entries.get(account)
    }

    /// Inserts only while the account is live: the membership check and the
    /// insert share this guard, so a reconcile cannot be interleaved.
    pub fn insert(&mut self, account: String, until_unix: i64) -> Option<i64> {
        if self
            .inner
            .active
            .as_ref()
            .is_some_and(|active| !active.contains(&account))
        {
            return None;
        }
        self.inner.entries.insert(account, until_unix)
    }

    pub fn remove(&mut self, account: &str) -> Option<i64> {
        self.inner.entries.remove(account)
    }

    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }

    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&String, &mut i64) -> bool,
    {
        self.inner
            .entries
            .retain(|account, until| f(account, until));
    }
}

impl PollBackoffStore {
    pub fn lock(&self) -> std::sync::LockResult<PollBackoffGuard<'_>> {
        match self.inner.lock() {
            Ok(inner) => Ok(PollBackoffGuard { inner }),
            Err(poisoned) => Err(std::sync::PoisonError::new(PollBackoffGuard {
                inner: poisoned.into_inner(),
            })),
        }
    }

    /// Publishes the live-id allowlist and drops the backoff of every account
    /// that left the pool, all under one guard. Returns how many were pruned.
    pub fn retain_accounts(&self, active: &std::collections::BTreeSet<String>) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.active = Some(active.clone());
        let before = inner.entries.len();
        inner.entries.retain(|id, _| active.contains(id));
        before - inner.entries.len()
    }

    pub fn tracked_accounts(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut ids: Vec<String> = inner.entries.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Guarded proxy-client cache.
///
/// A sticky TTL proxy mints a new upstream URL every rotation bucket, so the
/// cached clients are keyed by that URL. The cache keeps only the newest URL
/// per provider+member, and the live-id allowlist shares the same lock as the
/// map, so a request that finishes after its account was removed cannot re-cache
/// a client for the departed member.
#[derive(Debug, Default)]
pub struct ProxyClientCache {
    inner: std::sync::Mutex<ProxyClientCacheInner>,
}

#[derive(Debug, Default)]
struct ProxyClientCacheInner {
    /// Live pool ids; `None` until `retain_members` activates the allowlist.
    active: Option<std::collections::BTreeSet<String>>,
    generation: u64,
    clients: std::collections::HashMap<String, reqwest::Client>,
}

impl ProxyClientCacheInner {
    fn admits(&self, member_id: &str) -> bool {
        self.active
            .as_ref()
            .is_none_or(|active| active.contains(member_id))
    }
}

/// The member id segment of a `{provider}|{member_id}|{url}` cache key.
fn proxy_client_owner(key: &str) -> Option<&str> {
    key.split('|').nth(1)
}

impl ProxyClientCache {
    pub fn get(&self, key: &str) -> Option<reqwest::Client> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clients
            .get(key)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clients
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn generation(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.generation = inner.generation.wrapping_add(1);
        inner.clients.clear();
    }

    /// Publishes the live-id allowlist and drops every cached client whose
    /// member left the pool, all under one guard. Returns the number pruned.
    pub fn retain_members(&self, active: &std::collections::BTreeSet<String>) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.active = Some(active.clone());
        let before = inner.clients.len();
        inner
            .clients
            .retain(|key, _| proxy_client_owner(key).is_some_and(|id| active.contains(id)));
        before - inner.clients.len()
    }

    /// Replaces the member's previous URL client with `client`, unless the
    /// member is no longer live (in which case the fresh client is used for the
    /// request that built it but is never cached).
    pub fn replace_scoped(
        &self,
        key: String,
        scope: &str,
        member_id: &str,
        client: reqwest::Client,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if !inner.admits(member_id) {
            return;
        }
        inner
            .clients
            .retain(|existing, _| existing == &key || !existing.starts_with(scope));
        inner.clients.insert(key, client);
    }

    pub fn replace_scoped_generation(
        &self,
        generation: u64,
        key: String,
        scope: &str,
        member_id: &str,
        client: reqwest::Client,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.generation != generation || !inner.admits(member_id) {
            return;
        }
        inner
            .clients
            .retain(|existing, _| existing == &key || !existing.starts_with(scope));
        inner.clients.insert(key, client);
    }
}

/// Single-flight claim for one account's Devin catalog discovery. Dropping the
/// guard releases the claim, so a cancelled or failed refresh never wedges the
/// account's discovery.
pub struct DevinRefreshGuard {
    gate: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    id: String,
}

impl Drop for DevinRefreshGuard {
    fn drop(&mut self) {
        let mut set = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        set.remove(&self.id);
    }
}

pub struct AppState {
    pub warmup: crate::warmup::WarmupRunner,
    pub router: Router,
    pub pool: Arc<arc_swap::ArcSwap<PoolSnapshot>>,
    pub runtime: Arc<UnifiedRuntimeState>,
    pub catalog: Arc<crate::registry::CatalogManager>,
    pub models_env: Option<String>,
    pub proxy: Arc<arc_swap::ArcSwap<crate::proxy_policy::ProxySnapshot>>,
    pub proxy_clients: Arc<ProxyClientCache>,
    pub metrics: Arc<GatewayMetrics>,
    pub monitor: Arc<MonitorState>,
    pub api_keys: Arc<ApiKeys>,
    /// In-memory scoped-key index: O(1), lock-free authentication and token
    /// accounting for delegated inbound keys.
    pub scoped_keys: Arc<ScopedKeyTracker>,
    pub refresh_url: String,
    pub auth_refresh_enabled: bool,
    /// Captcha scene endpoint + solver sidecar overrides for the zcode plan
    /// gateway (config/env-resolved at startup; tests inject directly).
    pub captcha_config_url: Option<String>,
    pub captcha_solver_bin: Option<PathBuf>,
    /// Verify params are single-use — challenge solves serialize on this gate
    /// (per-state, never a global static).
    pub captcha_solve_gate: tokio::sync::Mutex<()>,
    pub refreshed: AtomicU64,
    pub max_failover: usize,
    pub model_restrictions: AtomicBool,
    pub settings: Arc<SettingsStore>,
    pub scheduler: crate::scheduler::SchedulerRegistry,
    pub claude_reset_order: crate::claude_reset_order::ClaudeResetOrder,
    pub history: crate::request_history::HistoryService,
    pub signature_ledger: Arc<crate::compat::signature_ledger::SignatureLedger>,
    pub telemetry: Arc<TelemetryStore>,
    /// Live in-memory log tail, always fed regardless of `logging-to-file`.
    pub log_tail: LogTail,
    pub usage_samples: crate::usage::UsageSampleStore,
    pub usage_state: crate::usage::UsageStateStore,
    pub shutdown: Arc<tokio::sync::Notify>,
    /// Per-account usage-poll backoff (unix secs). A 429 from a usage endpoint
    /// parks the account here so the poller stops keeping the throttle hot.
    /// The live-id allowlist shares this guard (see `PollBackoffStore`).
    pub usage_poll_backoff: PollBackoffStore,
    /// Devin discovery single-flight claims, taken before a refresh task is
    /// spawned.
    pub devin_refresh_in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    pub devin_cache: Arc<crate::devin_catalog::DevinDiscoveryCache>,
    /// Admission ceiling for upstream inference; see
    /// `MAX_CONCURRENT_INFERENCE_REQUESTS`.
    pub inference_gate: Arc<tokio::sync::Semaphore>,
    /// Serializes membership application for `rescan_pool`.
    rescan_gate: std::sync::Mutex<()>,
    pub finalizer_notifiers: Arc<
        std::sync::Mutex<std::collections::HashMap<String, Vec<tokio::sync::mpsc::Sender<()>>>>,
    >,
    pub aistudio: Arc<crate::aistudio::Relay>,
    pub plugins: Option<Arc<crate::plugins::loader::PluginHost>>,
    pub home: Option<Arc<crate::home::HomeClient>>,
    pub realtime: Arc<crate::realtime::RealtimeStore>,
    /// xAI encrypted-reasoning replay, scoped per caller/account/model/session.
    pub xai_replay: Arc<std::sync::Mutex<crate::compat::xai::ReasoningReplay>>,
    /// Kimi signed-thinking replay for Claude-format turns (Task 17).
    pub kimi_replay: Arc<std::sync::Mutex<crate::compat::kimi::ReplayCache>>,
    /// Opt-in LCP trajectory matcher (Task 13), bounded by routing settings.
    pub lcp: std::sync::Mutex<mahoquot_router::lcp::Matcher>,
    /// Explicit/inherited session bindings with wall-clock TTL (Task 13).
    pub session_bindings: std::sync::Mutex<std::collections::HashMap<String, (String, i64)>>,
}

pub(crate) fn adopt_runtime_state(target: &AccountMember, previous: &Arc<AccountMember>) {
    // Only adopt runtime state if target represents the exact same credential identity.
    // True credential replacement (token change, endpoint change, or credential recreation)
    // creates a clean break, leaving target with fresh, distinct runtime cells.
    if !target.same_credential_identity(previous) {
        return;
    }

    let seq = std::sync::atomic::Ordering::Relaxed;
    let target_health = target.health();
    if target_health != Health::Disabled {
        let prev_health = previous.health();
        if prev_health != Health::Disabled {
            *target
                .health
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = prev_health;
        }
    }
    *target
        .usage
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous
        .usage
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    *target
        .unsupported_models
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous
        .unsupported_models
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    // Per-group cooldowns survive a reload for the same reason account-wide
    // health does: a reload must not hand back an allowance upstream is still
    // rate-limiting.
    *target
        .group_cooldowns
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous
        .group_cooldowns
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    target.ok_count.store(previous.ok_count.load(seq), seq);
    *target.last_activity.lock().unwrap() = *previous.last_activity.lock().unwrap();
    target.fail_count.store(previous.fail_count.load(seq), seq);
    if target.kind() == crate::account::ProviderKind::Devin
        && previous.kind() == crate::account::ProviderKind::Devin
    {
        target
            .devin_catalog
            .store(previous.devin_catalog.load_full());
    }
}

impl AppState {
    pub fn new(config: &GatewayConfig) -> anyhow::Result<Self> {
        let router = Router::new(config.strategy);
        let metrics = Arc::new(GatewayMetrics::default());
        let monitor = Arc::new(MonitorState::default());
        let refresh_url = config.refresh_url.clone();
        let auth_refresh_enabled = config.auth_refresh_enabled;
        let captcha_config_url = config.captcha_config_url.clone();
        let captcha_solver_bin = config.captcha_solver_bin.clone();
        let captcha_solve_gate = tokio::sync::Mutex::new(());
        let settings = Arc::new(SettingsStore::load_or(
            config.config_path.clone(),
            config.as_settings(),
        )?);

        let initial_strategy = match settings.current().routing.strategy.as_str() {
            "fill-first" => Strategy::FillFirst,
            "round-robin" => Strategy::StrictRoundRobin,
            other => crate::management::scalar_table::parse_routing_strategy(other)
                .unwrap_or(config.strategy),
        };
        router.set_strategy(initial_strategy);

        let initial_settings = settings.current();
        let mut members = load_account_members(std::path::Path::new(&initial_settings.auth_dir))?;
        for member in &mut members {
            Arc::get_mut(member)
                .ok_or_else(|| anyhow::anyhow!("freshly loaded account unexpectedly shared"))?
                .apply_config_retry(&initial_settings);
        }
        let initial_proxy_snapshot =
            crate::proxy_policy::ProxySnapshot::from_settings(&initial_settings)
                .map_err(|e| anyhow::anyhow!("failed to build initial proxy snapshot: {}", e))?;
        let proxy = Arc::new(arc_swap::ArcSwap::from_pointee(initial_proxy_snapshot));
        let proxy_clients = Arc::new(ProxyClientCache::default());

        let proxy_pub = Arc::clone(&proxy);
        let proxy_clients_pub = Arc::clone(&proxy_clients);
        settings.set_proxy_publisher(Arc::new(move |new_snapshot| {
            proxy_pub.store(new_snapshot);
            proxy_clients_pub.clear();
        }));
        let scoped_keys = Arc::new(ScopedKeyTracker::new(&settings.current().scoped_api_keys));
        // Every published settings document rebuilds the index, so a key that
        // is revoked or re-scoped through the management API takes effect on
        // the next request without a restart.
        let scoped_keys_for_observer = Arc::clone(&scoped_keys);
        settings.add_observer(Arc::new(move |published| {
            scoped_keys_for_observer.reconcile(&published.scoped_api_keys);
        }));
        let api_keys = Arc::new(ApiKeys::with_live_settings(
            Arc::clone(&settings),
            config.api_keys.clone(),
        ));
        let scheduler = crate::scheduler::SchedulerRegistry::load(&config.config_path, &members);
        let history = crate::request_history::HistoryService::open(
            &config.config_path.with_file_name("request-history.sqlite"),
            config.history_queue_capacity,
            config.history_batch_size,
            Arc::clone(&metrics),
        );
        let signature_ledger = crate::compat::signature_ledger::SignatureLedger::open(
            config
                .auth_dir
                .join(crate::compat::signature_ledger::DEFAULT_SNAPSHOT_FILE),
        );
        if let Ok(store) = history.store() {
            if let Err(error) = store.prune(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
                    .unwrap_or(0),
            ) {
                tracing::warn!(%error, "failed to prune request history on startup");
            }
        }
        let telemetry = Arc::new(TelemetryStore::load(
            config.config_path.with_file_name("telemetry.json"),
        ));
        let usage_state = crate::usage::UsageStateStore::load(
            config.config_path.with_file_name("usage-state.json"),
        );
        // Restore the last observed quota snapshots so the console shows real
        // windows right after a restart instead of waiting for the first poll.
        let restored_usage = usage_state.restore();
        for member in &members {
            if let Some(snapshot) = restored_usage.get(&member.id) {
                let mut usage = snapshot.clone();
                if member.provider_name() == "cline" {
                    let now_unix = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    usage.expire_stale_cline_limits(now_unix);
                }
                member.set_usage(usage);
            }
        }

        let catalog_settings = settings.current().model_catalog.clone();
        let catalog_config = crate::registry::CatalogConfig {
            cache_path: config.catalog_cache_path.clone(),
            remote_catalog_url: catalog_settings.as_ref().map(|catalog| catalog.url.clone()),
            remote_signature_url: catalog_settings
                .as_ref()
                .map(|catalog| catalog.signature_url.clone()),
            ..crate::registry::CatalogConfig::default()
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let catalog = Arc::new(crate::registry::CatalogManager::boot_with_metrics(
            catalog_config,
            now,
            Arc::clone(&metrics),
        ));
        catalog.bind_proxy(Arc::clone(&proxy));
        let base_registry = catalog.current_snapshot();
        let initial_registry = settings
            .current()
            .validate_against_registry(&base_registry)
            .map(Arc::new)
            .unwrap_or(base_registry);
        let candidate = crate::runtime_state::compute_candidate_composition_with_created(
            1,
            members,
            initial_registry,
            config.models_env.as_deref(),
            match catalog.active_created_unix() {
                0 => now as i64,
                created => created as i64,
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to compute initial composition: {e}"))?;

        let runtime = Arc::new(UnifiedRuntimeState::new(
            candidate,
            config.models_env.clone(),
        ));
        let pool = runtime.pool();
        catalog.bind_runtime(&runtime);
        catalog.bind_settings(&settings);

        let catalog_for_snapshot = Arc::clone(&catalog);
        settings.set_snapshot_provider(Arc::new(move || catalog_for_snapshot.raw_snapshot()));
        let runtime_for_publisher = Arc::clone(&runtime);
        settings.set_pool_publisher(Arc::new(move |registry| {
            runtime_for_publisher.update_registry(registry).map(|_| ())
        }));
        let runtime_for_retry = Arc::clone(&runtime);
        settings.add_observer(Arc::new(move |published| {
            let current = runtime_for_retry.pool().load_full();
            let members = current
                .members
                .iter()
                .map(|member| {
                    let mut updated = member.clone_for_snapshot(member.devin_catalog_state());
                    updated.apply_config_retry(published);
                    Arc::new(updated)
                })
                .collect();
            if let Err(error) = runtime_for_retry.reload_accounts(members) {
                tracing::error!(
                    "failed to publish retry configuration for active members: {error}"
                );
            }
        }));

        let state = Self {
            settings,
            scheduler,
            claude_reset_order: crate::claude_reset_order::ClaudeResetOrder::default(),
            history,
            signature_ledger,
            telemetry,
            log_tail: LogTail::default(),
            usage_samples: crate::usage::UsageSampleStore::load(
                config.config_path.with_file_name("usage-samples.json"),
            ),
            usage_state,
            shutdown: Arc::new(tokio::sync::Notify::new()),
            usage_poll_backoff: PollBackoffStore::default(),
            devin_refresh_in_flight: Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            inference_gate: Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_inference)),
            rescan_gate: std::sync::Mutex::new(()),
            router,
            runtime,
            catalog,
            warmup: crate::warmup::WarmupRunner::default(),
            pool,
            models_env: config.models_env.clone(),
            proxy,
            proxy_clients,
            metrics,
            monitor,
            api_keys,
            scoped_keys,
            refresh_url,
            auth_refresh_enabled,
            captcha_config_url,
            captcha_solver_bin,
            captcha_solve_gate,
            refreshed: AtomicU64::new(0),
            max_failover: if config.max_failover == 0 {
                3
            } else {
                config.max_failover
            },
            model_restrictions: AtomicBool::new(false),
            devin_cache: Arc::new(crate::devin_catalog::DevinDiscoveryCache::new()),
            finalizer_notifiers: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            aistudio: Arc::new(crate::aistudio::Relay::new()),
            plugins: None,
            home: None,
            realtime: Arc::new(crate::realtime::RealtimeStore::default()),
            xai_replay: Arc::new(std::sync::Mutex::new(Default::default())),
            kimi_replay: Arc::new(std::sync::Mutex::new(Default::default())),
            lcp: std::sync::Mutex::new(Default::default()),
            session_bindings: std::sync::Mutex::new(Default::default()),
        };
        // Bootstrap membership: every store that can outlive an account starts
        // with the initial pool's allowlist, so no later writer can recreate an
        // id the pool never had.
        state.reconcile_account_state();
        Ok(state)
    }

    pub fn set_routing_strategy(&self, strategy: mahoquot_types::Strategy) {
        self.router.set_strategy(strategy);
    }

    /// Re-applies the published `routing.strategy` to the live router. Every
    /// whole-document write (v0 config.yaml, v8 config, Home snapshot) calls
    /// this so the router never keeps a strategy the settings no longer hold.
    pub fn sync_routing_strategy(&self) {
        let current = self.settings.current();
        let strategy = match current.routing.strategy.as_str() {
            "fill-first" | "" => Some(mahoquot_types::Strategy::FillFirst),
            "round-robin" => Some(mahoquot_types::Strategy::StrictRoundRobin),
            other => crate::management::scalar_table::parse_routing_strategy(other),
        };
        if let Some(strategy) = strategy {
            self.router.set_strategy(strategy);
        }
    }

    pub fn find_member(&self, id: &str) -> Option<Arc<AccountMember>> {
        self.pool
            .load()
            .members
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    /// Rebuilds the pool from the auth directory so credentials written after
    /// startup (imports, OAuth onboarding) become live without a restart.
    pub fn rescan_pool(&self) -> anyhow::Result<usize> {
        // Serialize the entire load+apply sequence. Two overlapping rescans
        // must never let the older membership land after the newer one and
        // resurrect accounts the pool has already dropped.
        let _gate = self
            .rescan_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let auth_dir = self.settings.current().auth_dir.clone();
        let mut members = load_account_members(std::path::Path::new(&auth_dir))?;
        for member in &mut members {
            Arc::get_mut(member)
                .ok_or_else(|| anyhow::anyhow!("freshly loaded account unexpectedly shared"))?
                .apply_config_retry(&self.settings.current());
        }
        // Surviving accounts keep their runtime state (health, counters, cached
        // usage): reloading them fresh would wipe cooldowns and quota caches on
        // every import or delete.
        let previous: std::collections::BTreeMap<String, Arc<AccountMember>> = self
            .pool
            .load()
            .members
            .iter()
            .map(|m| (m.id.clone(), m.clone()))
            .collect();
        let members: Vec<Arc<AccountMember>> = members
            .into_iter()
            .map(|mut m| match previous.get(&m.id) {
                Some(previous_member) => {
                    // fresh parse wins (new tokens/project), runtime state transfers
                    adopt_runtime_state(&m, previous_member);
                    if m.same_credential_identity(previous_member) {
                        let fresh = Arc::get_mut(&mut m).expect("freshly loaded account");
                        fresh.last_activity = previous_member.last_activity.clone();
                        fresh.active_requests = previous_member.active_requests.clone();
                        fresh.refresh_lock = Arc::clone(&previous_member.refresh_lock);
                    }
                    m
                }
                None => m,
            })
            .collect();
        let count = members.len();
        let new_snapshot = self.runtime.reload_accounts(members)?;
        self.scheduler.reconcile(&new_snapshot.members);
        self.reconcile_account_state();
        Ok(count)
    }

    /// Publishes the live pool ids into every store that can be written by a
    /// task outliving an account. This is a fan-out of `retain_accounts`; it
    /// never performs a membership check on a writer's behalf — each store
    /// keeps its allowlist inside the same guard as its data.
    pub fn reconcile_account_state(&self) {
        let ids: std::collections::BTreeSet<String> = self
            .pool
            .load()
            .members
            .iter()
            .map(|member| member.id.clone())
            .collect();
        let _ = self.usage_samples.retain_accounts(&ids);
        let _ = self.monitor.retain_accounts(&ids);
        let _ = self.warmup.retain_accounts(&ids);
        let _ = self.router.retain_members(&ids);
        let _ = self.devin_cache.retain_accounts(&ids);
        let _ = self.scheduler.retain_accounts(&ids);
        let _ = self.usage_poll_backoff.retain_accounts(&ids);
        let _ = self.proxy_clients.retain_members(&ids);
    }

    /// Authorization check for management input (scheduler reserve), not a
    /// state-write gate.
    pub fn is_active_account(&self, id: &str) -> bool {
        self.pool
            .load()
            .members
            .iter()
            .any(|member| member.id == id)
    }

    /// Admittance gate for upstream inference requests.
    pub fn inference_gate(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.inference_gate)
    }

    /// Number of cached per-target proxy clients (diagnostic; the retained set
    /// is bounded to the latest URL per provider+member).
    pub fn proxy_client_count(&self) -> usize {
        self.proxy_clients.len()
    }

    /// Claims the single-flight slot for an account's Devin discovery. Returns
    /// `None` when a refresh is already in flight, so a burst of stale requests
    /// collapses onto the worker that is already running.
    pub fn begin_devin_refresh(&self, id: &str) -> Option<DevinRefreshGuard> {
        let mut in_flight = self
            .devin_refresh_in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !in_flight.insert(id.to_string()) {
            return None;
        }
        Some(DevinRefreshGuard {
            gate: Arc::clone(&self.devin_refresh_in_flight),
            id: id.to_string(),
        })
    }

    pub fn devin_refresh_in_flight_count(&self) -> usize {
        self.devin_refresh_in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    pub fn runtime_state(&self) -> Arc<UnifiedRuntimeState> {
        Arc::clone(&self.runtime)
    }

    pub fn composition(&self) -> Arc<PoolSnapshot> {
        self.runtime.composition()
    }

    pub fn force_health(&self, id: &str, health: Health) {
        if let Some(m) = self.find_member(id) {
            m.set_health(health);
        }
    }

    pub async fn refresh_member(
        &self,
        member: &AccountMember,
        presented_token: Option<&str>,
    ) -> Result<bool, mahoquot_providers::refresh_exec::RefreshError> {
        let client = self.try_client_for_member(member).map_err(|error| {
            mahoquot_providers::refresh_exec::RefreshError::Parse(format!(
                "proxy client construction failed: {error}"
            ))
        })?;
        let did_refresh = member
            .refresh(&client, &self.refresh_url, presented_token)
            .await?;
        if did_refresh {
            self.refreshed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(did_refresh)
    }

    pub fn http_client(&self) -> reqwest::Client {
        self.default_client()
    }

    pub fn default_client(&self) -> reqwest::Client {
        self.proxy.load().default_client.clone()
    }

    pub fn direct_client(&self) -> reqwest::Client {
        self.proxy.load().direct_client.clone()
    }

    pub fn devin_default_client(&self) -> reqwest::Client {
        self.proxy.load().devin_default_client.clone()
    }

    pub fn devin_direct_client(&self) -> reqwest::Client {
        self.proxy.load().devin_direct_client.clone()
    }

    pub fn client_for_member(&self, member: &AccountMember) -> reqwest::Client {
        self.try_client_for_member(member)
            .unwrap_or_else(|_| self.direct_client())
    }

    pub fn try_client_for_member(
        &self,
        member: &AccountMember,
    ) -> Result<reqwest::Client, crate::proxy_policy::ProxyClientBuildError> {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.try_client_for_member_at(member, now_unix)
    }

    pub fn try_client_for_member_at(
        &self,
        member: &AccountMember,
        now_unix: u64,
    ) -> Result<reqwest::Client, crate::proxy_policy::ProxyClientBuildError> {
        self.try_client_for_target_at_with_override(
            &member.provider_name(),
            member.id(),
            member.proxy_url.as_deref(),
            now_unix,
        )
    }

    pub fn client_for_target(&self, provider_name: &str, member_id: &str) -> reqwest::Client {
        self.try_client_for_target(provider_name, member_id)
            .unwrap_or_else(|_| self.direct_client())
    }

    pub fn try_client_for_target(
        &self,
        provider_name: &str,
        member_id: &str,
    ) -> Result<reqwest::Client, crate::proxy_policy::ProxyClientBuildError> {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.try_client_for_target_at(provider_name, member_id, now_unix)
    }

    /// Time-injected core of `client_for_target`.
    #[allow(dead_code)]
    pub(crate) fn client_for_target_at(
        &self,
        provider_name: &str,
        member_id: &str,
        now_unix: u64,
    ) -> reqwest::Client {
        self.try_client_for_target_at(provider_name, member_id, now_unix)
            .unwrap_or_else(|_| self.direct_client())
    }

    pub(crate) fn try_client_for_target_at(
        &self,
        provider_name: &str,
        member_id: &str,
        now_unix: u64,
    ) -> Result<reqwest::Client, crate::proxy_policy::ProxyClientBuildError> {
        self.try_client_for_target_at_with_override(provider_name, member_id, None, now_unix)
    }

    pub(crate) fn try_client_for_target_at_with_override(
        &self,
        provider_name: &str,
        member_id: &str,
        credential_proxy: Option<&str>,
        now_unix: u64,
    ) -> Result<reqwest::Client, crate::proxy_policy::ProxyClientBuildError> {
        let snapshot = self.proxy.load();
        let runtime = &snapshot.runtime;
        let cache_gen = self.proxy_clients.generation();

        let effective_proxy_url = if let Some(cred_url) = credential_proxy {
            let trimmed = cred_url.trim();
            if !trimmed.is_empty() {
                if crate::proxy_policy::is_direct_or_none(trimmed) {
                    Some("direct".to_string())
                } else {
                    Some(crate::proxy_policy::format_session_proxy_url(
                        trimmed, member_id, false, 0, now_unix,
                    ))
                }
            } else {
                runtime.session_proxy_url(provider_name, member_id, now_unix)
            }
        } else {
            runtime.session_proxy_url(provider_name, member_id, now_unix)
        };

        let Some(proxy_url) = effective_proxy_url else {
            // Toggling a provider on at runtime must not retroactively proxy
            // the providers that were never opted in.
            return Ok(if runtime.scoped_routing_active() {
                snapshot.direct_client.clone()
            } else {
                snapshot.default_client.clone()
            });
        };

        if crate::proxy_policy::is_direct_or_none(&proxy_url) {
            return Ok(snapshot.direct_client.clone());
        }

        let cache_key = format!("{provider_name}|{member_id}|{proxy_url}");
        if let Some(client) = self.proxy_clients.get(&cache_key) {
            return Ok(client);
        }

        match crate::proxy_policy::build_http_client(Some(&proxy_url)) {
            Ok(client) => {
                let scope = format!("{provider_name}|{member_id}|");
                self.proxy_clients.replace_scoped_generation(
                    cache_gen,
                    cache_key,
                    &scope,
                    member_id,
                    client.clone(),
                );
                Ok(client)
            }
            Err(err) => {
                tracing::error!(
                    provider = %provider_name,
                    member_id = %member_id,
                    proxy_url = %proxy_url,
                    "failed to configure outbound proxy: {err}"
                );
                Err(crate::proxy_policy::ProxyClientBuildError::ProxyConfig(
                    err.to_string(),
                ))
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn client_for_target_at_with_override(
        &self,
        provider_name: &str,
        member_id: &str,
        credential_proxy: Option<&str>,
        now_unix: u64,
    ) -> reqwest::Client {
        self.try_client_for_target_at_with_override(
            provider_name,
            member_id,
            credential_proxy,
            now_unix,
        )
        .unwrap_or_else(|_| self.direct_client())
    }

    /// Reusable HTTP/1.1 client with no-redirect policy for Devin discovery and Chat relay.
    /// Follows provider-aware outbound proxy configuration and caches clients by target.
    /// Returns typed/safe Result propagating pre-dispatch failure without secret disclosure or unproxied fallback.
    pub fn devin_client_for_member(
        &self,
        member: &AccountMember,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.devin_client_for_member_at(member, now_unix)
    }

    pub fn devin_client_for_member_at(
        &self,
        member: &AccountMember,
        now_unix: u64,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        self.devin_client_for_target_at_with_override(
            member.id(),
            member.proxy_url.as_deref(),
            now_unix,
        )
    }

    pub fn subscribe_finalizer(&self, account_or_event: &str) -> tokio::sync::mpsc::Receiver<()> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut guard = self
            .finalizer_notifiers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard
            .entry(account_or_event.to_string())
            .or_default()
            .push(tx);
        rx
    }

    pub fn notify_finalizer(&self, account: Option<&str>, event_id: &str) {
        let mut guard = self
            .finalizer_notifiers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(acc) = account {
            if let Some(senders) = guard.remove(acc) {
                for sender in senders {
                    let _ = sender.try_send(());
                }
            }
        }
        if let Some(senders) = guard.remove(event_id) {
            for sender in senders {
                let _ = sender.try_send(());
            }
        }
    }

    /// Alias for devin_client_for_member returning typed/safe Result.
    pub fn try_devin_client_for_member(
        &self,
        member: &AccountMember,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        self.devin_client_for_member(member)
    }

    /// Reusable HTTP/1.1 client with no-redirect policy for Devin by member ID.
    pub fn devin_client_for_target(
        &self,
        member_id: &str,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.devin_client_for_target_at(member_id, now_unix)
    }

    /// Time-injected core of `devin_client_for_target`; see
    /// `client_for_target_at` for the retention rule.
    pub(crate) fn devin_client_for_target_at(
        &self,
        member_id: &str,
        now_unix: u64,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        self.devin_client_for_target_at_with_override(member_id, None, now_unix)
    }

    pub(crate) fn devin_client_for_target_at_with_override(
        &self,
        member_id: &str,
        credential_proxy: Option<&str>,
        now_unix: u64,
    ) -> Result<reqwest::Client, crate::proxy_policy::DevinClientBuildError> {
        let snapshot = self.proxy.load();
        let runtime = &snapshot.runtime;
        let cache_gen = self.proxy_clients.generation();

        let effective_proxy_url = if let Some(cred_url) = credential_proxy {
            let trimmed = cred_url.trim();
            if !trimmed.is_empty() {
                if crate::proxy_policy::is_direct_or_none(trimmed) {
                    Some("direct".to_string())
                } else {
                    Some(trimmed.to_string())
                }
            } else {
                runtime.session_proxy_url("devin", member_id, now_unix)
            }
        } else {
            runtime.session_proxy_url("devin", member_id, now_unix)
        };

        let Some(proxy_url) = effective_proxy_url else {
            // session_proxy_url(None) must honor global proxy semantics consistently with existing client_for_target
            return Ok(if runtime.scoped_routing_active() {
                snapshot.devin_direct_client.clone()
            } else {
                snapshot.devin_default_client.clone()
            });
        };

        if crate::proxy_policy::is_direct_or_none(&proxy_url) {
            return Ok(snapshot.devin_direct_client.clone());
        }

        let cache_key = format!("devin|{member_id}|{proxy_url}");
        if let Some(client) = self.proxy_clients.get(&cache_key) {
            return Ok(client);
        }

        match crate::proxy_policy::build_devin_http_client(Some(&proxy_url)) {
            Ok(client) => {
                let scope = format!("devin|{member_id}|");
                self.proxy_clients.replace_scoped_generation(
                    cache_gen,
                    cache_key,
                    &scope,
                    member_id,
                    client.clone(),
                );
                Ok(client)
            }
            Err(err) => {
                // Propagate pre-dispatch failure without secret disclosure or unproxied fallback
                tracing::error!(
                    member_id = %member_id,
                    "failed to configure Devin outbound proxy: {err}"
                );
                Err(err)
            }
        }
    }

    pub fn get_stats(&self) -> AdminStatsResponse {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // Routability mirrors what the relay would do, per account, over the
        // models that account can serve. Candidates are the models generic
        // accounts declare (deduplicated) — data-driven, so a provider adding
        // or retiring models needs no edit here. Non-generic providers (codex,
        // antigravity, ...) keep their own discovery surfaces.
        let mut candidate_models: Vec<String> = Vec::new();
        for member in self.pool.load().members.iter() {
            if let Some((_, declared)) = member.generic_models() {
                for model in declared {
                    if !candidate_models.contains(&model) {
                        candidate_models.push(model);
                    }
                }
            }
        }
        let model_candidates: Vec<_> = candidate_models
            .iter()
            .map(|model| {
                (
                    model.as_str(),
                    crate::relay::eligible_account_ids_for_model(self, model, now_ms),
                )
            })
            .collect();

        let accounts = self
            .pool
            .load()
            .members
            .iter()
            .map(|m| {
                let health = m.health();
                let (input_tokens, output_tokens) = self.telemetry.account_tokens(&m.id);
                // Per-model quota blocks live only in `group_cooldowns`; account-wide
                // health never sees them. Fold the earliest active one in so a benched
                // account stops reporting itself `available`.
                let group_deadline = m.earliest_group_cooldown_ms(now_ms);
                let (effective_health, reset_at_unix_ms) = match health {
                    Health::Cooldown { until_unix_ms } if until_unix_ms <= now_ms => {
                        match group_deadline {
                            Some(until_unix_ms) => (
                                crate::metrics::HealthStats::Cooldown { until_unix_ms },
                                Some(until_unix_ms),
                            ),
                            None => (crate::metrics::HealthStats::Available, None),
                        }
                    }
                    Health::Cooldown { until_unix_ms } => {
                        // Whichever block releases first is the one the operator awaits.
                        let until_unix_ms = group_deadline
                            .filter(|group| *group < until_unix_ms)
                            .unwrap_or(until_unix_ms);
                        (
                            crate::metrics::HealthStats::Cooldown { until_unix_ms },
                            Some(until_unix_ms),
                        )
                    }
                    Health::Available => match group_deadline {
                        Some(until_unix_ms) => (
                            crate::metrics::HealthStats::Cooldown { until_unix_ms },
                            Some(until_unix_ms),
                        ),
                        None => (crate::metrics::HealthStats::Available, None),
                    },
                    Health::AuthFailed => (crate::metrics::HealthStats::AuthFailed, None),
                    Health::Disabled => (crate::metrics::HealthStats::Disabled, None),
                };
                crate::metrics::AccountStats {
                    id: m.id.clone(),
                    provider: m.provider_name(),
                    plan: m.relay_plan(),
                    health: effective_health,
                    ok: m.ok_count.load(Ordering::Relaxed),
                    fails: m.fail_count.load(Ordering::Relaxed),
                    input_tokens,
                    output_tokens,
                    total_tokens: input_tokens.saturating_add(output_tokens),
                    reset_at_unix_ms,
                    model_routability: m.generic_models().map(|(_, declared)| {
                        model_candidates
                            .iter()
                            .filter(|(model, _)| {
                                declared.is_empty()
                                    || declared.iter().any(|id| id.as_str() == *model)
                            })
                            .map(|(model, ids)| ((*model).to_string(), ids.contains(&m.id)))
                            .collect()
                    }),
                    last_error: self.monitor.last_error(&m.id),
                    ttft: self.monitor.account_ttft(&m.id),
                    usage: m.usage_snapshot(),
                    models: if m.kind() == crate::account::ProviderKind::Devin {
                        m.devin_models()
                    } else {
                        None
                    },
                    discovery: if m.kind() == crate::account::ProviderKind::Devin {
                        Some(match m.devin_catalog_state() {
                            Some(cat) => crate::metrics::AccountDiscoveryMetadata {
                                status: if m.is_manually_disabled() {
                                    "disabled".to_string()
                                } else if !cat.has_succeeded {
                                    "unknown".to_string()
                                } else if cat.models.is_empty() {
                                    "empty".to_string()
                                } else {
                                    "discovered".to_string()
                                },
                                has_succeeded: cat.has_succeeded,
                                refreshed_at_unix_ms: cat.last_refresh_at,
                                models: if m.is_manually_disabled() {
                                    Vec::new()
                                } else {
                                    cat.models
                                        .iter()
                                        .map(|model| crate::metrics::DiscoveredModelMetadata {
                                            id: model.public_id.clone(),
                                            model_uid: model.model_uid.clone(),
                                            supports_images: model.supports_images,
                                            credit_multiplier: model.credit_multiplier,
                                            is_recommended: model.is_recommended,
                                            is_new: model.is_new,
                                            is_capacity_limited: model.is_capacity_limited,
                                            promo_active: model.promo_active,
                                        })
                                        .collect()
                                },
                            },
                            None => crate::metrics::AccountDiscoveryMetadata {
                                status: if m.is_manually_disabled() {
                                    "disabled".to_string()
                                } else {
                                    "unknown".to_string()
                                },
                                has_succeeded: false,
                                refreshed_at_unix_ms: None,
                                models: Vec::new(),
                            },
                        })
                    } else {
                        None
                    },
                    credits_after_limit: if m.kind() == crate::account::ProviderKind::Codex {
                        Some(
                            self.settings
                                .current()
                                .is_codex_account_credit_enabled(&m.id),
                        )
                    } else {
                        None
                    },
                }
            })
            .collect();

        AdminStatsResponse {
            uptime_secs: self.monitor.uptime_secs(now_ms),
            in_flight: self.monitor.in_flight(),
            served: self.metrics.served.load(Ordering::Relaxed),
            failed_over: self.metrics.failed_over.load(Ordering::Relaxed),
            refreshed: self.refreshed.load(Ordering::Relaxed),
            exposed_errors: self.metrics.exposed_errors.load(Ordering::Relaxed),
            exposed_client_errors: self.metrics.exposed_client_errors.load(Ordering::Relaxed),
            ttft: self.monitor.ttft_percentiles(),
            accounts,
            history: self.telemetry.snapshot(),
            signature_ledger: self.signature_ledger.stats(),
        }
    }

    pub fn plugin_host(&self) -> Option<Arc<crate::plugins::loader::PluginHost>> {
        self.plugins.clone()
    }

    pub fn set_plugin_host(&mut self, host: Option<Arc<crate::plugins::loader::PluginHost>>) {
        self.plugins = host;
    }

    pub fn home_client(&self) -> Option<Arc<crate::home::HomeClient>> {
        self.home.clone()
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    fn state_with_cline_account() -> (Arc<AppState>, Arc<crate::account::AccountMember>) {
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-stats-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        let credential = serde_json::json!({
            "type": "generic",
            "identity_slug": "cline-account",
            "provider": "cline",
            "label": "Cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:9",
            "api_key": "fixture-cline",
            "models": ["cline-free/gemini-3.8-flash", "cline-free/deepseek-v4.1-flash"],
        })
        .to_string();
        std::fs::write(auth_dir.join("generic-cline.json"), credential).expect("write cline");
        let config = crate::config::GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..crate::config::GatewayConfig::default()
        };
        let state = Arc::new(AppState::new(&config).expect("state"));
        let member = state.pool.load_full().members[0].clone();
        (state, member)
    }

    fn account_json(state: &AppState, id: &str) -> serde_json::Value {
        let stats = state.get_stats();
        let account = stats
            .accounts
            .iter()
            .find(|account| account.id == id)
            .expect("account present in stats");
        serde_json::to_value(account).expect("serialize account stats")
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn an_account_benched_for_one_quota_group_reports_cooldown() {
        let (state, member) = state_with_cline_account();
        let deadline = (now_ms() / 1000 + 600) * 1000;
        assert!(
            member.set_group_cooldown("z-ai/glm-5.3-flash", deadline),
            "test premise: cline must split quota groups"
        );

        let value = account_json(&state, &member.id);
        assert_eq!(
            value["health"]["status"], "cooldown",
            "an account blocked for one model must not report itself available while \
             requests for it are being rejected"
        );
        assert_eq!(
            value["reset_at_unix_ms"],
            serde_json::json!(deadline),
            "the earliest per-model deadline is the reset the operator waits on"
        );
    }

    #[test]
    fn an_account_with_no_active_quota_block_stays_available() {
        let (state, member) = state_with_cline_account();
        let expired = (now_ms() / 1000 - 60) * 1000;
        assert!(member.set_group_cooldown("z-ai/glm-5.3-flash", expired));

        let value = account_json(&state, &member.id);
        assert_eq!(
            value["health"]["status"], "available",
            "an expired per-model deadline must not pin the cooldown badge"
        );
        assert_eq!(value["reset_at_unix_ms"], serde_json::json!(null));
    }

    #[test]
    fn cline_stats_separate_estimated_usage_from_model_routing() {
        let (state, member) = state_with_cline_account();
        let now = now_ms();
        crate::relay::record_cline_quota_bucket(
            &member,
            "cline-free/gemini-3.8-flash",
            600,
            now / 1000,
            99.9,
        );
        let before = account_json(&state, &member.id);
        assert_eq!(
            before["model_routability"]["cline-free/gemini-3.8-flash"],
            true
        );
        member.set_group_cooldown("cline-free/gemini-3.8-flash", now + 600_000);
        let stats = account_json(&state, &member.id);
        assert_eq!(
            stats["model_routability"]["cline-free/gemini-3.8-flash"],
            false
        );
        assert_eq!(
            stats["model_routability"]["cline-free/deepseek-v4.1-flash"],
            true
        );

        let (expired_state, expired_member) = state_with_cline_account();
        expired_member.set_group_cooldown("cline-free/gemini-3.8-flash", now - 1_000);
        let expired = account_json(&expired_state, &expired_member.id);
        assert_eq!(
            expired["model_routability"]["cline-free/gemini-3.8-flash"],
            true
        );

        state.force_health(&member.id, Health::Disabled);
        let disabled = account_json(&state, &member.id);
        assert_eq!(
            disabled["model_routability"]["cline-free/gemini-3.8-flash"],
            false
        );
        assert_eq!(
            disabled["model_routability"]["cline-free/deepseek-v4.1-flash"],
            false
        );
    }

    #[test]
    fn a_non_cline_generic_account_reports_routability_for_its_declared_models() {
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-stats-zhipu-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        let credential = serde_json::json!({
            "type": "generic",
            "identity_slug": "zhipu-coding",
            "provider": "zhipu-bigmodel-coding",
            "label": "Zhipu",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:9",
            "api_key": "fixture-zhipu",
            "models": ["glm-5.3", "glm-5.3-flash"],
        })
        .to_string();
        std::fs::write(auth_dir.join("generic-zhipu.json"), credential).expect("write zhipu");
        let config = crate::config::GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..crate::config::GatewayConfig::default()
        };
        let state = Arc::new(AppState::new(&config).expect("state"));
        let member = state.pool.load_full().members[0].clone();

        let value = account_json(&state, &member.id);
        assert_eq!(
            value["model_routability"]["glm-5.3"], true,
            "a healthy account must report its declared models routable"
        );
        assert_eq!(value["model_routability"]["glm-5.3-flash"], true);

        // Providers without a quota-group split bench per model through the
        // upstream unsupported-models feedback path, not group cooldowns.
        member.mark_model_unsupported("glm-5.3-flash");
        let value = account_json(&state, &member.id);
        assert_eq!(
            value["model_routability"]["glm-5.3-flash"], false,
            "an upstream-unsupported model must show up as unroutable"
        );
        assert_eq!(
            value["model_routability"]["glm-5.3"], true,
            "the sibling model must stay routable — per-model, not account-wide"
        );

        state.force_health(&member.id, Health::Disabled);
        let value = account_json(&state, &member.id);
        assert_eq!(
            value["model_routability"]["glm-5.3"], false,
            "account-wide health must dominate every per-model entry"
        );
        assert_eq!(value["model_routability"]["glm-5.3-flash"], false);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::management::settings::ProviderProxyPolicy;

    fn state_with_one_account() -> (Arc<AppState>, Arc<crate::account::AccountMember>) {
        let auth_dir = std::env::temp_dir().join(format!(
            "mahoquot-shared-integration-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&auth_dir).expect("create auth dir");
        let credential = serde_json::json!({
            "type": "generic",
            "identity_slug": "cline-account",
            "provider": "cline",
            "label": "Cline",
            "adapter": "openai-chat",
            "base_url": "http://127.0.0.1:9",
            "api_key": "fixture-cline",
            "models": ["cline-free/gemini-3.8-flash"],
        })
        .to_string();
        std::fs::write(auth_dir.join("generic-cline.json"), credential).expect("write cline");
        let config = crate::config::GatewayConfig {
            auth_dir: auth_dir.clone(),
            config_path: auth_dir.join("config.yaml"),
            auth_refresh_enabled: false,
            ..crate::config::GatewayConfig::default()
        };
        let state = Arc::new(AppState::new(&config).expect("state"));
        let member = state.pool.load_full().members[0].clone();
        (state, member)
    }

    /// Sticky TTL policy: the session proxy URL changes once per bucket, which is
    /// the rotation that used to grow the client map without bound.
    fn install_sticky_policy(state: &AppState, provider: &str) {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            provider.to_string(),
            ProviderProxyPolicy {
                enabled: true,
                sticky: true,
                ttl_secs: 600,
                url: "http://127.0.0.1:18840".to_string(),
            },
        );
        let settings = crate::management::settings::Settings {
            proxy_providers: providers,
            ..crate::management::settings::Settings::default()
        };
        let snapshot =
            crate::proxy_policy::ProxySnapshot::from_settings(&settings).expect("snapshot");
        state.proxy.store(Arc::new(snapshot));
    }

    #[test]
    fn proxy_clients_keep_only_the_latest_url_per_member() {
        let (state, member) = state_with_one_account();
        install_sticky_policy(&state, "cline");

        let first = state.client_for_target_at("cline", &member.id, 1_000_000);
        assert_eq!(state.proxy_client_count(), 1);

        let second = state.client_for_target_at("cline", &member.id, 1_000_600);
        assert_eq!(
            state.proxy_client_count(),
            1,
            "a rotation bucket must replace the member's previous client, not accumulate"
        );
        let again = state.client_for_target_at("cline", &member.id, 1_000_600);
        assert_eq!(
            state.proxy_client_count(),
            1,
            "the newest bucket stays cached"
        );

        drop((first, second, again));
        assert_eq!(state.proxy_client_count(), 1);
    }

    #[test]
    fn proxy_clients_keep_only_the_latest_url_per_devin_member() {
        let (state, member) = state_with_one_account();
        install_sticky_policy(&state, "devin");

        state
            .devin_client_for_target_at(&member.id, 2_000_000)
            .expect("devin client");
        assert_eq!(state.proxy_client_count(), 1);
        state
            .devin_client_for_target_at(&member.id, 2_000_600)
            .expect("devin client");
        assert_eq!(
            state.proxy_client_count(),
            1,
            "Devin rotation must replace, not accumulate"
        );
    }

    #[test]
    fn proxy_clients_keep_distinct_members_separate() {
        let (state, _member) = state_with_one_account();
        install_sticky_policy(&state, "cline");
        state.proxy_clients.retain_members(
            &["member-a".to_string(), "member-b".to_string()]
                .into_iter()
                .collect(),
        );

        state.client_for_target_at("cline", "member-a", 3_000_000);
        state.client_for_target_at("cline", "member-b", 3_000_000);
        assert_eq!(state.proxy_client_count(), 2);

        state.client_for_target_at("cline", "member-a", 3_000_600);
        assert_eq!(
            state.proxy_client_count(),
            2,
            "rotating one member must not evict another member's client"
        );
    }

    #[test]
    fn inference_gate_is_exhausted_at_ceiling_and_recovers_on_drop() {
        let (state, _member) = state_with_one_account();
        let gate = state.inference_gate();
        let ceiling = gate.available_permits();
        assert!(
            ceiling >= 100,
            "the default ceiling must swallow a hundred concurrent requests, got {}",
            ceiling
        );

        let mut held = Vec::new();
        for _ in 0..ceiling {
            held.push(
                gate.clone()
                    .try_acquire_owned()
                    .expect("permit within the ceiling"),
            );
        }
        assert!(
            gate.clone().try_acquire_owned().is_err(),
            "the request past the ceiling must be rejected immediately"
        );

        held.pop();
        assert!(
            gate.clone().try_acquire_owned().is_ok(),
            "releasing a permit must admit the next request"
        );
    }

    #[test]
    fn devin_refresh_claim_is_single_flight_and_released_on_drop() {
        let (state, _member) = state_with_one_account();

        let guard = state.begin_devin_refresh("acct").expect("first claim");
        assert!(
            state.begin_devin_refresh("acct").is_none(),
            "a repeated request while a refresh is in flight must coalesce"
        );
        assert_eq!(state.devin_refresh_in_flight_count(), 1);

        drop(guard);
        assert!(
            state.begin_devin_refresh("acct").is_some(),
            "dropping the guard must release the claim"
        );
    }

    #[test]
    fn per_credential_override_wins_over_provider_and_global_policy() {
        let (state, member) = state_with_one_account();
        install_sticky_policy(&state, "cline");

        // 1. With credential proxy set to direct, it returns direct client without caching in proxy_clients
        let _direct_client = state.client_for_target_at_with_override(
            "cline",
            &member.id,
            Some("direct"),
            1_000_000,
        );
        assert_eq!(
            state.proxy_client_count(),
            0,
            "direct client must not cache in proxy_clients"
        );

        // 2. With credential proxy set to custom URL, it caches for that credential-specific URL
        let _cred_client = state.client_for_target_at_with_override(
            "cline",
            &member.id,
            Some("http://127.0.0.1:18841"),
            1_000_000,
        );
        assert_eq!(
            state.proxy_client_count(),
            1,
            "credential proxy override must cache in proxy_clients"
        );
        let cached_key = format!("cline|{}|http://any=1:x@127.0.0.1:18841/", member.id);
        assert!(
            state.proxy_clients.get(&cached_key).is_some(),
            "expected cache key for credential proxy URL"
        );

        // 3. Without credential override, it resolves provider policy (18840) and evicts older member cache
        let _provider_client =
            state.client_for_target_at_with_override("cline", &member.id, None, 1_000_000);
        assert_eq!(state.proxy_client_count(), 1);
        let provider_cached_key = format!(
            "cline|{}|http://sess={}-b1666;any=1:x@127.0.0.1:18840/",
            member.id, member.id
        );
        assert!(
            state.proxy_clients.get(&provider_cached_key).is_some(),
            "expected cache key for provider proxy URL"
        );
    }

    #[test]
    fn atomic_proxy_snapshot_updates_runtime_and_clients_together() {
        let (state, _member) = state_with_one_account();
        assert_eq!(state.proxy.load().runtime.global_proxy_url, "");

        // Mutate proxy-url via SettingsStore
        state
            .settings
            .mutate(|s| s.proxy_url = "http://127.0.0.1:18842".to_string())
            .expect("mutate proxy-url");

        let snapshot = state.proxy.load();
        assert_eq!(snapshot.runtime.global_proxy_url, "http://127.0.0.1:18842");

        // Pre-validation protects the snapshot: invalid URL leaves runtime and disk untouched
        let before_url = state.proxy.load().runtime.global_proxy_url.clone();
        let mutate_err = state
            .settings
            .mutate(|s| s.proxy_url = "ftp://bad-url".to_string());
        assert!(mutate_err.is_err());
        assert_eq!(state.proxy.load().runtime.global_proxy_url, before_url);
    }

    #[tokio::test]
    async fn credential_proxy_precedence_actual_wire_including_devin() {
        let (state, member) = state_with_one_account();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock bind");
        let credential_proxy = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("mock addr").port()
        );
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::channel(4);
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let tx = seen_tx.clone();
            async move {
                tx.send(uri.to_string()).await.expect("capture");
                "credential-proxy-sentinel"
            }
        });
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    stop_rx.await.expect("stop");
                })
                .await
                .expect("serve");
        });
        let mut settings = crate::management::settings::Settings {
            proxy_url: "http://127.0.0.1:18846".into(),
            ..Default::default()
        };
        for provider in ["cline", "devin"] {
            settings.proxy_providers.insert(
                provider.into(),
                ProviderProxyPolicy {
                    enabled: true,
                    sticky: false,
                    ttl_secs: 0,
                    url: "http://127.0.0.1:18847".into(),
                },
            );
        }
        state.proxy.store(Arc::new(
            crate::proxy_policy::ProxySnapshot::from_settings(&settings).expect("snapshot"),
        ));
        for provider in ["cline", "devin"] {
            let client = if provider == "devin" {
                state
                    .devin_client_for_target_at_with_override(
                        &member.id,
                        Some(credential_proxy.as_str()),
                        1,
                    )
                    .expect("devin credential client")
            } else {
                state
                    .try_client_for_target_at_with_override(
                        provider,
                        &member.id,
                        Some(credential_proxy.as_str()),
                        1,
                    )
                    .expect("credential client")
            };
            let response = client
                .get(format!(
                    "http://127.0.0.1:18848/{provider}-credential-proof"
                ))
                .timeout(std::time::Duration::from_secs(3))
                .send()
                .await
                .expect("wire");
            assert_eq!(
                response.text().await.expect("body"),
                "credential-proxy-sentinel"
            );
            let captured = tokio::time::timeout(std::time::Duration::from_secs(3), seen_rx.recv())
                .await
                .expect("capture deadline")
                .expect("capture");
            assert!(captured.contains(&format!("/{provider}-credential-proof")));
        }
        assert!(state
            .try_client_for_target_at_with_override("cline", &member.id, Some("invalid-proxy"), 1)
            .is_err());
        assert!(state
            .devin_client_for_target_at_with_override(&member.id, Some("invalid-proxy"), 1)
            .is_err());
        stop_tx.send(()).expect("shutdown");
        server.await.expect("join");
        std::fs::remove_dir_all(state.settings.path().parent().expect("auth directory"))
            .expect("fixture cleanup");
    }
}

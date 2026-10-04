use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

const RETENTION_MINUTES: i64 = 30 * 24 * 60;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTelemetry {
    pub provider: String,
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountTelemetry {
    pub account: String,
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryBucket {
    pub minute_unix: i64,
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    pub providers: Vec<ProviderTelemetry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<AccountTelemetry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TelemetryDocument {
    buckets: Vec<TelemetryBucket>,
}

#[derive(Debug)]
pub struct TelemetryStore {
    path: PathBuf,
    buckets: Mutex<Vec<TelemetryBucket>>,
    flush_requested: tokio::sync::Notify,
    #[cfg(test)]
    flush_started: Mutex<Option<std::sync::Arc<tokio::sync::Notify>>>,
}

/// Index of the bucket for `minute_unix`, inserted (keeping the vector
/// ordered) when missing. Keyed lookup instead of a last-bucket check: an
/// event arriving out of order across a minute boundary must reuse its
/// bucket, not fragment the history with a duplicate minute.
fn bucket_index_for(buckets: &mut Vec<TelemetryBucket>, minute_unix: i64) -> usize {
    match buckets.binary_search_by_key(&minute_unix, |b| b.minute_unix) {
        Ok(index) => index,
        Err(position) => {
            buckets.insert(
                position,
                TelemetryBucket {
                    minute_unix,
                    ..TelemetryBucket::default()
                },
            );
            position
        }
    }
}

/// Owns the dedicated stop signal for the telemetry flush task, so shutting
/// the worker down can never contend with the store's shared
/// `flush_requested` notification (no stolen or lost wakeups).
pub struct FlushWorkerHandle {
    stop: std::sync::Arc<tokio::sync::Notify>,
    join: tokio::task::JoinHandle<()>,
}

impl FlushWorkerHandle {
    /// Signals the worker to stop and awaits it to completion.
    ///
    /// A `spawn_blocking` flush cannot be cancelled, so this deliberately applies
    /// no timeout: a dropped `JoinHandle` would detach the worker while it still
    /// owns a `TelemetryStore` reference, abandoning the very flush shutdown
    /// exists to finish. Callers that need a bound wrap this future in their own
    /// timeout; the implementation never gives up on the worker.
    pub async fn shutdown(self) {
        self.stop.notify_one();
        let _ = self.join.await;
    }
}

impl TelemetryStore {
    pub fn load(path: PathBuf) -> Self {
        let buckets = load_buckets(&path);
        Self {
            path,
            buckets: Mutex::new(buckets),
            flush_requested: tokio::sync::Notify::new(),
            #[cfg(test)]
            flush_started: Mutex::new(None),
        }
    }

    /// Historical entry point: callers without an account context.
    pub fn record(&self, unix_secs: i64, provider: &str, success: bool) {
        self.record_with_account(unix_secs, provider, None, success);
    }

    pub fn record_with_account(
        &self,
        unix_secs: i64,
        provider: &str,
        account: Option<&str>,
        success: bool,
    ) {
        let minute_unix = unix_secs.div_euclid(60) * 60;
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bucket_index = bucket_index_for(&mut buckets, minute_unix);
        let bucket = &mut buckets[bucket_index];
        bucket.requests += 1;
        if success {
            bucket.successes += 1;
        } else {
            bucket.failures += 1;
        }
        let provider_index = match bucket
            .providers
            .iter()
            .position(|item| item.provider == provider)
        {
            Some(provider_index) => provider_index,
            None => {
                bucket.providers.push(ProviderTelemetry {
                    provider: provider.to_string(),
                    ..ProviderTelemetry::default()
                });
                bucket.providers.len() - 1
            }
        };
        let provider_bucket = &mut bucket.providers[provider_index];
        provider_bucket.requests += 1;
        if success {
            provider_bucket.successes += 1;
        } else {
            provider_bucket.failures += 1;
        }
        if let Some(account) = account {
            let account_index = match bucket
                .accounts
                .iter()
                .position(|item| item.account == account)
            {
                Some(account_index) => account_index,
                None => {
                    bucket.accounts.push(AccountTelemetry {
                        account: account.to_string(),
                        ..AccountTelemetry::default()
                    });
                    bucket.accounts.len() - 1
                }
            };
            let account_bucket = &mut bucket.accounts[account_index];
            account_bucket.requests += 1;
            if success {
                account_bucket.successes += 1;
            } else {
                account_bucket.failures += 1;
            }
        }
        let earliest = minute_unix - RETENTION_MINUTES * 60;
        buckets.retain(|item| item.minute_unix >= earliest);
        self.flush_requested.notify_one();
    }

    pub fn snapshot(&self) -> Vec<TelemetryBucket> {
        self.buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn remove_history_groups(&self, groups: &[crate::request_history::HistoryGroup]) -> usize {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = buckets.len();
        for group in groups {
            let (Some(bucket_start_ms), Some(provider), Some(account)) = (
                group.key.bucket_start_ms,
                group.key.provider.as_deref(),
                group.key.account.as_deref(),
            ) else {
                continue;
            };
            let minute_unix = bucket_start_ms.div_euclid(1_000);
            let Some(bucket) = buckets
                .iter_mut()
                .find(|bucket| bucket.minute_unix == minute_unix)
            else {
                continue;
            };
            let totals = &group.totals;
            bucket.requests = bucket.requests.saturating_sub(totals.requests);
            bucket.successes = bucket.successes.saturating_sub(totals.successful_requests);
            bucket.failures = bucket.failures.saturating_sub(totals.failed_requests);
            bucket.input_tokens = bucket.input_tokens.saturating_sub(totals.input_tokens);
            bucket.output_tokens = bucket.output_tokens.saturating_sub(totals.output_tokens);
            if let Some(provider_bucket) = bucket
                .providers
                .iter_mut()
                .find(|item| item.provider == provider)
            {
                provider_bucket.requests = provider_bucket.requests.saturating_sub(totals.requests);
                provider_bucket.successes = provider_bucket
                    .successes
                    .saturating_sub(totals.successful_requests);
                provider_bucket.failures = provider_bucket
                    .failures
                    .saturating_sub(totals.failed_requests);
                provider_bucket.input_tokens = provider_bucket
                    .input_tokens
                    .saturating_sub(totals.input_tokens);
                provider_bucket.output_tokens = provider_bucket
                    .output_tokens
                    .saturating_sub(totals.output_tokens);
            }
            if let Some(account_bucket) = bucket
                .accounts
                .iter_mut()
                .find(|item| item.account == account)
            {
                account_bucket.requests = account_bucket.requests.saturating_sub(totals.requests);
                account_bucket.successes = account_bucket
                    .successes
                    .saturating_sub(totals.successful_requests);
                account_bucket.failures = account_bucket
                    .failures
                    .saturating_sub(totals.failed_requests);
                account_bucket.input_tokens = account_bucket
                    .input_tokens
                    .saturating_sub(totals.input_tokens);
                account_bucket.output_tokens = account_bucket
                    .output_tokens
                    .saturating_sub(totals.output_tokens);
            }
        }
        for bucket in buckets.iter_mut() {
            bucket.providers.retain(|item| {
                item.requests != 0
                    || item.successes != 0
                    || item.failures != 0
                    || item.input_tokens != 0
                    || item.output_tokens != 0
            });
            bucket.accounts.retain(|item| {
                item.requests != 0
                    || item.successes != 0
                    || item.failures != 0
                    || item.input_tokens != 0
                    || item.output_tokens != 0
            });
        }
        buckets.retain(|bucket| {
            bucket.requests != 0
                || bucket.successes != 0
                || bucket.failures != 0
                || bucket.input_tokens != 0
                || bucket.output_tokens != 0
                || !bucket.providers.is_empty()
                || !bucket.accounts.is_empty()
        });
        if !groups.is_empty() {
            self.flush_requested.notify_one();
        }
        before.saturating_sub(buckets.len())
    }

    pub fn record_tokens(
        &self,
        unix_secs: i64,
        provider: &str,
        account: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        let minute_unix = unix_secs.div_euclid(60) * 60;
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bucket_index = bucket_index_for(&mut buckets, minute_unix);
        let bucket = &mut buckets[bucket_index];
        bucket.input_tokens = bucket.input_tokens.saturating_add(input_tokens);
        bucket.output_tokens = bucket.output_tokens.saturating_add(output_tokens);

        let provider_bucket = match bucket
            .providers
            .iter_mut()
            .find(|item| item.provider == provider)
        {
            Some(provider_bucket) => provider_bucket,
            None => {
                bucket.providers.push(ProviderTelemetry {
                    provider: provider.to_string(),
                    ..ProviderTelemetry::default()
                });
                bucket
                    .providers
                    .last_mut()
                    .expect("provider bucket inserted")
            }
        };
        provider_bucket.input_tokens = provider_bucket.input_tokens.saturating_add(input_tokens);
        provider_bucket.output_tokens = provider_bucket.output_tokens.saturating_add(output_tokens);

        let account_bucket = match bucket
            .accounts
            .iter_mut()
            .find(|item| item.account == account)
        {
            Some(account_bucket) => account_bucket,
            None => {
                bucket.accounts.push(AccountTelemetry {
                    account: account.to_string(),
                    ..AccountTelemetry::default()
                });
                bucket.accounts.last_mut().expect("account bucket inserted")
            }
        };
        account_bucket.input_tokens = account_bucket.input_tokens.saturating_add(input_tokens);
        account_bucket.output_tokens = account_bucket.output_tokens.saturating_add(output_tokens);
        self.flush_requested.notify_one();
    }

    pub fn account_tokens(&self, account: &str) -> (u64, u64) {
        self.buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .flat_map(|bucket| bucket.accounts.iter())
            .filter(|item| item.account == account)
            .fold((0_u64, 0_u64), |(input, output), item| {
                (
                    input.saturating_add(item.input_tokens),
                    output.saturating_add(item.output_tokens),
                )
            })
    }

    pub fn flush(&self) -> std::io::Result<()> {
        let document = TelemetryDocument {
            buckets: self.snapshot(),
        };
        let rendered = serde_json::to_vec(&document)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self
            .path
            .with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&temporary, rendered)?;
        std::fs::rename(temporary, &self.path)
    }

    pub fn spawn_flush_worker(
        self: &std::sync::Arc<Self>,
        interval: std::time::Duration,
    ) -> FlushWorkerHandle {
        let store = std::sync::Arc::clone(self);
        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        let stop_signal = std::sync::Arc::clone(&stop);
        let join = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let mut stopping = false;
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = store.flush_requested.notified() => {}
                    _ = stop_signal.notified() => { stopping = true; }
                }
                let flush_store = std::sync::Arc::clone(&store);
                #[cfg(test)]
                if let Some(started) = store
                    .flush_started
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone()
                {
                    started.notify_one();
                }
                let result = tokio::task::spawn_blocking(move || flush_store.flush()).await;
                if let Ok(Err(error)) = result {
                    tracing::warn!(%error, "failed to flush telemetry history");
                }
                if stopping {
                    break;
                }
            }
        });
        FlushWorkerHandle { stop, join }
    }
}

fn load_buckets(path: &Path) -> Vec<TelemetryBucket> {
    std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<TelemetryDocument>(&raw).ok())
        .map(|document| document.buckets)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_buckets_survive_store_recreation() {
        let dir = std::env::temp_dir().join(format!("mahoquot-telemetry-{}", std::process::id()));
        let path = dir.join("telemetry.json");
        let store = TelemetryStore::load(path.clone());
        store.record_with_account(1_800, "codex", None, true);
        store.record_with_account(1_801, "codex", None, false);
        store.record_tokens(1_801, "codex", "codex-1", 120, 45);
        store.flush().expect("flush telemetry");

        let restored = TelemetryStore::load(path);

        assert_eq!(restored.snapshot()[0].requests, 2);
        assert_eq!(restored.snapshot()[0].successes, 1);
        assert_eq!(restored.snapshot()[0].failures, 1);
        assert_eq!(restored.snapshot()[0].providers[0].provider, "codex");
        assert_eq!(restored.account_tokens("codex-1"), (120, 45));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn token_usage_accumulates_without_incrementing_requests() {
        let store = TelemetryStore::load(PathBuf::from("unused.json"));
        store.record_with_account(1_800, "codex", Some("codex-1"), true);
        store.record_tokens(1_800, "codex", "codex-1", 50, 20);
        store.record_tokens(1_801, "codex", "codex-1", 25, 10);

        let bucket = &store.snapshot()[0];
        assert_eq!(bucket.requests, 1);
        assert_eq!(bucket.input_tokens, 75);
        assert_eq!(bucket.output_tokens, 30);
        assert_eq!(store.account_tokens("codex-1"), (75, 30));
    }

    #[test]
    fn retention_drops_buckets_older_than_thirty_days() {
        let store = TelemetryStore::load(PathBuf::from("unused.json"));
        store.record_with_account(0, "codex", None, true);
        store.record_with_account((RETENTION_MINUTES + 1) * 60, "claude", None, true);

        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(store.snapshot()[0].providers[0].provider, "claude");
    }

    #[tokio::test]
    async fn flush_worker_shutdown_waits_for_a_blocked_flush_to_finish() {
        let dir = std::env::temp_dir().join(format!(
            "mahoquot-telemetry-blocked-flush-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("telemetry.json");
        let store = std::sync::Arc::new(TelemetryStore::load(path.clone()));
        store.record_with_account(1_800, "codex", Some("codex-1"), true);
        let flush_started = std::sync::Arc::new(tokio::sync::Notify::new());
        *store.flush_started.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(std::sync::Arc::clone(&flush_started));
        let handle = store.spawn_flush_worker(std::time::Duration::from_secs(3600));

        // Hold the store lock so the worker's final `spawn_blocking(flush)`
        // blocks inside `snapshot()`. This is exactly the case a
        // timeout-then-drop shutdown would silently abandon.
        let guard = store.buckets.lock().unwrap_or_else(|p| p.into_inner());
        let worker = tokio::spawn(async move { handle.shutdown().await });
        tokio::time::timeout(std::time::Duration::from_secs(5), flush_started.notified())
            .await
            .expect("worker must begin its final flush");

        assert!(
            !worker.is_finished(),
            "shutdown must not return while the pending flush is still blocked"
        );
        assert!(
            std::sync::Arc::strong_count(&store) >= 2,
            "while the flush is blocked the worker still owns its store handle"
        );

        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .expect("shutdown must complete once the flush is released")
            .expect("the shutdown task must not panic");

        // A worker that was truly joined released every clone it held; a worker
        // that was detached would still own one and keep the count above one.
        assert_eq!(
            std::sync::Arc::strong_count(&store),
            1,
            "shutdown must return only after the worker released its store handle"
        );
        assert!(path.exists(), "the released flush must have reached disk");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn flush_worker_survives_a_flush_error() {
        let dir = std::env::temp_dir().join(format!(
            "mahoquot-telemetry-flush-error-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        // `blocker` is a regular file, so every `create_dir_all(parent)` inside
        // `flush` fails and the worker must keep looping instead of dying.
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"block").expect("write blocker");
        let path = blocker.join("telemetry.json");
        let store = std::sync::Arc::new(TelemetryStore::load(path.clone()));
        let handle = store.spawn_flush_worker(std::time::Duration::from_secs(3600));
        store.record_with_account(1_800, "codex", Some("codex-1"), true);

        handle.stop.notify_one();
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), handle.join).await;

        assert!(
            matches!(joined, Ok(Ok(()))),
            "flush worker must survive a flush error instead of panicking or hanging"
        );
        assert!(
            !path.exists(),
            "a failing flush must not create the store file"
        );
        std::fs::remove_dir_all(dir).ok();
    }
}

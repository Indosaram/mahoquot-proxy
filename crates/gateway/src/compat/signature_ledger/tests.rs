use std::sync::Arc;
use std::time::{Duration, Instant};

use super::store::{LruStore, SnapshotRecord, TTL};
use super::{LedgerConfig, ReplayScope, SignatureLedger};

fn scope(ledger: &Arc<SignatureLedger>) -> ReplayScope {
    ledger.scope("gemini-3.8-flash-high", "session-a")
}

fn temp_snapshot(label: &str) -> std::path::PathBuf {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    std::env::temp_dir().join(format!("mahoquot-ledger-{label}-{unique}/snapshot.json"))
}

#[test]
fn an_active_read_survives_capacity_pressure_that_evicts_a_cold_entry() {
    let ledger = SignatureLedger::with_config(LedgerConfig {
        max_entries: 8,
        ..LedgerConfig::default()
    });
    let scope = scope(&ledger);
    scope.remember("active", "tool", "{}", "SIG-ACTIVE");
    scope.remember("cold", "tool", "{}", "SIG-COLD");
    for index in 0..6 {
        scope.remember(&format!("fill-{index}"), "tool", "{}", "SIG-FILL");
    }
    assert_eq!(
        scope.recall("active", "tool", "{}").as_deref(),
        Some("SIG-ACTIVE")
    );

    scope.remember("overflow", "tool", "{}", "SIG-OVERFLOW");

    assert_eq!(
        scope.recall("active", "tool", "{}").as_deref(),
        Some("SIG-ACTIVE"),
        "a read must promote the entry ahead of untouched entries"
    );
    assert_eq!(
        scope.recall("cold", "tool", "{}"),
        None,
        "the untouched entry is the eviction victim"
    );
    assert_eq!(ledger.len(), 8);
}

#[test]
fn a_byte_budget_evicts_least_recently_used_entries_first() {
    let ledger = SignatureLedger::with_config(LedgerConfig {
        max_entries: 1024,
        max_bytes: 4096,
        ..LedgerConfig::default()
    });
    let scope = scope(&ledger);
    let blob = "S".repeat(900);
    scope.remember("kept", "tool", "{}", &blob);
    for index in 0..3 {
        scope.remember(&format!("filler-{index}"), "tool", "{}", &blob);
        assert_eq!(scope.recall("kept", "tool", "{}").as_deref(), Some(&blob[..]));
    }
    scope.remember("late", "tool", "{}", &blob);

    assert_eq!(scope.recall("kept", "tool", "{}").as_deref(), Some(&blob[..]));
    assert_eq!(scope.recall("filler-0", "tool", "{}"), None);
    assert!(ledger.bytes() <= 4096);
}

#[test]
fn scopes_do_not_share_entries_across_models_or_sessions() {
    let ledger = SignatureLedger::in_memory();
    let first = ledger.scope("gemini-3.8-flash-high", "session-a");
    let other_session = ledger.scope("gemini-3.8-flash-high", "session-b");
    let other_model = ledger.scope("gemini-3.7-flash-high", "session-a");
    first.remember("shared-id", "bash", "{}", "SIG-SCOPED");

    assert_eq!(
        first.recall("shared-id", "bash", "{}").as_deref(),
        Some("SIG-SCOPED")
    );
    assert_eq!(other_session.recall("shared-id", "bash", "{}"), None);
    assert_eq!(other_model.recall("shared-id", "bash", "{}"), None);
}

#[test]
fn equivalent_json_arguments_hit_and_different_values_miss() {
    let ledger = SignatureLedger::in_memory();
    let scope = scope(&ledger);
    scope.remember(
        "canonical",
        "tool",
        r#"{"outer":{"b":2,"a":1}}"#,
        "SIG-CANONICAL",
    );

    assert_eq!(
        scope
            .recall("canonical", "tool", r#"{ "outer": { "a": 1, "b": 2 } }"#)
            .as_deref(),
        Some("SIG-CANONICAL")
    );
    assert_eq!(
        scope.recall("canonical", "tool", r#"{"outer":{"a":2,"b":1}}"#),
        None
    );
}

#[test]
fn a_different_tool_name_or_missing_call_never_replays() {
    let ledger = SignatureLedger::in_memory();
    let scope = scope(&ledger);
    scope.remember("ledger-c", "read", "{}", "SIG-C");

    assert_eq!(scope.recall("ledger-c", "write", "{}"), None);
    assert_eq!(scope.recall("missing", "read", "{}"), None);
    assert_eq!(scope.recall("", "read", "{}"), None);
    scope.remember("", "read", "{}", "SIG-IGNORED");
    scope.remember("empty-signature", "read", "{}", "");
    assert_eq!(scope.recall("empty-signature", "read", "{}"), None);
}

#[test]
fn a_restored_snapshot_replays_after_a_restart() {
    let path = temp_snapshot("restore");
    let ledger = SignatureLedger::open(&path);
    let scope = scope(&ledger);
    scope.remember("durable", "bash", r#"{"command":"ls"}"#, "SIG-DURABLE");
    ledger.flush_blocking().expect("flush");

    let reopened = SignatureLedger::open(&path);
    let restored = reopened.scope("gemini-3.8-flash-high", "session-a");
    assert_eq!(
        restored
            .recall("durable", "bash", r#"{"command":"ls"}"#)
            .as_deref(),
        Some("SIG-DURABLE"),
        "a restart must answer from the durable snapshot"
    );
    assert_eq!(
        reopened.scope("gemini-3.8-flash-high", "other").recall(
            "durable",
            "bash",
            r#"{"command":"ls"}"#
        ),
        None,
        "restore must preserve the scope, not flatten it"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn a_bounded_snapshot_keeps_the_most_recently_used_entries() {
    let path = temp_snapshot("bounded");
    let ledger = SignatureLedger::with_config(LedgerConfig {
        snapshot_path: Some(path.clone()),
        snapshot_max_entries: 2,
        ..LedgerConfig::default()
    });
    let scope = scope(&ledger);
    scope.remember("oldest", "tool", "{}", "SIG-OLDEST");
    scope.remember("middle", "tool", "{}", "SIG-MIDDLE");
    scope.remember("newest", "tool", "{}", "SIG-NEWEST");
    assert_eq!(
        scope.recall("oldest", "tool", "{}").as_deref(),
        Some("SIG-OLDEST")
    );
    ledger.flush_blocking().expect("flush");

    let reopened = SignatureLedger::with_config(LedgerConfig {
        snapshot_path: Some(path.clone()),
        snapshot_max_entries: 2,
        ..LedgerConfig::default()
    });
    let restored = reopened.scope("gemini-3.8-flash-high", "session-a");
    assert_eq!(
        restored.recall("oldest", "tool", "{}").as_deref(),
        Some("SIG-OLDEST")
    );
    assert_eq!(restored.recall("newest", "tool", "{}").as_deref(), Some("SIG-NEWEST"));
    assert_eq!(restored.recall("middle", "tool", "{}"), None);
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn a_corrupt_snapshot_starts_empty_instead_of_failing() {
    let path = temp_snapshot("corrupt");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, b"{not json").expect("write");

    let ledger = SignatureLedger::open(&path);
    assert!(ledger.is_empty());
    let scope = scope(&ledger);
    scope.remember("after-corrupt", "tool", "{}", "SIG-OK");
    ledger.flush_blocking().expect("flush");
    assert_eq!(
        SignatureLedger::open(&path)
            .scope("gemini-3.8-flash-high", "session-a")
            .recall("after-corrupt", "tool", "{}")
            .as_deref(),
        Some("SIG-OK")
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn an_in_memory_ledger_never_writes_a_snapshot_file() {
    let ledger = SignatureLedger::in_memory();
    scope(&ledger).remember("mem", "tool", "{}", "SIG-MEM");
    ledger.flush_blocking().expect("flush");
    assert!(ledger.snapshot_path().is_none());
    assert_eq!(ledger.persist_count(), 0);
}

#[tokio::test]
async fn the_persistence_worker_coalesces_a_burst_into_one_write() {
    let path = temp_snapshot("coalesce");
    let ledger = SignatureLedger::with_config(LedgerConfig {
        snapshot_path: Some(path.clone()),
        coalesce_window: Duration::from_millis(40),
        ..LedgerConfig::default()
    });
    let worker = ledger.spawn_persistence_worker();
    let persisted = {
        let ledger = Arc::clone(&ledger);
        tokio::spawn(async move { ledger.persisted().await })
    };
    tokio::task::yield_now().await;

    let scope = scope(&ledger);
    for index in 0..32 {
        scope.remember(&format!("burst-{index}"), "tool", "{}", "SIG-BURST");
    }
    persisted.await.expect("persist notification");

    assert_eq!(ledger.write_count(), 32);
    assert_eq!(
        ledger.persist_count(),
        1,
        "a burst inside the coalescing window must cost one snapshot write"
    );
    ledger.shutdown();
    worker.await.expect("worker exit");
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[tokio::test]
async fn shutdown_flushes_the_pending_snapshot() {
    let path = temp_snapshot("shutdown");
    let ledger = SignatureLedger::with_config(LedgerConfig {
        snapshot_path: Some(path.clone()),
        coalesce_window: Duration::from_secs(3600),
        ..LedgerConfig::default()
    });
    let worker = ledger.spawn_persistence_worker();
    scope(&ledger).remember("pending", "tool", "{}", "SIG-PENDING");

    ledger.shutdown();
    worker.await.expect("worker exit");

    assert_eq!(
        SignatureLedger::open(&path)
            .scope("gemini-3.8-flash-high", "session-a")
            .recall("pending", "tool", "{}")
            .as_deref(),
        Some("SIG-PENDING")
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
}

#[test]
fn eviction_keeps_the_order_index_and_byte_total_consistent() {
    let mut store = LruStore::new(4, 1 << 20);
    let now = Instant::now();
    for index in 0..12 {
        store.insert(
            format!("key-{index}"),
            "{}".to_string(),
            "SIG".to_string(),
            now,
        );
    }

    assert_eq!(store.len(), 4);
    assert_eq!(store.order_len(), 4);
    assert_eq!(store.bytes(), store.accounted_bytes());
    assert!(!store.contains("key-0"));
    assert!(store.contains("key-11"));
}

#[test]
fn an_expired_entry_is_dropped_and_excluded_from_the_snapshot() {
    let mut store = LruStore::new(16, 1 << 20);
    let now = Instant::now();
    let stale = now.checked_sub(TTL + Duration::from_secs(1)).expect("stale");
    store.insert(
        "stale".to_string(),
        "{}".to_string(),
        "SIG-STALE".to_string(),
        stale,
    );
    store.insert(
        "fresh".to_string(),
        "{}".to_string(),
        "SIG-FRESH".to_string(),
        now,
    );

    assert_eq!(store.snapshot(16, 1 << 20, now).len(), 1);
    assert_eq!(store.get("stale", "{}", now), None);
    assert_eq!(store.get("fresh", "{}", now).as_deref(), Some("SIG-FRESH"));
    assert_eq!(store.bytes(), store.accounted_bytes());
}

#[test]
fn restore_rebuilds_recency_so_the_coldest_restored_entry_evicts_first() {
    let mut store = LruStore::new(2, 1 << 20);
    let now = Instant::now();
    let record = |key: &str, age_ms: u64| SnapshotRecord {
        key: key.to_string(),
        arguments: "{}".to_string(),
        signature: "SIG".to_string(),
        age_ms,
    };
    store.restore(vec![record("hot", 10), record("warm", 20), record("cold", 30)], now);

    assert_eq!(store.len(), 2);
    assert!(store.contains("hot"));
    assert!(store.contains("warm"));
    assert!(!store.contains("cold"));
}

#[test]
fn synthetic_ids_never_repeat() {
    let first = super::synthetic_call_id("eval");
    let second = super::synthetic_call_id("eval");
    assert_ne!(first, second);
    assert!(first.starts_with("call_eval_"));
}

#[test]
fn stats_count_replay_hits_misses_and_unsigned_replays() {
    let ledger = SignatureLedger::with_config(LedgerConfig::default());
    let scope = scope(&ledger);
    scope.remember("call-1", "tool", "{}", "SIG-1");

    assert_eq!(scope.recall("call-1", "tool", "{}").as_deref(), Some("SIG-1"));
    assert_eq!(scope.recall("call-2", "tool", "{}"), None);
    // An empty call id never reaches the store and must not be counted.
    assert_eq!(scope.recall("", "tool", "{}"), None);
    scope.record_unsigned_replay();

    let stats = ledger.stats();
    assert_eq!(stats.hits, 1, "hits: {stats:?}");
    assert_eq!(stats.misses, 1, "misses: {stats:?}");
    assert_eq!(stats.unsigned_replays, 1, "unsigned_replays: {stats:?}");
}

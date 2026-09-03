mod common;

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hasher;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::Router;
use common::unique_temp_dir;
use ed25519_dalek::SigningKey;
use mahoquot_gateway::account::{AccountMember, GenericAccount, ProviderAccount};
use mahoquot_gateway::models_route::ModelEntry;
use mahoquot_gateway::registry::{CatalogConfig, CatalogManager, LkgCache, RefreshEnqueue};
use mahoquot_gateway::state::{PoolSnapshot, UnifiedRuntimeState};
use mahoquot_registry::envelope::*;
use mahoquot_registry::*;
use tokio::sync::{oneshot, Notify};

static PORT_18878_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn hash_snapshot(snapshot: &RegistrySnapshot) -> u64 {
    let serialized = serde_json::to_vec(snapshot).expect("serialize snapshot");
    let mut hasher = DefaultHasher::new();
    hasher.write(&serialized);
    hasher.finish()
}

fn hash_file(path: &Path) -> Option<u64> {
    if !path.exists() {
        return None;
    }
    let data = fs::read(path).expect("read file for hash");
    let mut hasher = DefaultHasher::new();
    hasher.write(&data);
    Some(hasher.finish())
}

fn test_signer_and_keyring(key_id: &str) -> (CatalogSigner, Keyring) {
    let mut rng = rand::rngs::OsRng;
    let signing_key = SigningKey::generate(&mut rng);
    let keyring = Keyring::new().with_key(key_id, signing_key.verifying_key());
    let signer = CatalogSigner::new(signing_key, key_id);
    (signer, keyring)
}

fn sample_catalog(version: u64, model_name: &str) -> (RegistrySnapshot, Vec<u8>) {
    let mut builder = RegistryBuilder::new(CatalogVersion(version), CatalogSource::RemoteSigned);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    let mut model = ModelDescriptor::new(ModelId::new(model_name).unwrap(), "anthropic");
    model.capabilities.insert(ModelCapability::Chat);
    let binding = ProviderBinding::new(
        ProviderId::claude(),
        ProviderPolicy::Closed,
        CatalogSource::RemoteSigned,
    )
    .with_capabilities([ModelCapability::Chat]);
    builder.add_model(model).unwrap();
    builder
        .add_binding(ModelId::new(model_name).unwrap(), binding)
        .unwrap();
    let snapshot = builder.build().unwrap();
    let payload = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
    (snapshot, payload)
}

fn test_member(id: &str, provider: &str, models: Vec<String>) -> Arc<AccountMember> {
    let account = GenericAccount {
        identity_slug: id.to_string(),
        provider: provider.to_string(),
        label: id.to_string(),
        adapter: "chat".to_string(),
        base_url: "http://127.0.0.1:18899".to_string(),
        api_key: "key".to_string(),
        auth_mode: "bearer".to_string(),
        refresh_token: String::new(),
        expired: "2099-01-01T00:00:00Z".to_string(),
        token_url: String::new(),
        client_id: String::new(),
        project_id: String::new(),
        static_headers: Default::default(),
        disabled: false,
        models,
    };
    Arc::new(AccountMember::for_test_with_id(
        id,
        ProviderAccount::Generic(account),
    ))
}

fn assert_port_released(port: u16) {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let bound = std::net::TcpListener::bind(addr);
    assert!(
        bound.is_ok(),
        "Port {port} must be released and free to bind, but got error: {:?}",
        bound.err()
    );
}

// =========================================================================
// Scenario 1: Embedded-only boot
// =========================================================================
#[test]
fn test_lifecycle_01_embedded_only_boot() {
    let tmp = unique_temp_dir("lifecycle-01-embedded");
    let lkg_path = tmp.join("models-v1.signed.json");
    assert!(!lkg_path.exists());

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        ..Default::default()
    };

    let now = 1_700_000_000;
    let manager = CatalogManager::boot(config, now);

    let embedded = embedded_registry_snapshot().unwrap();
    assert_eq!(manager.active_source(), CatalogSource::EmbeddedFallback);
    assert_eq!(manager.active_version(), embedded.version());
    assert_eq!(
        manager.current_snapshot().models().len(),
        embedded.models().len()
    );
    assert_eq!(manager.status().lkg_version, None);
    assert!(!manager.status().stale);
    assert_eq!(manager.status().last_error, None);

    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 2: Valid cached boot
// =========================================================================
#[test]
fn test_lifecycle_02_valid_cached_boot() {
    let tmp = unique_temp_dir("lifecycle-02-valid-cached");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring("key-02");
    let (snapshot, payload) = sample_catalog(250, "claude-cached-boot");
    let now = 1_700_000_000;

    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("sign catalog");

    let cache = LkgCache::new(&lkg_path);
    cache
        .write_atomically(&envelope, &payload)
        .expect("write LKG");
    assert!(lkg_path.exists());

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        keyring,
        ..Default::default()
    };

    // Offline boot with valid LKG on disk
    let manager = CatalogManager::boot(config, now + 10);

    assert_eq!(manager.active_source(), CatalogSource::LkgCache);
    assert_eq!(manager.active_version(), CatalogVersion(250));
    assert_eq!(manager.status().lkg_version, Some(CatalogVersion(250)));
    assert!(manager
        .current_snapshot()
        .get_model(&ModelId::new("claude-cached-boot").unwrap())
        .is_some());
    assert!(!manager.status().stale);

    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 3: Successful newer remote update
// =========================================================================
#[tokio::test]
async fn test_lifecycle_03_successful_newer_remote_update() {
    let port = 18876;
    let tmp = unique_temp_dir("lifecycle-03-remote-update");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring("key-03");
    let now = 1_700_000_000;

    // Boot initial manager with LKG at version 200
    let (initial_snap, initial_payload) = sample_catalog(200, "claude-initial");
    let initial_env = signer
        .sign_catalog(initial_snap.version(), now, None, &initial_payload)
        .unwrap();
    LkgCache::new(&lkg_path)
        .write_atomically(&initial_env, &initial_payload)
        .unwrap();

    // Prepare newer remote catalog at version 300
    let (newer_snap, newer_payload) = sample_catalog(300, "claude-newer-v300");
    let newer_env = signer
        .sign_catalog(newer_snap.version(), now, None, &newer_payload)
        .unwrap();
    let newer_sig_json = newer_env.to_json().unwrap();

    struct MockServer3 {
        sig_json: String,
        payload: Vec<u8>,
        sig_requested: Arc<Notify>,
        cat_requested: Arc<Notify>,
    }

    let mock_state = Arc::new(MockServer3 {
        sig_json: newer_sig_json,
        payload: newer_payload,
        sig_requested: Arc::new(Notify::new()),
        cat_requested: Arc::new(Notify::new()),
    });

    let app = Router::new()
        .route(
            "/models-v1.json.sig",
            get(
                |headers: HeaderMap, State(state): State<Arc<MockServer3>>| async move {
                    let host = headers.get("host").unwrap().to_str().unwrap();
                    assert!(
                        host.starts_with("127.0.0.1"),
                        "outbound request must target 127.0.0.1, got {host}"
                    );
                    state.sig_requested.notify_one();
                    (StatusCode::OK, state.sig_json.clone())
                },
            ),
        )
        .route(
            "/models-v1.json",
            get(
                |headers: HeaderMap, State(state): State<Arc<MockServer3>>| async move {
                    let host = headers.get("host").unwrap().to_str().unwrap();
                    assert!(
                        host.starts_with("127.0.0.1"),
                        "outbound request must target 127.0.0.1, got {host}"
                    );
                    state.cat_requested.notify_one();
                    (StatusCode::OK, state.payload.clone())
                },
            ),
        )
        .with_state(mock_state.clone());

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        remote_catalog_url: Some(format!("http://127.0.0.1:{port}/models-v1.json")),
        remote_signature_url: Some(format!("http://127.0.0.1:{port}/models-v1.json.sig")),
        keyring: keyring.clone(),
        ..Default::default()
    };

    let manager = CatalogManager::boot(config.clone(), now);
    assert_eq!(manager.active_version(), CatalogVersion(200));

    // Subscribe to explicit request events before trigger
    let sig_notified = mock_state.sig_requested.notified();
    let cat_notified = mock_state.cat_requested.notified();

    // Trigger update
    let update_res = manager.fetch_and_update().await;
    assert!(update_res.is_ok(), "fetch_and_update should succeed");

    tokio::time::timeout(Duration::from_secs(5), sig_notified)
        .await
        .expect("timed out waiting for signature request");
    tokio::time::timeout(Duration::from_secs(5), cat_notified)
        .await
        .expect("timed out waiting for catalog payload request");

    // Active state updated
    assert_eq!(manager.active_source(), CatalogSource::RemoteSigned);
    assert_eq!(manager.active_version(), CatalogVersion(300));
    assert!(manager
        .current_snapshot()
        .get_model(&ModelId::new("claude-newer-v300").unwrap())
        .is_some());

    // Assert LKG cache file on disk updated
    assert!(lkg_path.exists());

    // Assert newer valid update survives restart offline
    let reboot_config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        keyring,
        ..Default::default()
    };
    let rebooted = CatalogManager::boot(reboot_config, now + 100);
    assert_eq!(rebooted.active_source(), CatalogSource::LkgCache);
    assert_eq!(rebooted.active_version(), CatalogVersion(300));
    assert!(rebooted
        .current_snapshot()
        .get_model(&ModelId::new("claude-newer-v300").unwrap())
        .is_some());

    // Cleanup server
    let _ = shutdown_tx.send(());
    server_handle.await.unwrap();
    assert_port_released(port);

    // Cleanup temp dir
    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 4: 304 Not Modified
// =========================================================================
#[tokio::test]
async fn test_lifecycle_04_304_not_modified() {
    let port = 18877;
    let tmp = unique_temp_dir("lifecycle-04-not-modified");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring("key-04");
    let now = 1_700_000_000;

    // Boot initial manager with LKG at version 400
    let (initial_snap, initial_payload) = sample_catalog(400, "claude-v400");
    let initial_env = signer
        .sign_catalog(initial_snap.version(), now, None, &initial_payload)
        .unwrap();
    LkgCache::new(&lkg_path)
        .write_atomically(&initial_env, &initial_payload)
        .unwrap();

    struct MockServer4 {
        sig_requested: Arc<Notify>,
    }

    let mock_state = Arc::new(MockServer4 {
        sig_requested: Arc::new(Notify::new()),
    });

    // Server responds with 304 Not Modified to signature request
    let app = Router::new()
        .route(
            "/models-v1.json.sig",
            get(
                |headers: HeaderMap, State(state): State<Arc<MockServer4>>| async move {
                    let host = headers.get("host").unwrap().to_str().unwrap();
                    assert!(
                        host.starts_with("127.0.0.1"),
                        "outbound request must target 127.0.0.1, got {host}"
                    );
                    state.sig_requested.notify_one();
                    StatusCode::NOT_MODIFIED
                },
            ),
        )
        .with_state(mock_state.clone());

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        remote_catalog_url: Some(format!("http://127.0.0.1:{port}/models-v1.json")),
        remote_signature_url: Some(format!("http://127.0.0.1:{port}/models-v1.json.sig")),
        keyring,
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    assert_eq!(manager.active_version(), CatalogVersion(400));

    // Record hashes prior to 304 update
    let pre_active_hash = hash_snapshot(&manager.current_snapshot());
    let pre_lkg_hash = hash_file(&lkg_path).expect("LKG file must exist");

    let sig_notified = mock_state.sig_requested.notified();

    // Trigger update
    let result = manager.fetch_and_update().await;
    assert!(
        result.is_ok(),
        "304 Not Modified must be treated as successful refresh: {:?}",
        result.err()
    );

    tokio::time::timeout(Duration::from_secs(5), sig_notified)
        .await
        .expect("timed out waiting for signature request");

    // Hashes must remain 100% unchanged
    let post_active_hash = hash_snapshot(&manager.current_snapshot());
    let post_lkg_hash = hash_file(&lkg_path).expect("LKG file must exist");
    assert_eq!(
        pre_active_hash, post_active_hash,
        "active snapshot hash must remain unchanged on 304"
    );
    assert_eq!(
        pre_lkg_hash, post_lkg_hash,
        "LKG file hash must remain unchanged on 304"
    );

    // Status assertions
    assert!(
        manager.status().last_refresh_success,
        "last_refresh_success must be true on 304"
    );
    assert!(
        !manager.status().stale,
        "catalog must not be marked stale on 304"
    );

    // Cleanup server
    let _ = shutdown_tx.send(());
    server_handle.await.unwrap();
    assert_port_released(port);

    // Cleanup temp dir
    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 5: Stale cache offline
// =========================================================================
#[tokio::test]
async fn test_lifecycle_05_stale_cache_offline() {
    let tmp = unique_temp_dir("lifecycle-05-stale-offline");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring("key-05");
    let now = 1_700_000_000;

    let (snap, payload) = sample_catalog(500, "claude-v500");
    let env = signer
        .sign_catalog(snap.version(), now, None, &payload)
        .unwrap();
    LkgCache::new(&lkg_path)
        .write_atomically(&env, &payload)
        .unwrap();

    // Configure remote to port 18879 where no server is running (dedicated offline port)
    let offline_port = 18879;
    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        remote_catalog_url: Some(format!("http://127.0.0.1:{offline_port}/models-v1.json")),
        remote_signature_url: Some(format!(
            "http://127.0.0.1:{offline_port}/models-v1.json.sig"
        )),
        keyring,
        request_timeout: Duration::from_millis(500),
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    assert_eq!(manager.active_version(), CatalogVersion(500));
    assert_eq!(manager.active_source(), CatalogSource::LkgCache);

    let pre_active_hash = hash_snapshot(&manager.current_snapshot());
    let pre_lkg_hash = hash_file(&lkg_path).expect("LKG file must exist");

    // Trigger update while offline
    let result = manager.fetch_and_update().await;
    assert!(result.is_err(), "offline update must return error");

    // Hashes must remain 100% unchanged
    let post_active_hash = hash_snapshot(&manager.current_snapshot());
    let post_lkg_hash = hash_file(&lkg_path).expect("LKG file must exist");
    assert_eq!(
        pre_active_hash, post_active_hash,
        "active hash must be unchanged offline"
    );
    assert_eq!(
        pre_lkg_hash, post_lkg_hash,
        "LKG hash must be unchanged offline"
    );

    // Status assertions: stale valid LKG remains usable
    assert!(manager.status().stale, "catalog must be marked stale");
    assert!(
        !manager.status().last_refresh_success,
        "last_refresh_success must be false"
    );
    assert!(
        manager.status().last_error.is_some(),
        "last_error must be recorded"
    );
    assert_eq!(manager.active_version(), CatalogVersion(500));
    assert!(manager
        .current_snapshot()
        .get_model(&ModelId::new("claude-v500").unwrap())
        .is_some());

    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 6: Signature/schema/downgrade/malformed rejection
// =========================================================================
#[test]
fn test_lifecycle_06_rejection_preserves_hashes() {
    let tmp = unique_temp_dir("lifecycle-06-rejection");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring("key-06");
    let now = 1_700_000_000;

    let (snap, payload) = sample_catalog(600, "claude-v600");
    let env = signer
        .sign_catalog(snap.version(), now, None, &payload)
        .unwrap();
    LkgCache::new(&lkg_path)
        .write_atomically(&env, &payload)
        .unwrap();

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        keyring,
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    assert_eq!(manager.active_version(), CatalogVersion(600));

    let base_active_hash = hash_snapshot(&manager.current_snapshot());
    let base_lkg_hash = hash_file(&lkg_path).expect("LKG file exists");

    // Case A: Signature tampering
    {
        let (s700, p700) = sample_catalog(700, "claude-v700-tampered");
        let mut env_tampered = signer
            .sign_catalog(s700.version(), now, None, &p700)
            .unwrap();
        // Tamper signature string
        let mut sig_chars: Vec<char> = env_tampered.signature.chars().collect();
        sig_chars[0] = if sig_chars[0] == 'A' { 'B' } else { 'A' };
        env_tampered.signature = sig_chars.into_iter().collect();

        let res = manager.apply_verified_update(&env_tampered, &p700, now);
        assert!(res.is_err(), "tampered signature must be rejected");
        assert_eq!(hash_snapshot(&manager.current_snapshot()), base_active_hash);
        assert_eq!(hash_file(&lkg_path).unwrap(), base_lkg_hash);
    }

    // Case B: Incompatible schema version
    {
        let (s701, p701) = sample_catalog(701, "claude-v701-bad-schema");
        let mut env_bad_schema = signer
            .sign_catalog(s701.version(), now, None, &p701)
            .unwrap();
        env_bad_schema.schema_version = 999; // Incompatible future schema

        let res = manager.apply_verified_update(&env_bad_schema, &p701, now);
        assert!(res.is_err(), "incompatible schema version must be rejected");
        assert_eq!(hash_snapshot(&manager.current_snapshot()), base_active_hash);
        assert_eq!(hash_file(&lkg_path).unwrap(), base_lkg_hash);
    }

    // Case C: Downgrade / equal version
    {
        let (s500, p500) = sample_catalog(500, "claude-v500-downgrade");
        let env_downgrade = signer
            .sign_catalog(s500.version(), now, None, &p500)
            .unwrap();

        let res = manager.apply_verified_update(&env_downgrade, &p500, now);
        assert!(res.is_err(), "downgrade must be rejected");
        assert_eq!(hash_snapshot(&manager.current_snapshot()), base_active_hash);
        assert_eq!(hash_file(&lkg_path).unwrap(), base_lkg_hash);

        // Equal version 600
        let (s600_dup, p600_dup) = sample_catalog(600, "claude-v600-equal");
        let env_equal = signer
            .sign_catalog(s600_dup.version(), now, None, &p600_dup)
            .unwrap();
        let res_eq = manager.apply_verified_update(&env_equal, &p600_dup, now);
        assert!(res_eq.is_err(), "equal version must be rejected");
        assert_eq!(hash_snapshot(&manager.current_snapshot()), base_active_hash);
        assert_eq!(hash_file(&lkg_path).unwrap(), base_lkg_hash);
    }

    // Case D: Malformed payload JSON
    {
        let (s750, p750) = sample_catalog(750, "claude-v750");
        let env_valid = signer
            .sign_catalog(s750.version(), now, None, &p750)
            .unwrap();
        let invalid_payload = b"{\"invalid_json\": [unclosed";

        let res = manager.apply_verified_update(&env_valid, invalid_payload, now);
        assert!(res.is_err(), "malformed json payload must be rejected");
        assert_eq!(hash_snapshot(&manager.current_snapshot()), base_active_hash);
        assert_eq!(hash_file(&lkg_path).unwrap(), base_lkg_hash);
    }

    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 7: Interrupted persistence
// =========================================================================
#[test]
fn test_lifecycle_07_interrupted_persistence() {
    let tmp = unique_temp_dir("lifecycle-07-interrupted-persistence");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, _keyring) = test_signer_and_keyring("key-07");
    let now = 1_700_000_000;

    let (snap, payload) = sample_catalog(700, "claude-v700");
    let env = signer
        .sign_catalog(snap.version(), now, None, &payload)
        .unwrap();
    LkgCache::new(&lkg_path)
        .write_atomically(&env, &payload)
        .unwrap();

    let pre_lkg_hash = hash_file(&lkg_path).expect("LKG file exists");

    // Make parent directory read-only to interrupt atomic write of next update
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp).unwrap().permissions();
        perms.set_mode(0o555); // read and execute, no write
        fs::set_permissions(&tmp, perms).unwrap();

        let (s800, p800) = sample_catalog(800, "claude-v800");
        let env800 = signer
            .sign_catalog(s800.version(), now, None, &p800)
            .unwrap();

        // Attempt write_atomically under read-only directory
        let cache = LkgCache::new(&lkg_path);
        let write_res = cache.write_atomically(&env800, &p800);
        assert!(
            write_res.is_err(),
            "write must fail when directory is not writable"
        );

        // Restore permissions so we can inspect and cleanup
        let mut restore = fs::metadata(&tmp).unwrap().permissions();
        restore.set_mode(0o755);
        fs::set_permissions(&tmp, restore).unwrap();
    }

    // Existing LKG file must NOT be deleted or corrupted
    let post_lkg_hash = hash_file(&lkg_path).expect("LKG file must still exist intact");
    assert_eq!(
        pre_lkg_hash, post_lkg_hash,
        "interrupted write must leave original LKG completely intact"
    );

    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 8: Overlapping timer/manual/rescan/settings refresh
// =========================================================================
#[tokio::test]
async fn test_lifecycle_08_overlapping_refresh_coalescing() {
    let _port_lock = PORT_18878_MUTEX.lock().await;
    let port = 18878;
    let tmp = unique_temp_dir("lifecycle-08-coalesce");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring("key-08");
    let now = 1_700_000_000;

    let (snap, payload) = sample_catalog(800, "claude-v800");
    let env = signer
        .sign_catalog(snap.version(), now, None, &payload)
        .unwrap();
    let sig_json = env.to_json().unwrap();

    struct MockServer8 {
        sig_json: String,
        payload: Vec<u8>,
        requests_count: AtomicU64,
        request_started: Arc<Notify>,
        release_response: Arc<Notify>,
    }

    let mock_state = Arc::new(MockServer8 {
        sig_json,
        payload,
        requests_count: AtomicU64::new(0),
        request_started: Arc::new(Notify::new()),
        release_response: Arc::new(Notify::new()),
    });

    let app = Router::new()
        .route(
            "/models-v1.json.sig",
            get(
                |headers: HeaderMap, State(state): State<Arc<MockServer8>>| async move {
                    let host = headers.get("host").unwrap().to_str().unwrap();
                    assert!(host.starts_with("127.0.0.1"));
                    state.requests_count.fetch_add(1, Ordering::SeqCst);
                    state.request_started.notify_one();
                    // Block until test releases response
                    state.release_response.notified().await;
                    (StatusCode::OK, state.sig_json.clone())
                },
            ),
        )
        .route(
            "/models-v1.json",
            get(|State(state): State<Arc<MockServer8>>| async move {
                (StatusCode::OK, state.payload.clone())
            }),
        )
        .with_state(mock_state.clone());

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let config = CatalogConfig {
        cache_path: Some(lkg_path),
        remote_catalog_url: Some(format!("http://127.0.0.1:{port}/models-v1.json")),
        remote_signature_url: Some(format!("http://127.0.0.1:{port}/models-v1.json.sig")),
        keyring,
        ..Default::default()
    };

    let manager = Arc::new(CatalogManager::boot(config, now));
    let initial_seq = manager.refresh_completion_seq();

    let started = mock_state.request_started.notified();

    // 1st refresh trigger: Accepted
    let r1 = manager.enqueue_refresh();
    assert_eq!(r1, RefreshEnqueue::Accepted);

    // Wait until 1st refresh is in flight on the server
    tokio::time::timeout(Duration::from_secs(5), started)
        .await
        .expect("timed out waiting for 1st request to hit server");
    assert!(manager.refresh_in_flight());

    // 2nd, 3rd, 4th refresh triggers: must be Coalesced
    let r2 = manager.enqueue_refresh();
    let r3 = manager.enqueue_refresh();
    let r4 = manager.enqueue_refresh();
    assert_eq!(r2, RefreshEnqueue::Coalesced);
    assert_eq!(r3, RefreshEnqueue::Coalesced);
    assert_eq!(r4, RefreshEnqueue::Coalesced);

    // Release the response
    mock_state.release_response.notify_one();

    // Await completion via bounded event
    tokio::time::timeout(
        Duration::from_secs(5),
        manager.wait_for_refresh_after(initial_seq),
    )
    .await
    .expect("timed out waiting for refresh completion");

    assert!(!manager.refresh_in_flight());
    assert_eq!(
        mock_state.requests_count.load(Ordering::SeqCst),
        1,
        "only 1 request must hit the server due to coalescing"
    );
    assert_eq!(manager.active_version(), CatalogVersion(800));

    // Cleanup server
    let _ = shutdown_tx.send(());
    server_handle.await.unwrap();
    assert_port_released(port);

    // Cleanup temp dir
    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 9: Account add/delete during refresh
// =========================================================================
#[tokio::test]
async fn test_lifecycle_09_account_add_delete_during_refresh() {
    let _port_lock = PORT_18878_MUTEX.lock().await;
    let port = 18878;
    let tmp = unique_temp_dir("lifecycle-09-account-refresh");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring("key-09");
    let now = 1_700_000_000;

    // Initial catalog V1 has model-1
    let (snap_v1, payload_v1) = sample_catalog(1, "claude-v1");
    let env_v1 = signer
        .sign_catalog(snap_v1.version(), now, None, &payload_v1)
        .unwrap();
    LkgCache::new(&lkg_path)
        .write_atomically(&env_v1, &payload_v1)
        .unwrap();

    // Catalog V2 adds model-2
    let mut builder_v2 = RegistryBuilder::new(CatalogVersion(2), CatalogSource::RemoteSigned);
    builder_v2.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    let mut m1 = ModelDescriptor::new(ModelId::new("claude-v1").unwrap(), "anthropic");
    m1.capabilities.insert(ModelCapability::Chat);
    let mut m2 = ModelDescriptor::new(ModelId::new("claude-v2").unwrap(), "anthropic");
    m2.capabilities.insert(ModelCapability::Chat);
    let b1 = ProviderBinding::new(
        ProviderId::claude(),
        ProviderPolicy::Closed,
        CatalogSource::RemoteSigned,
    )
    .with_capabilities([ModelCapability::Chat]);
    let b2 = ProviderBinding::new(
        ProviderId::claude(),
        ProviderPolicy::Closed,
        CatalogSource::RemoteSigned,
    )
    .with_capabilities([ModelCapability::Chat]);
    builder_v2.add_model(m1).unwrap();
    builder_v2
        .add_binding(ModelId::new("claude-v1").unwrap(), b1)
        .unwrap();
    builder_v2.add_model(m2).unwrap();
    builder_v2
        .add_binding(ModelId::new("claude-v2").unwrap(), b2)
        .unwrap();
    let snap_v2 = builder_v2.build().unwrap();
    let payload_v2 = canonicalize_json(&serde_json::to_vec(&snap_v2).unwrap()).unwrap();
    let env_v2 = signer
        .sign_catalog(snap_v2.version(), now, None, &payload_v2)
        .unwrap();
    let sig_json_v2 = env_v2.to_json().unwrap();

    struct MockServer9 {
        sig_json: String,
        payload: Vec<u8>,
        request_started: Arc<Notify>,
        release_response: Arc<Notify>,
    }

    let mock_state = Arc::new(MockServer9 {
        sig_json: sig_json_v2,
        payload: payload_v2,
        request_started: Arc::new(Notify::new()),
        release_response: Arc::new(Notify::new()),
    });

    let app = Router::new()
        .route(
            "/models-v1.json.sig",
            get(
                |headers: HeaderMap, State(state): State<Arc<MockServer9>>| async move {
                    let host = headers.get("host").unwrap().to_str().unwrap();
                    assert!(host.starts_with("127.0.0.1"));
                    state.request_started.notify_one();
                    state.release_response.notified().await;
                    (StatusCode::OK, state.sig_json.clone())
                },
            ),
        )
        .route(
            "/models-v1.json",
            get(|State(state): State<Arc<MockServer9>>| async move {
                (StatusCode::OK, state.payload.clone())
            }),
        )
        .with_state(mock_state.clone());

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let config = CatalogConfig {
        cache_path: Some(lkg_path),
        remote_catalog_url: Some(format!("http://127.0.0.1:{port}/models-v1.json")),
        remote_signature_url: Some(format!("http://127.0.0.1:{port}/models-v1.json.sig")),
        keyring,
        ..Default::default()
    };

    let member_a = test_member("acc-a", "claude", vec![]);
    let initial_pool = PoolSnapshot::new(
        1,
        vec![member_a.clone()],
        vec![ModelEntry {
            id: "claude-v1".to_string(),
            owned_by: "claude".to_string(),
        }],
        Arc::new(snap_v1),
    );

    let runtime = Arc::new(UnifiedRuntimeState::new(initial_pool, None));
    let manager = Arc::new(CatalogManager::boot(config, now));
    manager.bind_runtime(&runtime);

    let started = mock_state.request_started.notified();
    let seq = manager.refresh_completion_seq();

    // Trigger refresh to V2
    assert_eq!(manager.enqueue_refresh(), RefreshEnqueue::Accepted);

    // Wait until refresh is in-flight on the server
    tokio::time::timeout(Duration::from_secs(5), started)
        .await
        .expect("timed out waiting for refresh to hit server");

    // Add Account B while refresh is in-flight
    let member_b = test_member("acc-b", "claude", vec![]);
    runtime
        .reload_accounts(vec![member_a.clone(), member_b.clone()])
        .expect("reload accounts");

    // Release server response
    mock_state.release_response.notify_one();

    // Wait for refresh to complete
    tokio::time::timeout(Duration::from_secs(5), manager.wait_for_refresh_after(seq))
        .await
        .expect("timed out waiting for refresh completion");

    // Verify atomic state: both accounts and catalog V2 are present
    let snap_after_refresh = runtime.load();
    assert_eq!(snap_after_refresh.registry().version(), CatalogVersion(2));
    assert_eq!(snap_after_refresh.members().len(), 2);
    assert_eq!(
        snap_after_refresh
            .routable_accounts_for_model("claude-v2")
            .len(),
        2
    );
    assert_eq!(
        snap_after_refresh
            .routable_accounts_for_model("claude-v1")
            .len(),
        2
    );

    // Delete Account A while system is running
    runtime
        .reload_accounts(vec![member_b.clone()])
        .expect("delete account a");

    let final_snap = runtime.load();
    assert_eq!(final_snap.members().len(), 1);
    assert_eq!(final_snap.members()[0].id, "acc-b");
    assert_eq!(final_snap.routable_accounts_for_model("claude-v1").len(), 1);
    assert_eq!(final_snap.routable_accounts_for_model("claude-v2").len(), 1);

    // Cleanup server
    let _ = shutdown_tx.send(());
    server_handle.await.unwrap();
    assert_port_released(port);

    // Cleanup temp dir
    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 10: In-flight request generation consistency
// =========================================================================
#[test]
fn test_lifecycle_10_inflight_request_generation_consistency() {
    let tmp = unique_temp_dir("lifecycle-10-consistency");

    let initial_reg = Arc::new(sample_catalog(1, "model-v1").0);
    let member_1 = test_member("acc-1", "claude", vec!["model-v1".to_string()]);
    let initial_pool = PoolSnapshot::new(
        1,
        vec![member_1],
        vec![ModelEntry {
            id: "model-v1".to_string(),
            owned_by: "claude".to_string(),
        }],
        initial_reg,
    );

    let runtime = Arc::new(UnifiedRuntimeState::new(initial_pool, None));
    let readers_active = Arc::new(AtomicBool::new(true));
    let split_brain_count = Arc::new(AtomicU64::new(0));
    let total_reads = Arc::new(AtomicU64::new(0));

    let mut reader_handles = Vec::new();
    // 8 concurrent reader threads, each executing 500 atomic reads
    for _ in 0..8 {
        let rt = Arc::clone(&runtime);
        let split = Arc::clone(&split_brain_count);
        let reads = Arc::clone(&total_reads);

        reader_handles.push(std::thread::spawn(move || {
            for _ in 0..500 {
                let snap = rt.load();
                let gen = snap.generation();
                let reg_ver = snap.registry().version().as_u64();
                reads.fetch_add(1, Ordering::Relaxed);

                // Generation must always equal registry version
                if gen != reg_ver {
                    split.fetch_add(1, Ordering::SeqCst);
                }

                // Check model list consistency with generation
                let expected_model = format!("model-v{gen}");
                if !snap.models().iter().any(|m| m.id == expected_model) {
                    split.fetch_add(1, Ordering::SeqCst);
                }

                // Check routable accounts consistency with generation
                let routable = snap.routable_accounts_for_model(&expected_model);
                if routable.is_empty() {
                    split.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    // Writer cycles generations while readers are running
    let writer_rt = Arc::clone(&runtime);
    let writer_active = Arc::clone(&readers_active);
    let writer_handle = std::thread::spawn(move || {
        let mut target_gen = 2u64;
        while writer_active.load(Ordering::Relaxed) {
            let model_name = format!("model-v{target_gen}");
            let reg = Arc::new(sample_catalog(target_gen, &model_name).0);
            let member = test_member("acc-dyn", "claude", vec![model_name.clone()]);

            let candidate = PoolSnapshot::new(
                target_gen,
                vec![member],
                vec![ModelEntry {
                    id: model_name,
                    owned_by: "claude".to_string(),
                }],
                reg,
            );

            writer_rt.publish_candidate(candidate).unwrap();
            target_gen += 1;
            std::hint::spin_loop();
        }
        target_gen
    });

    // Wait for all 8 readers to complete their 500 observations each
    for h in reader_handles {
        h.join().unwrap();
    }

    // Signal writer to finish and wait for it
    readers_active.store(false, Ordering::Relaxed);
    let last_written_gen = writer_handle.join().unwrap();

    assert_eq!(
        split_brain_count.load(Ordering::SeqCst),
        0,
        "readers must observe zero half-states or inconsistent generation pairings"
    );
    assert_eq!(
        total_reads.load(Ordering::Relaxed),
        4000,
        "readers must perform exactly 4000 concurrent observations"
    );
    assert!(
        last_written_gen > 2,
        "writer must have advanced generations concurrently"
    );

    let final_snap = runtime.load();
    assert_eq!(
        final_snap.generation(),
        final_snap.registry().version().as_u64()
    );

    let _ = fs::remove_dir_all(&tmp);
    assert!(!tmp.exists(), "temp directory must be cleaned up");
}

// =========================================================================
// Scenario 11: Equal version re-fetch is treated as up-to-date no-op success
// =========================================================================
#[tokio::test]
async fn test_lifecycle_11_equal_version_refetch_is_noop_success() {
    let tmp = unique_temp_dir("lifecycle-11-equal-version");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring("key-11");
    let now = 1_700_000_000;

    // Boot initial manager with LKG at version 500
    let (initial_snap, initial_payload) = sample_catalog(500, "claude-v500");
    let initial_env = signer
        .sign_catalog(initial_snap.version(), now, None, &initial_payload)
        .unwrap();
    LkgCache::new(&lkg_path)
        .write_atomically(&initial_env, &initial_payload)
        .unwrap();

    // Prepare remote catalog ALSO at version 500 (equal version)
    let sig_json = initial_env.to_json().unwrap();

    struct MockServer11 {
        sig_json: String,
        payload: Vec<u8>,
    }

    let mock_state = Arc::new(MockServer11 {
        sig_json,
        payload: initial_payload,
    });

    let app = Router::new()
        .route(
            "/models-v1.json.sig",
            get(|State(state): State<Arc<MockServer11>>| async move {
                (StatusCode::OK, state.sig_json.clone())
            }),
        )
        .route(
            "/models-v1.json",
            get(|State(state): State<Arc<MockServer11>>| async move {
                (StatusCode::OK, state.payload.clone())
            }),
        )
        .with_state(mock_state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        remote_catalog_url: Some(format!("http://127.0.0.1:{port}/models-v1.json")),
        remote_signature_url: Some(format!("http://127.0.0.1:{port}/models-v1.json.sig")),
        keyring: keyring.clone(),
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    assert_eq!(manager.active_version(), CatalogVersion(500));

    // Equal version re-fetch must NOT fail with VersionDowngrade, but succeed as an up-to-date no-op!
    let update_res = manager.fetch_and_update().await;
    assert!(update_res.is_ok(), "equal version re-fetch should succeed: {:?}", update_res.err());

    let status = manager.status();
    assert_eq!(status.active_version, CatalogVersion(500));
    assert!(status.last_refresh_success, "last_refresh_success must be true");
    assert!(!status.stale, "catalog must not be stale");
    assert!(status.last_rejection_reason.is_none());
    assert!(status.last_error.is_none());

    // Cleanup server
    let _ = shutdown_tx.send(());
    server_handle.await.unwrap();
    assert_port_released(port);

    // Cleanup temp dir
    let _ = fs::remove_dir_all(&tmp);
}

// =========================================================================
// Scenario 12: Stream bounded response reading rejects oversized responses
// =========================================================================
#[tokio::test]
async fn test_lifecycle_12_stream_bounded_response_reading() {
    let tmp = unique_temp_dir("lifecycle-12-stream-bounded");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring("key-12");
    let now = 1_700_000_000;

    let (initial_snap, initial_payload) = sample_catalog(100, "claude-v100");
    let initial_env = signer
        .sign_catalog(initial_snap.version(), now, None, &initial_payload)
        .unwrap();
    LkgCache::new(&lkg_path)
        .write_atomically(&initial_env, &initial_payload)
        .unwrap();

    // Oversized signature response (exceeding cap of 100 bytes)
    let app = Router::new()
        .route(
            "/models-v1.json.sig",
            get(|| async move {
                (StatusCode::OK, "x".repeat(200))
            }),
        )
        .route(
            "/models-v1.json",
            get(|| async move {
                (StatusCode::OK, "y".repeat(200))
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        remote_catalog_url: Some(format!("http://127.0.0.1:{port}/models-v1.json")),
        remote_signature_url: Some(format!("http://127.0.0.1:{port}/models-v1.json.sig")),
        keyring: keyring.clone(),
        max_response_bytes: 100, // Small cap
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    let update_res = manager.fetch_and_update().await;
    assert!(update_res.is_err(), "oversized response must be rejected");
    let err_msg = update_res.unwrap_err().to_string();
    assert!(err_msg.contains("exceeds cap"), "error must indicate exceeding cap, got: {err_msg}");

    // Active version remains unchanged
    assert_eq!(manager.active_version(), CatalogVersion(100));

    // Cleanup server
    let _ = shutdown_tx.send(());
    server_handle.await.unwrap();
    assert_port_released(port);

    let _ = fs::remove_dir_all(&tmp);
}

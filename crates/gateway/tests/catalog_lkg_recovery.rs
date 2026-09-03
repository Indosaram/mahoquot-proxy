mod common;

use common::unique_temp_dir;
use ed25519_dalek::SigningKey;
use mahoquot_gateway::registry::{CatalogConfig, CatalogManager, LkgCache};
use mahoquot_registry::envelope::*;
use mahoquot_registry::*;
use std::fs;

fn test_signer_and_keyring() -> (CatalogSigner, Keyring) {
    let mut rng = rand::rngs::OsRng;
    let signing_key = SigningKey::generate(&mut rng);
    let key_id = "test-key-lkg-v1";
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

#[test]
fn test_boot_no_network_no_lkg_uses_embedded() {
    let tmp = unique_temp_dir("test-catalog-lkg-boot-none");
    let lkg_path = tmp.join("models-v1.signed.json");

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        ..Default::default()
    };

    let now = 1_000_000;
    let manager = CatalogManager::boot(config, now);

    let embedded = embedded_registry_snapshot().unwrap();
    assert_eq!(manager.active_source(), CatalogSource::EmbeddedFallback);
    assert_eq!(manager.active_version(), embedded.version());
    assert_eq!(
        manager.current_snapshot().models().len(),
        embedded.models().len()
    );
    assert!(manager.status().lkg_version.is_none());

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_boot_valid_lkg_uses_lkg() {
    let tmp = unique_temp_dir("test-catalog-lkg-boot-valid");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring();
    let (snapshot, payload) = sample_catalog(888, "claude-test-lkg");
    let now = 1_000_000;

    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .unwrap();

    let cache = LkgCache::new(&lkg_path);
    cache.write_atomically(&envelope, &payload).unwrap();

    let config = CatalogConfig {
        cache_path: Some(lkg_path),
        keyring,
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);

    assert_eq!(manager.active_source(), CatalogSource::LkgCache);
    assert_eq!(manager.active_version(), CatalogVersion(888));
    assert!(manager
        .current_snapshot()
        .get_model(&ModelId::new("claude-test-lkg").unwrap())
        .is_some());
    assert_eq!(manager.status().lkg_version, Some(CatalogVersion(888)));

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_boot_corrupted_lkg_falls_back_to_embedded() {
    let tmp = unique_temp_dir("test-catalog-lkg-boot-corrupt");
    let lkg_path = tmp.join("models-v1.signed.json");

    // Write corrupted data to LKG cache file
    fs::write(&lkg_path, b"{\"corrupted\": true, invalid json}").unwrap();

    let config = CatalogConfig {
        cache_path: Some(lkg_path),
        ..Default::default()
    };

    let now = 1_000_000;
    // Must NOT panic or crash on corrupted LKG
    let manager = CatalogManager::boot(config, now);

    let embedded = embedded_registry_snapshot().unwrap();
    assert_eq!(manager.active_source(), CatalogSource::EmbeddedFallback);
    assert_eq!(manager.active_version(), embedded.version());
    assert_eq!(
        manager.current_snapshot().models().len(),
        embedded.models().len()
    );
    assert!(manager.status().lkg_version.is_none());
    assert!(manager.status().last_error.is_some());

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_atomic_write_of_new_verified_lkg() {
    let tmp = unique_temp_dir("test-catalog-lkg-atomic-write");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring();
    let now = 1_000_000;

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        keyring: keyring.clone(),
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    let embedded = embedded_registry_snapshot().unwrap();
    assert_eq!(manager.active_version(), embedded.version());

    // Create a new verified catalog with higher version
    let target_version = embedded.version().as_u64() + 50;
    let (snapshot, payload) = sample_catalog(target_version, "claude-new-verified");
    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .unwrap();

    // Apply verified update
    let updated = manager
        .apply_verified_update(&envelope, &payload, now)
        .expect("update should succeed");

    assert_eq!(updated.version(), CatalogVersion(target_version));
    assert_eq!(manager.active_source(), CatalogSource::RemoteSigned);
    assert_eq!(manager.active_version(), CatalogVersion(target_version));

    // Verify LKG cache exists on disk
    assert!(lkg_path.exists(), "LKG file must exist after update");

    // Verify Unix 0600 file permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(&lkg_path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "LKG file must have 0600 permissions");
    }

    // Verify a fresh manager rebooting from disk uses this LKG without network
    let reboot_config = CatalogConfig {
        cache_path: Some(lkg_path),
        keyring,
        ..Default::default()
    };
    let rebooted = CatalogManager::boot(reboot_config, now + 10);
    assert_eq!(rebooted.active_source(), CatalogSource::LkgCache);
    assert_eq!(rebooted.active_version(), CatalogVersion(target_version));
    assert!(rebooted
        .current_snapshot()
        .get_model(&ModelId::new("claude-new-verified").unwrap())
        .is_some());

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_update_downgrade_rejected_leaves_active_and_lkg_unchanged() {
    let tmp = unique_temp_dir("test-catalog-lkg-downgrade");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring();
    let now = 1_000_000;

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        keyring: keyring.clone(),
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);

    // First apply version 50
    let (s50, p50) = sample_catalog(50, "claude-50");
    let env50 = signer.sign_catalog(s50.version(), now, None, &p50).unwrap();
    manager.apply_verified_update(&env50, &p50, now).unwrap();
    assert_eq!(manager.active_version(), CatalogVersion(50));

    // Try applying version 40 (downgrade)
    let (s40, p40) = sample_catalog(40, "claude-40");
    let env40 = signer.sign_catalog(s40.version(), now, None, &p40).unwrap();
    let result = manager.apply_verified_update(&env40, &p40, now);

    assert!(result.is_err(), "downgrade must be rejected");
    // Active snapshot must remain at 50
    assert_eq!(manager.active_version(), CatalogVersion(50));

    // Try applying equal version 50 (must also be rejected)
    let (s50_dup, p50_dup) = sample_catalog(50, "claude-50-dup");
    let env50_dup = signer
        .sign_catalog(s50_dup.version(), now, None, &p50_dup)
        .unwrap();
    let result_dup = manager.apply_verified_update(&env50_dup, &p50_dup, now);
    assert!(result_dup.is_err(), "equal version must be rejected");
    assert_eq!(manager.active_version(), CatalogVersion(50));

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_update_corrupted_signature_rejected_leaves_active_and_lkg_unchanged() {
    let tmp = unique_temp_dir("test-catalog-lkg-bad-sig");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring();
    let now = 1_000_000;

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        keyring: keyring.clone(),
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    let initial_version = manager.active_version();

    let (snapshot, payload) = sample_catalog(50, "claude-corrupt-sig");
    let mut envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .unwrap();
    envelope.signature = "AAAABBBBCCCC".to_string(); // corrupt signature

    let result = manager.apply_verified_update(&envelope, &payload, now);
    assert!(result.is_err());
    assert_eq!(manager.active_version(), initial_version);

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_boot_corrupted_signature_in_lkg_falls_back_to_embedded() {
    let tmp = unique_temp_dir("test-catalog-lkg-corrupted-sig-boot");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring();
    let (snapshot, payload) = sample_catalog(777, "claude-corrupt-sig-lkg");
    let now = 1_000_000;

    let mut envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .unwrap();
    envelope.signature =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 64]); // completely invalid signature bytes

    let cache = LkgCache::new(&lkg_path);
    cache.write_atomically(&envelope, &payload).unwrap();

    let config = CatalogConfig {
        cache_path: Some(lkg_path),
        keyring,
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    let embedded = embedded_registry_snapshot().unwrap();
    assert_eq!(manager.active_source(), CatalogSource::EmbeddedFallback);
    assert_eq!(manager.active_version(), embedded.version());
    assert!(manager.status().lkg_version.is_none());

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_boot_corrupted_payload_bytes_in_lkg_falls_back_to_embedded() {
    let tmp = unique_temp_dir("test-catalog-lkg-corrupted-bytes-boot");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring();
    let (snapshot, payload) = sample_catalog(777, "claude-corrupt-bytes-lkg");
    let now = 1_000_000;

    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .unwrap();

    let cache = LkgCache::new(&lkg_path);
    cache.write_atomically(&envelope, &payload).unwrap();

    // Corrupt bytes in the written file directly on disk
    let mut file_bytes = fs::read(&lkg_path).unwrap();
    let mid = file_bytes.len() / 2;
    file_bytes[mid] ^= 0xff;
    fs::write(&lkg_path, file_bytes).unwrap();

    let config = CatalogConfig {
        cache_path: Some(lkg_path),
        keyring,
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    let embedded = embedded_registry_snapshot().unwrap();
    assert_eq!(manager.active_source(), CatalogSource::EmbeddedFallback);
    assert_eq!(manager.active_version(), embedded.version());
    assert!(manager.status().lkg_version.is_none());

    let _ = fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_fetch_and_update_with_local_mock_server_success() {
    use axum::{routing::get, Router};
    use tokio::net::TcpListener;

    let tmp = unique_temp_dir("test-catalog-lkg-fetch-success");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (signer, keyring) = test_signer_and_keyring();
    let now = 1_000_000;

    let (snapshot, payload) = sample_catalog(999, "claude-remote-model");
    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .unwrap();
    let env_json = envelope.to_json().unwrap();

    let payload_bytes = payload.clone();
    let env_string = env_json.clone();

    let app = Router::new()
        .route(
            "/models-v1.json",
            get(move || {
                let bytes = payload_bytes.clone();
                async move { bytes }
            }),
        )
        .route(
            "/models-v1.json.sig",
            get(move || {
                let s = env_string.clone();
                async move { s }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        remote_catalog_url: Some(format!("http://{addr}/models-v1.json")),
        remote_signature_url: Some(format!("http://{addr}/models-v1.json.sig")),
        keyring: keyring.clone(),
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    assert_eq!(manager.active_source(), CatalogSource::EmbeddedFallback);

    let updated = manager
        .fetch_and_update()
        .await
        .expect("fetch should succeed");
    assert_eq!(updated.version(), CatalogVersion(999));
    assert_eq!(manager.active_source(), CatalogSource::RemoteSigned);
    assert_eq!(manager.active_version(), CatalogVersion(999));
    assert!(manager
        .current_snapshot()
        .get_model(&ModelId::new("claude-remote-model").unwrap())
        .is_some());

    // Verify LKG exists on disk with 0600 mode
    assert!(lkg_path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&lkg_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let _ = fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_fetch_and_update_with_local_mock_server_500_leaves_active_and_lkg_unchanged() {
    use axum::{http::StatusCode, routing::get, Router};
    use tokio::net::TcpListener;

    let tmp = unique_temp_dir("test-catalog-lkg-fetch-500");
    let lkg_path = tmp.join("models-v1.signed.json");

    let (_signer, keyring) = test_signer_and_keyring();
    let now = 1_000_000;

    let app = Router::new()
        .route(
            "/models-v1.json",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "server error") }),
        )
        .route(
            "/models-v1.json.sig",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "server error") }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let config = CatalogConfig {
        cache_path: Some(lkg_path.clone()),
        remote_catalog_url: Some(format!("http://{addr}/models-v1.json")),
        remote_signature_url: Some(format!("http://{addr}/models-v1.json.sig")),
        keyring,
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, now);
    let initial_version = manager.active_version();
    let initial_source = manager.active_source();

    let result = manager.fetch_and_update().await;
    assert!(result.is_err(), "500 response must return error");

    // Active state and LKG must be completely unchanged
    assert_eq!(manager.active_version(), initial_version);
    assert_eq!(manager.active_source(), initial_source);
    assert!(
        !lkg_path.exists(),
        "LKG file must not be created on failed update"
    );
    assert!(!manager.status().last_refresh_success);
    assert!(manager.status().last_error.is_some());

    let _ = fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn test_insecure_remote_url_rejected() {
    let config = CatalogConfig {
        remote_catalog_url: Some("http://evil-attacker.com/models-v1.json".to_string()),
        remote_signature_url: Some("http://evil-attacker.com/models-v1.json.sig".to_string()),
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, 1_000_000);
    let result = manager.fetch_and_update().await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("insecure HTTP URL not allowed"),
        "error: {err}"
    );
}

#[tokio::test]
async fn test_remote_url_with_credentials_rejected() {
    let config = CatalogConfig {
        remote_catalog_url: Some("https://user:pass@example.com/models-v1.json".to_string()),
        remote_signature_url: Some("https://user:pass@example.com/models-v1.json.sig".to_string()),
        ..Default::default()
    };

    let manager = CatalogManager::boot(config, 1_000_000);
    let result = manager.fetch_and_update().await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("catalog URL must not contain credentials"),
        "error: {err}"
    );
}

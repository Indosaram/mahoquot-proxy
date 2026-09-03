use ed25519_dalek::{SigningKey, VerifyingKey};
use mahoquot_registry::{
    canonicalize_json, is_canonical_json, verify_catalog_envelope, CatalogSigner, CatalogSource,
    CatalogVerificationError, CatalogVersion, Keyring, ModelCapability, ModelDescriptor, ModelId,
    ProviderBinding, ProviderId, ProviderPolicy, RegistryBuilder, RegistrySnapshot,
};

fn test_keypair() -> (SigningKey, VerifyingKey) {
    let seed: [u8; 32] = [
        0x79, 0x61, 0x29, 0x84, 0x53, 0x40, 0x17, 0x68, 0x40, 0x39, 0x20, 0x19, 0x48, 0x57, 0x39,
        0x20, 0x18, 0x47, 0x59, 0x30, 0x29, 0x18, 0x47, 0x58, 0x39, 0x20, 0x19, 0x48, 0x57, 0x69,
        0x28, 0x34,
    ];
    let sk = SigningKey::from_bytes(&seed);
    let vk = sk.verifying_key();
    (sk, vk)
}

fn sample_catalog(version: u64) -> RegistrySnapshot {
    let mut builder =
        RegistryBuilder::new(CatalogVersion::new(version), CatalogSource::RemoteSigned);
    builder.register_provider(ProviderId::codex(), ProviderPolicy::Open);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);

    let desc = ModelDescriptor::new(ModelId::new("claude-3-7-sonnet").unwrap(), "anthropic")
        .with_capabilities([ModelCapability::Chat, ModelCapability::Tools])
        .with_binding(
            ProviderBinding::new(
                ProviderId::claude(),
                ProviderPolicy::Closed,
                CatalogSource::RemoteSigned,
            )
            .with_capabilities([ModelCapability::Chat, ModelCapability::Tools]),
        );
    builder.add_model(desc).unwrap();
    builder.build().unwrap()
}

#[test]
fn test_canonicalize_and_is_canonical() {
    let non_canonical = br#"  {  "b": 2, "a": 1 }  "#;
    let canonical = canonicalize_json(non_canonical).expect("canonicalize should succeed");
    assert_eq!(canonical, br#"{"a":1,"b":2}"#);
    assert!(!is_canonical_json(non_canonical).unwrap());
    assert!(is_canonical_json(&canonical).unwrap());
}

#[test]
fn test_valid_envelope_roundtrip() {
    let (sk, vk) = test_keypair();
    let key_id = "test-key-1";
    let keyring = Keyring::new().with_key(key_id, vk);

    let snapshot = sample_catalog(10);
    let raw_json = serde_json::to_vec(&snapshot).unwrap();
    let canonical = canonicalize_json(&raw_json).unwrap();

    let signer = CatalogSigner::new(sk, key_id);
    let now = 1_700_000_000;
    let envelope = signer
        .sign_catalog(CatalogVersion::new(10), now, Some(now + 3600), &canonical)
        .expect("signing must succeed");

    let verified = verify_catalog_envelope(
        &envelope,
        &canonical,
        &keyring,
        Some(CatalogVersion::new(9)),
        Some(CatalogVersion::new(8)),
        now + 10,
        300,
    )
    .expect("verification must succeed");

    assert_eq!(verified.version(), CatalogVersion::new(10));
    assert_eq!(verified.models().len(), 1);
}

#[test]
fn test_tampered_payload_fails_signature() {
    let (sk, vk) = test_keypair();
    let key_id = "test-key-1";
    let keyring = Keyring::new().with_key(key_id, vk);

    let snapshot = sample_catalog(10);
    let canonical = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let signer = CatalogSigner::new(sk, key_id);
    let now = 1_700_000_000;
    let envelope = signer
        .sign_catalog(CatalogVersion::new(10), now, None, &canonical)
        .unwrap();

    // Mutate 1 byte in payload while keeping it valid canonical JSON
    let snapshot2 = sample_catalog(11);
    let canonical2 = canonicalize_json(&serde_json::to_vec(&snapshot2).unwrap()).unwrap();

    let err = verify_catalog_envelope(&envelope, &canonical2, &keyring, None, None, now, 300)
        .unwrap_err();

    assert_eq!(err, CatalogVerificationError::SignatureVerificationFailed);
}

#[test]
fn test_unknown_key_id_rejected() {
    let (sk, _vk) = test_keypair();
    let keyring = Keyring::new(); // empty keyring

    let snapshot = sample_catalog(1);
    let canonical = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let signer = CatalogSigner::new(sk, "unknown-key");
    let envelope = signer
        .sign_catalog(CatalogVersion::new(1), 1000, None, &canonical)
        .unwrap();

    let err = verify_catalog_envelope(&envelope, &canonical, &keyring, None, None, 1000, 300)
        .unwrap_err();

    assert_eq!(
        err,
        CatalogVerificationError::UnknownKeyId("unknown-key".to_string())
    );
}

#[test]
fn test_incompatible_schema_rejected() {
    let (sk, vk) = test_keypair();
    let keyring = Keyring::new().with_key("k1", vk);
    let snapshot = sample_catalog(1);
    let canonical = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let mut envelope = CatalogSigner::new(sk, "k1")
        .sign_catalog(CatalogVersion::new(1), 1000, None, &canonical)
        .unwrap();
    envelope.schema_version = 2; // Incompatible

    let err = verify_catalog_envelope(&envelope, &canonical, &keyring, None, None, 1000, 300)
        .unwrap_err();

    assert_eq!(err, CatalogVerificationError::IncompatibleSchema(2));
}

#[test]
fn test_version_downgrade_rejected() {
    let (sk, vk) = test_keypair();
    let keyring = Keyring::new().with_key("k1", vk);
    let snapshot = sample_catalog(5);
    let canonical = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let envelope = CatalogSigner::new(sk, "k1")
        .sign_catalog(CatalogVersion::new(5), 1000, None, &canonical)
        .unwrap();

    // Active version is 5 (equal to incoming)
    let err = verify_catalog_envelope(
        &envelope,
        &canonical,
        &keyring,
        Some(CatalogVersion::new(5)),
        None,
        1000,
        300,
    )
    .unwrap_err();

    assert_eq!(
        err,
        CatalogVerificationError::VersionDowngrade {
            incoming: CatalogVersion::new(5),
            active: Some(CatalogVersion::new(5)),
            lkg: None,
            threshold: CatalogVersion::new(5),
        }
    );

    // LKG version is 6 (higher than incoming)
    let err = verify_catalog_envelope(
        &envelope,
        &canonical,
        &keyring,
        Some(CatalogVersion::new(4)),
        Some(CatalogVersion::new(6)),
        1000,
        300,
    )
    .unwrap_err();

    assert_eq!(
        err,
        CatalogVerificationError::VersionDowngrade {
            incoming: CatalogVersion::new(5),
            active: Some(CatalogVersion::new(4)),
            lkg: Some(CatalogVersion::new(6)),
            threshold: CatalogVersion::new(6),
        }
    );
}

#[test]
fn test_future_timestamp_rejected() {
    let (sk, vk) = test_keypair();
    let keyring = Keyring::new().with_key("k1", vk);
    let snapshot = sample_catalog(1);
    let canonical = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let now = 1000;
    let allowed_skew = 300;
    let envelope = CatalogSigner::new(sk, "k1")
        .sign_catalog(CatalogVersion::new(1), now + 301, None, &canonical)
        .unwrap();

    let err = verify_catalog_envelope(
        &envelope,
        &canonical,
        &keyring,
        None,
        None,
        now,
        allowed_skew,
    )
    .unwrap_err();

    assert_eq!(
        err,
        CatalogVerificationError::FutureTimestamp {
            generated_at: 1301,
            now: 1000,
            allowed_skew_secs: 300,
        }
    );
}

#[test]
fn test_expired_catalog_rejected() {
    let (sk, vk) = test_keypair();
    let keyring = Keyring::new().with_key("k1", vk);
    let snapshot = sample_catalog(1);
    let canonical = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let now = 1000;
    let envelope = CatalogSigner::new(sk, "k1")
        .sign_catalog(CatalogVersion::new(1), 500, Some(800), &canonical)
        .unwrap();

    let err =
        verify_catalog_envelope(&envelope, &canonical, &keyring, None, None, now, 300).unwrap_err();

    assert_eq!(
        err,
        CatalogVerificationError::Expired {
            expires_at: 800,
            now: 1000,
        }
    );
}

#[test]
fn test_payload_version_mismatch_rejected() {
    let (sk, vk) = test_keypair();
    let keyring = Keyring::new().with_key("k1", vk);
    let snapshot = sample_catalog(1);
    let canonical = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let mut envelope = CatalogSigner::new(sk, "k1")
        .sign_catalog(CatalogVersion::new(1), 1000, None, &canonical)
        .unwrap();
    // Tamper envelope version
    envelope.catalog_version = CatalogVersion::new(2);

    let err = verify_catalog_envelope(&envelope, &canonical, &keyring, None, None, 1000, 300)
        .unwrap_err();

    // Fails either signature or version mismatch
    assert!(matches!(
        err,
        CatalogVerificationError::SignatureVerificationFailed
            | CatalogVerificationError::VersionMismatch { .. }
    ));
}

#[test]
fn test_empty_catalog_rejected() {
    let (sk, vk) = test_keypair();
    let keyring = Keyring::new().with_key("k1", vk);

    let mut builder = RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::RemoteSigned);
    builder.register_provider(ProviderId::codex(), ProviderPolicy::Open);
    let snapshot = builder.build().unwrap(); // Empty models!
    let canonical = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let envelope = CatalogSigner::new(sk, "k1")
        .sign_catalog(CatalogVersion::new(1), 1000, None, &canonical)
        .unwrap();

    let err = verify_catalog_envelope(&envelope, &canonical, &keyring, None, None, 1000, 300)
        .unwrap_err();

    assert_eq!(err, CatalogVerificationError::EmptyCatalog);
}

#[test]
fn test_zero_fallback_routable_bindings_rejected() {
    let (sk, vk) = test_keypair();
    let keyring = Keyring::new().with_key("k1", vk);

    let mut builder = RegistryBuilder::new(CatalogVersion::new(1), CatalogSource::RemoteSigned);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    // Add model with NO bindings
    let desc = ModelDescriptor::new(ModelId::new("claude-test").unwrap(), "anthropic");
    builder.add_model(desc).unwrap();
    let snapshot = builder.build().unwrap();
    let canonical = canonicalize_json(&serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let envelope = CatalogSigner::new(sk, "k1")
        .sign_catalog(CatalogVersion::new(1), 1000, None, &canonical)
        .unwrap();

    let err = verify_catalog_envelope(&envelope, &canonical, &keyring, None, None, 1000, 300)
        .unwrap_err();

    assert_eq!(err, CatalogVerificationError::ZeroFallbackRoutableBindings);
}

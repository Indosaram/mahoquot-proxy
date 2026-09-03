use ed25519_dalek::SigningKey;
use mahoquot_registry::envelope::*;
use mahoquot_registry::*;

fn test_keys() -> (CatalogSigner, Keyring) {
    let mut rng = rand::rngs::OsRng;
    let signing_key = SigningKey::generate(&mut rng);
    let key_id = "test-key-2026-v1";
    let keyring = Keyring::new().with_key(key_id, signing_key.verifying_key());
    let signer = CatalogSigner::new(signing_key, key_id);
    (signer, keyring)
}

fn sample_snapshot(version: u64) -> RegistrySnapshot {
    let mut builder = RegistryBuilder::new(CatalogVersion(version), CatalogSource::RemoteSigned);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    let mut model = ModelDescriptor::new(ModelId::new("claude-sonnet-4-6").unwrap(), "anthropic");
    model.capabilities.insert(ModelCapability::Chat);
    let binding = ProviderBinding::new(
        ProviderId::claude(),
        ProviderPolicy::Closed,
        CatalogSource::RemoteSigned,
    )
    .with_capabilities([ModelCapability::Chat]);
    builder.add_model(model).unwrap();
    builder
        .add_binding(ModelId::new("claude-sonnet-4-6").unwrap(), binding)
        .unwrap();
    builder.build().unwrap()
}

fn canonical_payload(snapshot: &RegistrySnapshot) -> Vec<u8> {
    canonicalize_json(&serde_json::to_vec(snapshot).unwrap()).unwrap()
}

#[test]
fn signed_catalog_valid_signature_verification() {
    let (signer, keyring) = test_keys();
    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);
    let now = 1000;

    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing should succeed");

    let verified = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        Some(CatalogVersion(5)),
        Some(CatalogVersion(8)),
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    )
    .expect("verification should succeed");

    assert_eq!(verified.version(), CatalogVersion(10));
    assert_eq!(verified.models().len(), 1);
}

#[test]
fn signed_catalog_corrupted_byte_rejection() {
    let (signer, keyring) = test_keys();
    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);
    let now = 1000;

    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing should succeed");

    let mut corrupted = payload.clone();
    corrupted[20] ^= 0xff; // corrupt a byte

    let result = verify_catalog_envelope(
        &envelope,
        &corrupted,
        &keyring,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );

    assert_eq!(
        result,
        Err(CatalogVerificationError::SignatureVerificationFailed)
    );
}

#[test]
fn signed_catalog_corrupted_signature_rejection() {
    let (signer, keyring) = test_keys();
    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);
    let now = 1000;

    let mut envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing should succeed");

    // Corrupt base64 signature by replacing chars
    envelope.signature = format!("AAAA{}", &envelope.signature[4..]);

    let result = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );

    assert!(matches!(
        result,
        Err(CatalogVerificationError::SignatureVerificationFailed)
            | Err(CatalogVerificationError::InvalidSignatureFormat(_))
    ));
}

#[test]
fn signed_catalog_wrong_key_rejection() {
    let (signer_a, _) = test_keys();
    let (_, keyring_wrong) = test_keys(); // different key pair, same key_id "test-key-2026-v1"

    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);
    let now = 1000;

    let envelope = signer_a
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing should succeed");

    let result = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring_wrong,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );

    assert_eq!(
        result,
        Err(CatalogVerificationError::SignatureVerificationFailed)
    );
}

#[test]
fn signed_catalog_unknown_key_id_rejection() {
    let (signer, keyring) = test_keys();
    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);
    let now = 1000;

    let mut envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing should succeed");

    envelope.key_id = "unknown-key-999".to_string();

    let result = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );

    assert_eq!(
        result,
        Err(CatalogVerificationError::UnknownKeyId(
            "unknown-key-999".to_string()
        ))
    );
}

#[test]
fn signed_catalog_anti_downgrade_equal_and_lower_version_rejection() {
    let (signer, keyring) = test_keys();
    let now = 1000;

    // Incoming version 10
    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);

    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing should succeed");

    // Equal version rejection (replay)
    let err_equal = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        Some(CatalogVersion(10)),
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert_eq!(
        err_equal,
        Err(CatalogVerificationError::VersionDowngrade {
            incoming: CatalogVersion(10),
            active: Some(CatalogVersion(10)),
            lkg: None,
            threshold: CatalogVersion(10),
        })
    );

    // Lower version rejection (downgrade below active)
    let err_lower = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        Some(CatalogVersion(11)),
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert_eq!(
        err_lower,
        Err(CatalogVerificationError::VersionDowngrade {
            incoming: CatalogVersion(10),
            active: Some(CatalogVersion(11)),
            lkg: None,
            threshold: CatalogVersion(11),
        })
    );

    // Downgrade below LKG even if active is lower
    let err_lkg = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        Some(CatalogVersion(8)),
        Some(CatalogVersion(12)),
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert_eq!(
        err_lkg,
        Err(CatalogVerificationError::VersionDowngrade {
            incoming: CatalogVersion(10),
            active: Some(CatalogVersion(8)),
            lkg: Some(CatalogVersion(12)),
            threshold: CatalogVersion(12),
        })
    );

    // Higher than max(active, lkg) succeeds
    let ok = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        Some(CatalogVersion(8)),
        Some(CatalogVersion(9)),
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert!(ok.is_ok());
}

#[test]
fn signed_catalog_future_timestamp_rejection() {
    let (signer, keyring) = test_keys();
    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);
    let now = 1000;
    let allowed_skew = 300;

    // Generated far in future: 1000 + 300 + 1 = 1301
    let envelope_future = signer
        .sign_catalog(snapshot.version(), 1301, None, &payload)
        .expect("signing should succeed");

    let err_future = verify_catalog_envelope(
        &envelope_future,
        &payload,
        &keyring,
        None,
        None,
        now,
        allowed_skew,
    );
    assert_eq!(
        err_future,
        Err(CatalogVerificationError::FutureTimestamp {
            generated_at: 1301,
            now,
            allowed_skew_secs: allowed_skew,
        })
    );

    // Within allowed skew: 1000 + 300 = 1300
    let envelope_ok = signer
        .sign_catalog(snapshot.version(), 1300, None, &payload)
        .expect("signing should succeed");

    let ok = verify_catalog_envelope(
        &envelope_ok,
        &payload,
        &keyring,
        None,
        None,
        now,
        allowed_skew,
    );
    assert!(ok.is_ok());
}

#[test]
fn signed_catalog_expired_catalog_rejection() {
    let (signer, keyring) = test_keys();
    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);
    let now = 2000;

    // Expired catalog: expires_at 1999 < now 2000
    let envelope_expired = signer
        .sign_catalog(snapshot.version(), 1000, Some(1999), &payload)
        .expect("signing should succeed");

    let err_expired = verify_catalog_envelope(
        &envelope_expired,
        &payload,
        &keyring,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert_eq!(
        err_expired,
        Err(CatalogVerificationError::Expired {
            expires_at: 1999,
            now,
        })
    );

    // Not yet expired: expires_at 2001 >= now 2000
    let envelope_valid = signer
        .sign_catalog(snapshot.version(), 1000, Some(2001), &payload)
        .expect("signing should succeed");

    let ok = verify_catalog_envelope(
        &envelope_valid,
        &payload,
        &keyring,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert!(ok.is_ok());
}

#[test]
fn signed_catalog_empty_catalog_rejection() {
    let (signer, keyring) = test_keys();
    let now = 1000;

    // Snapshot with open provider but zero models
    let mut builder = RegistryBuilder::new(CatalogVersion(10), CatalogSource::RemoteSigned);
    builder.register_provider(ProviderId::codex(), ProviderPolicy::Open);
    let snapshot = builder.build().unwrap();
    assert!(snapshot.models().is_empty());

    let payload = canonical_payload(&snapshot);
    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing should succeed");

    let err = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert_eq!(err, Err(CatalogVerificationError::EmptyCatalog));
}

#[test]
fn signed_catalog_non_canonical_json_rejection() {
    let (signer, _keyring) = test_keys();
    let now = 1000;

    let snapshot = sample_snapshot(10);
    let canonical = canonical_payload(&snapshot);

    // Whitespace-padded non-canonical JSON
    let mut non_canonical = canonical.clone();
    non_canonical.insert(0, b' ');
    non_canonical.push(b' ');
    assert_ne!(non_canonical, canonical);

    // sign_catalog must reject non-canonical payload
    let sign_err = signer.sign_catalog(snapshot.version(), now, None, &non_canonical);
    assert_eq!(
        sign_err,
        Err(CatalogVerificationError::CanonicalizationMismatch)
    );

    // If non-canonical payload is signed directly over its raw bytes,
    // verify_catalog_envelope must specifically reject it with CanonicalizationMismatch
    let mut rng = rand::rngs::OsRng;
    let test_sk = SigningKey::generate(&mut rng);
    let test_kr = Keyring::new().with_key("non-canonical-key", test_sk.verifying_key());
    let raw_signing_bytes = compute_signing_bytes(
        SCHEMA_VERSION_V1,
        snapshot.version(),
        now,
        None,
        "non-canonical-key",
        &non_canonical,
    );
    use base64::Engine;
    use ed25519_dalek::Signer;
    let raw_sig = test_sk.sign(&raw_signing_bytes);
    let raw_sig_b64 = base64::engine::general_purpose::STANDARD.encode(raw_sig.to_bytes());
    let raw_envelope = CatalogEnvelope::new(
        snapshot.version(),
        "non-canonical-key",
        now,
        None,
        raw_sig_b64,
    );

    let err = verify_catalog_envelope(
        &raw_envelope,
        &non_canonical,
        &test_kr,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert_eq!(err, Err(CatalogVerificationError::CanonicalizationMismatch));
}

#[test]
fn signed_catalog_zero_fallback_routable_bindings_rejection() {
    let (signer, keyring) = test_keys();
    let now = 1000;

    // Snapshot has a model, but provider is closed and no bindings
    let mut builder = RegistryBuilder::new(CatalogVersion(10), CatalogSource::RemoteSigned);
    builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    let model = ModelDescriptor::new(ModelId::new("orphan-model").unwrap(), "anthropic");
    builder.add_model(model).unwrap();
    let snapshot = builder.build().unwrap();

    let payload = canonical_payload(&snapshot);
    let envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing should succeed");

    let err = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert_eq!(
        err,
        Err(CatalogVerificationError::ZeroFallbackRoutableBindings)
    );
}

#[test]
fn signed_catalog_key_rotation_overlap() {
    let mut rng = rand::rngs::OsRng;
    let key1 = SigningKey::generate(&mut rng);
    let key2 = SigningKey::generate(&mut rng);

    let signer1 = CatalogSigner::new(key1, "key-v1");
    let signer2 = CatalogSigner::new(key2, "key-v2");

    // Keyring with both keys during rotation overlap
    let keyring = Keyring::new()
        .with_key("key-v1", signer1.verifying_key())
        .with_key("key-v2", signer2.verifying_key());

    let now = 1000;
    let snapshot1 = sample_snapshot(10);
    let payload1 = canonical_payload(&snapshot1);
    let env1 = signer1
        .sign_catalog(snapshot1.version(), now, None, &payload1)
        .unwrap();

    // Verification with key-v1 succeeds
    assert!(verify_catalog_envelope(&env1, &payload1, &keyring, None, None, now, 300).is_ok());

    let snapshot2 = sample_snapshot(11);
    let payload2 = canonical_payload(&snapshot2);
    let env2 = signer2
        .sign_catalog(snapshot2.version(), now, None, &payload2)
        .unwrap();

    // Verification with key-v2 succeeds
    assert!(verify_catalog_envelope(
        &env2,
        &payload2,
        &keyring,
        Some(CatalogVersion(10)),
        None,
        now,
        300
    )
    .is_ok());

    // Untrusted key-v3 fails with UnknownKeyId
    let key3 = SigningKey::generate(&mut rng);
    let signer3 = CatalogSigner::new(key3, "key-v3");
    let env3 = signer3
        .sign_catalog(snapshot2.version(), now, None, &payload2)
        .unwrap();
    assert_eq!(
        verify_catalog_envelope(&env3, &payload2, &keyring, None, None, now, 300),
        Err(CatalogVerificationError::UnknownKeyId("key-v3".to_string()))
    );
}

#[test]
fn signed_catalog_incompatible_schema_rejection() {
    let (signer, keyring) = test_keys();
    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);
    let now = 1000;

    let mut envelope = signer
        .sign_catalog(snapshot.version(), now, None, &payload)
        .expect("signing should succeed");

    envelope.schema_version = 99;

    let err = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert_eq!(err, Err(CatalogVerificationError::IncompatibleSchema(99)));
}

#[test]
fn signed_catalog_version_mismatch_between_envelope_and_payload() {
    let (signer, keyring) = test_keys();
    let snapshot = sample_snapshot(10);
    let payload = canonical_payload(&snapshot);
    let now = 1000;

    // Envelope version 20 signed over payload that has internal version 10
    let envelope = signer
        .sign_catalog(CatalogVersion(20), now, None, &payload)
        .expect("signing should succeed");

    let err = verify_catalog_envelope(
        &envelope,
        &payload,
        &keyring,
        None,
        None,
        now,
        DEFAULT_CLOCK_SKEW_SECS,
    );
    assert_eq!(
        err,
        Err(CatalogVerificationError::VersionMismatch {
            envelope_version: CatalogVersion(20),
            payload_version: CatalogVersion(10),
        })
    );
}

#[test]
fn signed_catalog_canonicalize_json_and_is_canonical_json() {
    let snapshot = sample_snapshot(5);
    let bytes = canonical_payload(&snapshot);

    assert!(is_canonical_json(&bytes).unwrap());

    let non_canonical = format!("  {}  ", std::str::from_utf8(&bytes).unwrap());
    assert!(!is_canonical_json(non_canonical.as_bytes()).unwrap());

    let canonicalized = canonicalize_json(non_canonical.as_bytes()).unwrap();
    assert_eq!(canonicalized, bytes);

    let malformed = b"{ invalid json }";
    assert!(matches!(
        canonicalize_json(malformed),
        Err(CatalogVerificationError::MalformedJson(_))
    ));
    assert!(matches!(
        is_canonical_json(malformed),
        Err(CatalogVerificationError::MalformedJson(_))
    ));
}

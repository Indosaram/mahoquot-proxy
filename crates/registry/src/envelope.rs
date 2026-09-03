//! Cryptographic catalog envelope and verification rules.
//!
//! Pure crate invariant: NO tokio, reqwest, axum, account secrets, arc-swap, or gateway dependency.

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{CatalogVersion, RegistryError, RegistrySnapshot};

pub const SCHEMA_VERSION_V1: u32 = 1;
pub const DEFAULT_CLOCK_SKEW_SECS: u64 = 300; // 5 minutes

/// Default embedded production public key for Indosaram/mahoquot-proxy model catalog.
pub const EMBEDDED_PROD_KEY_ID_V1: &str = "mahoquot-prod-2026-v1";
pub const EMBEDDED_PROD_KEY_BYTES_V1: [u8; 32] = [
    41, 74, 254, 13, 1, 102, 184, 174, 64, 6, 45, 226, 177, 174, 215, 63, 254, 243, 39, 173, 238,
    180, 141, 174, 44, 84, 23, 241, 90, 168, 28, 74,
];

/// Test Ed25519 public key for local development and CI testing.
pub const TEST_KEY_ID_V1: &str = "test-ed25519-v1";
pub const TEST_KEY_BYTES_V1: [u8; 32] = [
    91, 148, 162, 51, 103, 29, 252, 144, 197, 138, 122, 175, 189, 227, 148, 187, 37, 179, 95, 102,
    183, 130, 214, 242, 29, 183, 6, 97, 59, 116, 173, 172,
];

#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum CatalogVerificationError {
    #[error("unknown key id: '{0}'")]
    UnknownKeyId(String),

    #[error("incompatible schema version: expected 1, got {0}")]
    IncompatibleSchema(u32),

    #[error("anti-downgrade check failed: incoming version {incoming} <= threshold {threshold} (active: {active:?}, lkg: {lkg:?})")]
    VersionDowngrade {
        incoming: CatalogVersion,
        active: Option<CatalogVersion>,
        lkg: Option<CatalogVersion>,
        threshold: CatalogVersion,
    },

    #[error("future timestamp: generated_at {generated_at} is in the future (current time {now}, allowed skew {allowed_skew_secs}s)")]
    FutureTimestamp {
        generated_at: u64,
        now: u64,
        allowed_skew_secs: u64,
    },

    #[error("catalog expired: expires_at {expires_at} < current time {now}")]
    Expired { expires_at: u64, now: u64 },

    #[error("canonicalization mismatch: payload bytes do not match canonical representation")]
    CanonicalizationMismatch,

    #[error("invalid signature format: {0}")]
    InvalidSignatureFormat(String),

    #[error("signature verification failed")]
    SignatureVerificationFailed,

    #[error("malformed json: {0}")]
    MalformedJson(String),

    #[error("empty catalog: catalog contains zero models")]
    EmptyCatalog,

    #[error("zero fallback-routable bindings: no models have routable bindings")]
    ZeroFallbackRoutableBindings,

    #[error("payload version mismatch: envelope version {envelope_version} != payload version {payload_version}")]
    VersionMismatch {
        envelope_version: CatalogVersion,
        payload_version: CatalogVersion,
    },

    #[error("registry validation error: {0}")]
    Registry(#[from] RegistryError),

    #[error("serialization error: {0}")]
    SerializationError(String),
}

/// Detached signature envelope for a remote model catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEnvelope {
    pub schema_version: u32,
    pub catalog_version: CatalogVersion,
    pub key_id: String,
    pub generated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub signature: String,
}

impl CatalogEnvelope {
    pub fn new(
        catalog_version: CatalogVersion,
        key_id: impl Into<String>,
        generated_at: u64,
        expires_at: Option<u64>,
        signature: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION_V1,
            catalog_version,
            key_id: key_id.into(),
            generated_at,
            expires_at,
            signature: signature.into(),
        }
    }

    pub fn from_json(json: &str) -> Result<Self, CatalogVerificationError> {
        serde_json::from_str(json)
            .map_err(|e| CatalogVerificationError::MalformedJson(e.to_string()))
    }

    pub fn to_json(&self) -> Result<String, CatalogVerificationError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| CatalogVerificationError::SerializationError(e.to_string()))
    }
}

/// Keyring of active Ed25519 public keys trusted to sign catalogs.
#[derive(Debug, Clone)]
pub struct Keyring {
    keys: BTreeMap<String, VerifyingKey>,
}

impl Keyring {
    pub fn new() -> Self {
        Self {
            keys: BTreeMap::new(),
        }
    }

    pub fn embedded_default() -> Self {
        let mut kr = Self::new();
        if let Ok(key) = VerifyingKey::from_bytes(&EMBEDDED_PROD_KEY_BYTES_V1) {
            kr.add_key(EMBEDDED_PROD_KEY_ID_V1, key);
        }
        if let Ok(key) = VerifyingKey::from_bytes(&TEST_KEY_BYTES_V1) {
            kr.add_key(TEST_KEY_ID_V1, key);
        }
        kr
    }

    pub fn with_key(mut self, key_id: impl Into<String>, key: VerifyingKey) -> Self {
        self.keys.insert(key_id.into(), key);
        self
    }

    pub fn add_key(&mut self, key_id: impl Into<String>, key: VerifyingKey) {
        self.keys.insert(key_id.into(), key);
    }

    pub fn get(&self, key_id: &str) -> Option<&VerifyingKey> {
        self.keys.get(key_id)
    }

    pub fn contains(&self, key_id: &str) -> bool {
        self.keys.contains_key(key_id)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

impl Default for Keyring {
    fn default() -> Self {
        Self::embedded_default()
    }
}

/// Signer helper for generating detached signatures.
pub struct CatalogSigner {
    signing_key: SigningKey,
    key_id: String,
}

impl CatalogSigner {
    pub fn new(signing_key: SigningKey, key_id: impl Into<String>) -> Self {
        Self {
            signing_key,
            key_id: key_id.into(),
        }
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    pub fn sign_catalog(
        &self,
        catalog_version: CatalogVersion,
        generated_at: u64,
        expires_at: Option<u64>,
        canonical_payload: &[u8],
    ) -> Result<CatalogEnvelope, CatalogVerificationError> {
        if !is_canonical_json(canonical_payload)? {
            return Err(CatalogVerificationError::CanonicalizationMismatch);
        }

        let signing_bytes = compute_signing_bytes(
            SCHEMA_VERSION_V1,
            catalog_version,
            generated_at,
            expires_at,
            &self.key_id,
            canonical_payload,
        );

        let signature = self.signing_key.sign(&signing_bytes);
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());

        Ok(CatalogEnvelope::new(
            catalog_version,
            &self.key_id,
            generated_at,
            expires_at,
            sig_b64,
        ))
    }
}

pub fn compute_signing_bytes(
    schema_version: u32,
    catalog_version: CatalogVersion,
    generated_at: u64,
    expires_at: Option<u64>,
    key_id: &str,
    canonical_payload: &[u8],
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(canonical_payload.len() + 128);
    msg.extend_from_slice(b"MAHOQUOT-MODEL-CATALOG-V1\n");
    msg.extend_from_slice(format!("schema_version:{}\n", schema_version).as_bytes());
    msg.extend_from_slice(format!("catalog_version:{}\n", catalog_version.as_u64()).as_bytes());
    msg.extend_from_slice(format!("generated_at:{}\n", generated_at).as_bytes());
    if let Some(exp) = expires_at {
        msg.extend_from_slice(format!("expires_at:{}\n", exp).as_bytes());
    } else {
        msg.extend_from_slice(b"expires_at:none\n");
    }
    msg.extend_from_slice(format!("key_id:{}\n", key_id).as_bytes());
    msg.extend_from_slice(b"canonical_payload:\n");
    msg.extend_from_slice(canonical_payload);
    msg
}

pub fn canonicalize_json(bytes: &[u8]) -> Result<Vec<u8>, CatalogVerificationError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| CatalogVerificationError::MalformedJson(e.to_string()))?;
    serde_json::to_vec(&value)
        .map_err(|e| CatalogVerificationError::SerializationError(e.to_string()))
}

pub fn is_canonical_json(bytes: &[u8]) -> Result<bool, CatalogVerificationError> {
    let canonical = canonicalize_json(bytes)?;
    Ok(bytes == canonical.as_slice())
}

pub fn verify_catalog_envelope(
    envelope: &CatalogEnvelope,
    raw_payload: &[u8],
    keyring: &Keyring,
    active_version: Option<CatalogVersion>,
    lkg_version: Option<CatalogVersion>,
    now: u64,
    allowed_skew_secs: u64,
) -> Result<RegistrySnapshot, CatalogVerificationError> {
    if envelope.schema_version != SCHEMA_VERSION_V1 {
        return Err(CatalogVerificationError::IncompatibleSchema(
            envelope.schema_version,
        ));
    }

    let verifying_key = keyring
        .get(&envelope.key_id)
        .ok_or_else(|| CatalogVerificationError::UnknownKeyId(envelope.key_id.clone()))?;

    let threshold = match (active_version, lkg_version) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    if let Some(t) = threshold {
        if envelope.catalog_version <= t {
            return Err(CatalogVerificationError::VersionDowngrade {
                incoming: envelope.catalog_version,
                active: active_version,
                lkg: lkg_version,
                threshold: t,
            });
        }
    }

    if envelope.generated_at > now.saturating_add(allowed_skew_secs) {
        return Err(CatalogVerificationError::FutureTimestamp {
            generated_at: envelope.generated_at,
            now,
            allowed_skew_secs,
        });
    }

    if let Some(exp) = envelope.expires_at {
        if exp < now {
            return Err(CatalogVerificationError::Expired {
                expires_at: exp,
                now,
            });
        }
    }

    // Cryptographic signature verification must precede parsing or payload interpretation
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(envelope.signature.trim())
        .map_err(|e| CatalogVerificationError::InvalidSignatureFormat(e.to_string()))?;
    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|e| CatalogVerificationError::InvalidSignatureFormat(e.to_string()))?;

    let signing_bytes = compute_signing_bytes(
        envelope.schema_version,
        envelope.catalog_version,
        envelope.generated_at,
        envelope.expires_at,
        &envelope.key_id,
        raw_payload,
    );

    verifying_key
        .verify(&signing_bytes, &signature)
        .map_err(|_| CatalogVerificationError::SignatureVerificationFailed)?;

    if !is_canonical_json(raw_payload)? {
        return Err(CatalogVerificationError::CanonicalizationMismatch);
    }

    let snapshot: RegistrySnapshot = serde_json::from_slice(raw_payload)
        .map_err(|e| CatalogVerificationError::MalformedJson(e.to_string()))?;

    if snapshot.version != envelope.catalog_version {
        return Err(CatalogVerificationError::VersionMismatch {
            envelope_version: envelope.catalog_version,
            payload_version: snapshot.version,
        });
    }

    if snapshot.models.is_empty() {
        return Err(CatalogVerificationError::EmptyCatalog);
    }

    let has_routable_binding = snapshot.models.iter().any(|(mid, model)| {
        model.bindings.iter().any(|(pid, _binding)| {
            !snapshot.exclusions.contains(&crate::ModelExclusionRule {
                model_id: mid.clone(),
                provider_id: Some(pid.clone()),
            }) && !snapshot.exclusions.contains(&crate::ModelExclusionRule {
                model_id: mid.clone(),
                provider_id: None,
            })
        })
    });
    if !has_routable_binding {
        return Err(CatalogVerificationError::ZeroFallbackRoutableBindings);
    }

    snapshot.validate()?;

    Ok(snapshot)
}

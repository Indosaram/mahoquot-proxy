use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mahoquot_registry::{
    canonicalize_json, is_canonical_json, verify_catalog_envelope, CatalogEnvelope, CatalogSource,
    CatalogVersion, Keyring, RegistrySnapshot,
};
use serde::{Deserialize, Serialize};

use super::error::CatalogError;

static TEMP_FILE_NONCE: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

fn random_suffix() -> u64 {
    let counter = TEMP_FILE_NONCE.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    let mut value = counter ^ nanos ^ u64::from(std::process::id()).rotate_left(32);
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    value
}

/// A serialized signed catalog package suitable for single-file disk caching (`models-v1.signed.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedCatalogPackage {
    pub envelope: CatalogEnvelope,
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_payload: Option<String>,
}

impl SignedCatalogPackage {
    pub fn new(
        envelope: CatalogEnvelope,
        raw_canonical_payload: &[u8],
    ) -> Result<Self, CatalogError> {
        let payload: serde_json::Value = serde_json::from_slice(raw_canonical_payload)?;
        let raw_payload = String::from_utf8(raw_canonical_payload.to_vec()).map_err(|e| {
            CatalogError::InvalidState(format!("canonical payload is not valid UTF-8: {e}"))
        })?;
        Ok(Self {
            envelope,
            payload,
            raw_payload: Some(raw_payload),
        })
    }

    pub fn payload_bytes(&self) -> Result<Vec<u8>, CatalogError> {
        if let Some(raw) = &self.raw_payload {
            if is_canonical_json(raw.as_bytes()).unwrap_or(false) {
                return Ok(raw.as_bytes().to_vec());
            }
        }
        let canonical = canonicalize_json(&serde_json::to_vec(&self.payload)?)?;
        Ok(canonical)
    }

    pub fn verify(
        &self,
        keyring: &Keyring,
        active_version: Option<CatalogVersion>,
        lkg_version: Option<CatalogVersion>,
        now: u64,
        allowed_skew: u64,
    ) -> Result<RegistrySnapshot, CatalogError> {
        let bytes = self.payload_bytes()?;
        let snapshot = verify_catalog_envelope(
            &self.envelope,
            &bytes,
            keyring,
            active_version,
            lkg_version,
            now,
            allowed_skew,
        )?;
        Ok(snapshot)
    }
}

/// Last-Known-Good (LKG) disk cache manager.
#[derive(Debug, Clone)]
pub struct LkgCache {
    path: PathBuf,
}

impl LkgCache {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        let path = if p.is_dir() {
            p.join("models-v1.signed.json")
        } else {
            p
        };
        Self { path }
    }

    pub fn default_path() -> PathBuf {
        if let Ok(dir) = std::env::var("MAHOQUOT_CACHE_DIR") {
            return PathBuf::from(dir).join("models-v1.signed.json");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".mahoquot")
                .join("cache")
                .join("models-v1.signed.json");
        }
        std::env::temp_dir()
            .join(".mahoquot")
            .join("cache")
            .join("models-v1.signed.json")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn load(
        &self,
        keyring: &Keyring,
        now: u64,
        allowed_skew: u64,
    ) -> Result<RegistrySnapshot, CatalogError> {
        self.load_with_generated_at(keyring, now, allowed_skew)
            .map(|(snapshot, _)| snapshot)
    }

    pub fn load_with_generated_at(
        &self,
        keyring: &Keyring,
        now: u64,
        allowed_skew: u64,
    ) -> Result<(RegistrySnapshot, u64), CatalogError> {
        if !self.path.exists() {
            return Err(CatalogError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("LKG cache file does not exist at {}", self.path.display()),
            )));
        }

        let raw = fs::read(&self.path)?;
        let package: SignedCatalogPackage = serde_json::from_slice(&raw)?;
        let generated_at = package.envelope.generated_at;
        let mut snapshot = package.verify(keyring, None, None, now, allowed_skew)?;
        snapshot.source = CatalogSource::LkgCache;
        Ok((snapshot, generated_at))
    }

    pub fn write_atomically(
        &self,
        envelope: &CatalogEnvelope,
        canonical_payload: &[u8],
    ) -> Result<(), CatalogError> {
        let package = SignedCatalogPackage::new(envelope.clone(), canonical_payload)?;
        let serialized = serde_json::to_vec_pretty(&package)?;

        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;

        let file_name = self
            .path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("models-v1.signed.json"))
            .to_string_lossy();

        for _ in 0..128 {
            let temp_path = parent.join(format!(
                ".{file_name}.tmp.{}.{:016x}",
                std::process::id(),
                random_suffix()
            ));

            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }

            let mut file = match options.open(&temp_path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(CatalogError::Io(error)),
            };

            let result = (|| -> Result<(), CatalogError> {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(fs::Permissions::from_mode(0o600))?;
                }
                file.write_all(&serialized)?;
                file.sync_all()?;
                drop(file);
                fs::rename(&temp_path, &self.path)?;
                Ok(())
            })();

            if result.is_err() {
                let _ = fs::remove_file(&temp_path);
            }
            return result;
        }

        Err(CatalogError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "failed to create temporary file for atomic write after 128 attempts",
        )))
    }
}

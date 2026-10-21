use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::plugins::abi::PluginError;
use crate::plugins::loader::PluginHost;

pub const DEFAULT_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/router-for-me/CLIProxyAPI-Plugins-Store/main/registry.json";
pub const DEFAULT_SOURCE_ID: &str = "official";
pub const DEFAULT_SOURCE_NAME: &str = "Official";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreSource {
    pub id: String,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreArtifact {
    #[serde(default)]
    pub goos: String,
    #[serde(default)]
    pub goarch: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreInstallPlan {
    #[serde(rename = "type", default)]
    pub install_type: String,
    #[serde(default)]
    pub artifacts: Vec<StoreArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorePlugin {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub logo: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub install: Option<StoreInstallPlan>,
    #[serde(default)]
    pub auth_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreRegistry {
    #[serde(default = "default_schema_version")]
    pub schema_version: i32,
    #[serde(default)]
    pub plugins: Vec<StorePlugin>,
}

fn default_schema_version() -> i32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: i32,
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub release_tag: String,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub logo: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_id: String,
    #[serde(default)]
    pub source_name: String,
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub install: StoreInstallPlan,
}

impl Default for StoreInstallPlan {
    fn default() -> Self {
        Self {
            install_type: "direct".to_string(),
            artifacts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSyncItem {
    pub manifest: StoreManifest,
    #[serde(default)]
    pub auth: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInstallStatus {
    pub id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub release_tag: String,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub install_type: String,
    pub install_status: String,
    #[serde(default)]
    pub load_status: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub overwritten: bool,
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledArtifact {
    pub id: String,
    pub version: String,
    pub source_id: String,
    pub source_name: String,
    pub source_url: String,
    pub path: PathBuf,
    pub overwritten: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub schema_version: i32,
    #[serde(default)]
    pub task_id: u64,
    pub task: String,
    #[serde(default)]
    pub node_id: String,
    pub status: String,
    pub phase: String,
    pub ok: bool,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: String,
    pub updated_at: String,
    pub platform: HashMap<String, String>,
    pub plugins: Vec<PluginInstallStatus>,
    #[serde(default)]
    pub error: String,
}

pub struct PluginStore {
    pub plugins_dir: PathBuf,
    pub host: Option<Arc<PluginHost>>,
}

impl PluginStore {
    pub fn new(plugins_dir: PathBuf, host: Option<Arc<PluginHost>>) -> Self {
        Self { plugins_dir, host }
    }

    pub fn current_platform() -> (String, String) {
        let goos = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "windows",
            _ => "linux",
        };
        let goarch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            _ => "amd64",
        };
        (goos.to_string(), goarch.to_string())
    }

    pub fn plugin_extension(goos: &str) -> &'static str {
        match goos {
            "darwin" => ".dylib",
            "windows" => ".dll",
            _ => ".so",
        }
    }

    pub fn parse_registry(&self, json_bytes: &[u8]) -> Result<StoreRegistry, PluginError> {
        serde_json::from_slice(json_bytes).map_err(|e| PluginError {
            code: "invalid_registry".to_string(),
            message: format!("failed to parse plugin store registry: {e}"),
            retryable: false,
            http_status: Some(502),
        })
    }

    pub fn install_artifact_direct(
        &self,
        id: &str,
        version: &str,
        artifact_bytes: &[u8],
        expected_sha256: &str,
    ) -> Result<PathBuf, PluginError> {
        if !expected_sha256.is_empty() {
            let mut hasher = Sha256::new();
            hasher.update(artifact_bytes);
            let calculated = format!("{:x}", hasher.finalize());
            if !calculated.eq_ignore_ascii_case(expected_sha256.trim()) {
                return Err(PluginError {
                    code: "checksum_mismatch".to_string(),
                    message: format!(
                        "checksum mismatch: calculated {calculated}, expected {expected_sha256}"
                    ),
                    retryable: false,
                    http_status: Some(400),
                });
            }
        }

        if let Some(ref host) = self.host {
            if host.is_plugin_busy(id) {
                return Err(PluginError {
                    code: "plugin_busy".to_string(),
                    message: format!("plugin {id} is loaded and busy; cannot overwrite"),
                    retryable: false,
                    http_status: Some(409),
                });
            }
        }

        let (goos, _) = Self::current_platform();
        let ext = Self::plugin_extension(&goos);
        fs::create_dir_all(&self.plugins_dir).map_err(|e| PluginError {
            code: "fs_error".to_string(),
            message: format!("failed to create plugins dir: {e}"),
            retryable: false,
            http_status: Some(500),
        })?;

        let filename = if version.is_empty() {
            format!("{id}{ext}")
        } else {
            format!("{id}-v{version}{ext}")
        };

        let target_path = self.plugins_dir.join(&filename);
        let tmp_path = self
            .plugins_dir
            .join(format!(".{filename}.tmp-{}", uuid::Uuid::new_v4()));

        fs::write(&tmp_path, artifact_bytes).map_err(|e| PluginError {
            code: "fs_error".to_string(),
            message: format!("failed to write plugin temp file: {e}"),
            retryable: false,
            http_status: Some(500),
        })?;

        fs::rename(&tmp_path, &target_path).map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            PluginError {
                code: "fs_error".to_string(),
                message: format!("failed to atomic rename plugin file: {e}"),
                retryable: false,
                http_status: Some(500),
            }
        })?;

        Ok(target_path)
    }

    pub fn delete_plugin(&self, id: &str) -> Result<bool, PluginError> {
        if let Some(ref host) = self.host {
            if host.is_plugin_busy(id) {
                return Err(PluginError {
                    code: "plugin_locked".to_string(),
                    message: "loaded plugin cannot be deleted while in use".to_string(),
                    retryable: false,
                    http_status: Some(409),
                });
            }
            let _ = host.unload_plugin(id);
        }

        let mut deleted = false;
        if let Ok(entries) = fs::read_dir(&self.plugins_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(id) {
                    let _ = fs::remove_file(entry.path());
                    deleted = true;
                }
            }
        }
        Ok(deleted)
    }

    pub fn execute_home_sync(
        &self,
        node_id: &str,
        items: &[PluginSyncItem],
        fake_fetcher: impl Fn(&str) -> Result<Vec<u8>, String>,
    ) -> SyncReport {
        let (goos, goarch) = Self::current_platform();
        let mut platform_map = HashMap::new();
        platform_map.insert("goos".to_string(), goos.clone());
        platform_map.insert("goarch".to_string(), goarch.clone());

        let now = chrono::Utc::now().to_rfc3339();
        let mut report = SyncReport {
            schema_version: 1,
            task_id: 1,
            task: "plugin-sync".to_string(),
            node_id: node_id.to_string(),
            status: "success".to_string(),
            phase: "install".to_string(),
            ok: true,
            started_at: now.clone(),
            finished_at: String::new(),
            updated_at: now,
            platform: platform_map,
            plugins: Vec::new(),
            error: String::new(),
        };

        for item in items {
            let p_id = &item.manifest.id;
            let mut status = PluginInstallStatus {
                id: p_id.clone(),
                version: item.manifest.version.clone(),
                release_tag: item.manifest.release_tag.clone(),
                repository: item.manifest.repository.clone(),
                install_type: item.manifest.install.install_type.clone(),
                install_status: "installed".to_string(),
                load_status: "loaded".to_string(),
                path: String::new(),
                skipped: false,
                overwritten: false,
                error: String::new(),
            };

            let matching_art = item.manifest.install.artifacts.iter().find(|a| {
                (a.goos.is_empty() || a.goos == goos) && (a.goarch.is_empty() || a.goarch == goarch)
            });

            if let Some(art) = matching_art {
                match fake_fetcher(&art.url) {
                    Ok(bytes) => {
                        match self.install_artifact_direct(
                            p_id,
                            &item.manifest.version,
                            &bytes,
                            &art.sha256,
                        ) {
                            Ok(path) => {
                                status.path = path.to_string_lossy().to_string();
                            }
                            Err(e) => {
                                status.install_status = "failed".to_string();
                                status.error = e.message;
                                report.ok = false;
                                report.status = "failed".to_string();
                            }
                        }
                    }
                    Err(e) => {
                        status.install_status = "failed".to_string();
                        status.error = e;
                        report.ok = false;
                        report.status = "failed".to_string();
                    }
                }
            } else {
                status.install_status = "skipped".to_string();
                status.skipped = true;
            }

            report.plugins.push(status);
        }

        report.finished_at = chrono::Utc::now().to_rfc3339();
        report.updated_at = report.finished_at.clone();
        report
    }

    pub async fn fetch_registry(
        &self,
        url: &str,
        proxy_url: Option<&str>,
    ) -> Result<StoreRegistry, PluginError> {
        let client =
            crate::proxy_policy::build_http_client(proxy_url).map_err(|e| PluginError {
                code: "http_client_error".to_string(),
                message: e.to_string(),
                retryable: false,
                http_status: Some(500),
            })?;
        let resp = client.get(url).send().await.map_err(|e| PluginError {
            code: "registry_fetch_failed".to_string(),
            message: format!("failed to fetch registry from {url}: {e}"),
            retryable: false,
            http_status: Some(502),
        })?;
        if !resp.status().is_success() {
            return Err(PluginError {
                code: "registry_fetch_failed".to_string(),
                message: format!("registry fetch returned status {}", resp.status()),
                retryable: false,
                http_status: Some(502),
            });
        }
        let bytes = resp.bytes().await.map_err(|e| PluginError {
            code: "registry_fetch_failed".to_string(),
            message: format!("failed to read registry response body: {e}"),
            retryable: false,
            http_status: Some(502),
        })?;
        self.parse_registry(&bytes)
    }

    pub async fn download_artifact(
        &self,
        url: &str,
        proxy_url: Option<&str>,
    ) -> Result<Vec<u8>, PluginError> {
        let client =
            crate::proxy_policy::build_http_client(proxy_url).map_err(|e| PluginError {
                code: "http_client_error".to_string(),
                message: e.to_string(),
                retryable: false,
                http_status: Some(500),
            })?;
        let resp = client.get(url).send().await.map_err(|e| PluginError {
            code: "download_failed".to_string(),
            message: format!("failed to download artifact from {url}: {e}"),
            retryable: false,
            http_status: Some(502),
        })?;
        if !resp.status().is_success() {
            return Err(PluginError {
                code: "download_failed".to_string(),
                message: format!("download artifact status: {}", resp.status()),
                retryable: false,
                http_status: Some(502),
            });
        }
        let bytes = resp.bytes().await.map_err(|e| PluginError {
            code: "download_failed".to_string(),
            message: format!("failed to read artifact bytes: {e}"),
            retryable: false,
            http_status: Some(502),
        })?;
        Ok(bytes.to_vec())
    }

    pub async fn install_from_registry(
        &self,
        id: &str,
        requested_version: &str,
        registry_url: &str,
        proxy_url: Option<&str>,
    ) -> Result<InstalledArtifact, PluginError> {
        let registry = self.fetch_registry(registry_url, proxy_url).await?;
        let plugin = registry
            .plugins
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| PluginError {
                code: "plugin_not_found".to_string(),
                message: format!("plugin '{id}' not found in store"),
                retryable: false,
                http_status: Some(404),
            })?;

        let (goos, goarch) = Self::current_platform();
        let install_plan = plugin.install.as_ref().ok_or_else(|| PluginError {
            code: "missing_install_plan".to_string(),
            message: format!("plugin '{id}' has no install plan"),
            retryable: false,
            http_status: Some(400),
        })?;

        let artifact = install_plan
            .artifacts
            .iter()
            .find(|a| {
                (a.goos.is_empty() || a.goos == goos) && (a.goarch.is_empty() || a.goarch == goarch)
            })
            .ok_or_else(|| PluginError {
                code: "unsupported_platform".to_string(),
                message: format!("no artifact for platform {goos}/{goarch}"),
                retryable: false,
                http_status: Some(400),
            })?;

        let bytes = self.download_artifact(&artifact.url, proxy_url).await?;
        let version = if !requested_version.is_empty() {
            requested_version
        } else if !plugin.version.is_empty() {
            &plugin.version
        } else {
            ""
        };

        let path = self.install_artifact_direct(id, version, &bytes, &artifact.sha256)?;
        Ok(InstalledArtifact {
            id: id.to_string(),
            version: version.to_string(),
            source_id: DEFAULT_SOURCE_ID.to_string(),
            source_name: DEFAULT_SOURCE_NAME.to_string(),
            source_url: registry_url.to_string(),
            path,
            overwritten: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_registry_valid() {
        let store = PluginStore::new(PathBuf::from("/tmp/plugins"), None);
        let raw = br#"{
            "schema_version": 1,
            "plugins": [
                {
                    "id": "my-plugin",
                    "name": "My Plugin",
                    "version": "1.0.0",
                    "install": {
                        "type": "direct",
                        "artifacts": [
                            {
                                "goos": "darwin",
                                "goarch": "arm64",
                                "url": "https://example.com/p.dylib",
                                "sha256": "abcdef",
                                "size": 1234
                            }
                        ]
                    }
                }
            ]
        }"#;
        let reg = store.parse_registry(raw).expect("parse registry");
        assert_eq!(reg.schema_version, 1);
        assert_eq!(reg.plugins.len(), 1);
        assert_eq!(reg.plugins[0].id, "my-plugin");
        assert_eq!(reg.plugins[0].name, "My Plugin");
    }

    #[test]
    fn test_install_artifact_direct_sha256_match_and_mismatch() {
        let temp_dir = std::env::temp_dir().join(format!("store-test-{}", uuid::Uuid::new_v4()));
        let store = PluginStore::new(temp_dir.clone(), None);

        let data = b"hello binary plugin";
        let mut hasher = Sha256::new();
        hasher.update(data);
        let real_sha256 = format!("{:x}", hasher.finalize());

        // Mismatch test
        let err = store
            .install_artifact_direct(
                "test-plugin",
                "1.0.0",
                data,
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap_err();
        assert_eq!(err.code, "checksum_mismatch");
        assert_eq!(err.http_status, Some(400));

        // Match test
        let installed_path = store
            .install_artifact_direct("test-plugin", "1.0.0", data, &real_sha256)
            .expect("install direct");
        assert!(installed_path.exists());
        let read_back = fs::read(&installed_path).expect("read installed");
        assert_eq!(read_back, data);

        // Delete test
        let deleted = store.delete_plugin("test-plugin").expect("delete");
        assert!(deleted);
        assert!(!installed_path.exists());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_execute_home_sync_report() {
        let temp_dir =
            std::env::temp_dir().join(format!("store-sync-test-{}", uuid::Uuid::new_v4()));
        let store = PluginStore::new(temp_dir.clone(), None);

        let (goos, goarch) = PluginStore::current_platform();
        let data = b"home sync binary";
        let mut hasher = Sha256::new();
        hasher.update(data);
        let real_sha256 = format!("{:x}", hasher.finalize());

        let item = PluginSyncItem {
            manifest: StoreManifest {
                schema_version: 1,
                id: "sync-plugin".to_string(),
                name: "Sync Plugin".to_string(),
                description: "".to_string(),
                author: "".to_string(),
                version: "1.0.0".to_string(),
                release_tag: "".to_string(),
                repository: "".to_string(),
                logo: "".to_string(),
                homepage: "".to_string(),
                license: "".to_string(),
                tags: vec![],
                source_id: "official".to_string(),
                source_name: "Official".to_string(),
                source_url: "".to_string(),
                install: StoreInstallPlan {
                    install_type: "direct".to_string(),
                    artifacts: vec![StoreArtifact {
                        goos,
                        goarch,
                        url: "https://mock.invalid/plugin.dylib".to_string(),
                        sha256: real_sha256,
                        size: data.len() as i64,
                    }],
                },
            },
            auth: json!({}),
        };

        let report = store.execute_home_sync("node-1", &[item], |_url| Ok(data.to_vec()));
        assert!(report.ok);
        assert_eq!(report.plugins.len(), 1);
        assert_eq!(report.plugins[0].id, "sync-plugin");
        assert_eq!(report.plugins[0].install_status, "installed");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}

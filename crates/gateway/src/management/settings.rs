use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn default_catalog_refresh_enabled() -> bool {
    true
}

fn default_catalog_url() -> String {
    crate::registry::DEFAULT_REMOTE_CATALOG_URL.to_string()
}

fn default_catalog_signature_url() -> String {
    crate::registry::DEFAULT_REMOTE_SIGNATURE_URL.to_string()
}

fn default_catalog_refresh_interval_secs() -> u64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCatalogSettings {
    #[serde(
        rename = "refresh-enabled",
        default = "default_catalog_refresh_enabled"
    )]
    pub refresh_enabled: bool,
    #[serde(default = "default_catalog_url")]
    pub url: String,
    #[serde(rename = "signature-url", default = "default_catalog_signature_url")]
    pub signature_url: String,
    #[serde(
        rename = "refresh-interval-secs",
        default = "default_catalog_refresh_interval_secs"
    )]
    pub refresh_interval_secs: u64,
    #[serde(
        rename = "allowed-blackouts",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allowed_blackouts: Vec<String>,
    #[serde(
        rename = "custom-models",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub custom_models: Vec<mahoquot_registry::ModelDescriptor>,
}

impl Default for ModelCatalogSettings {
    fn default() -> Self {
        Self {
            refresh_enabled: default_catalog_refresh_enabled(),
            url: default_catalog_url(),
            signature_url: default_catalog_signature_url(),
            refresh_interval_secs: default_catalog_refresh_interval_secs(),
            allowed_blackouts: Vec::new(),
            custom_models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteManagement {
    #[serde(rename = "allow-remote", default)]
    pub allow_remote: bool,
    #[serde(rename = "secret-key", default)]
    pub secret_key: String,
    #[serde(rename = "disable-control-panel", default)]
    pub disable_control_panel: bool,
}

/// Upstream exposes these through `GET /config` even when unset, and a client
/// that reads the config expects the key to exist. They are carried verbatim so
/// the document round-trips without dropping fields this build does not act on.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PassthroughSettings {
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginsSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub dir: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingSettings {
    #[serde(default)]
    pub strategy: String,
}

/// An inbound key route stores only the key's stable one-way identifier. Raw
/// client keys remain in the canonical `api-keys` setting and are never copied
/// into routing metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiKeyBinding {
    pub key_identifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// A delegated inbound key with a restricted blast radius.
///
/// Only the one-way `key_identifier` (SHA-256 of the raw key, as produced by
/// [`crate::request_history::stable_key_identifier`]) is persisted; the raw key
/// is shown to the operator once at mint time and never stored. `key_prefix` is
/// a non-secret display fragment so a key can be recognised in a list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopedApiKey {
    pub id: String,
    pub name: String,
    pub key_identifier: String,
    pub key_prefix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_key: Option<String>,
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    #[serde(default)]
    pub allowed_accounts: Vec<String>,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub token_limit: u64,
    #[serde(default)]
    pub token_used: u64,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

impl ScopedApiKey {
    /// True when the key may still authenticate at `now_ms`: active, and either
    /// non-expiring or not yet past its expiry.
    pub fn is_usable_at(&self, now_ms: i64) -> bool {
        self.is_active && self.expires_at_ms.map(|at| now_ms < at).unwrap_or(true)
    }

    /// True when the key has consumed its allowance. A zero `token_limit` means
    /// unlimited, matching the serde default for keys minted without a cap.
    pub fn is_exhausted(&self) -> bool {
        self.token_limit > 0 && self.token_used >= self.token_limit
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaExceededSettings {
    #[serde(rename = "switch-project", default = "yes")]
    pub switch_project: bool,
    #[serde(rename = "switch-preview-model", default = "yes")]
    pub switch_preview_model: bool,
}

impl Default for QuotaExceededSettings {
    fn default() -> Self {
        Self {
            switch_project: true,
            switch_preview_model: true,
        }
    }
}

fn yes() -> bool {
    true
}

fn default_log_size_mb() -> i64 {
    100
}

fn default_port() -> u16 {
    18801
}

fn default_max_retry() -> usize {
    3
}

/// The persisted settings document, mirroring the YAML keys CLIProxyAPI uses
/// so a `config.yaml` written by either proxy is readable by the other.
///
/// Only the fields mahoquot-rs actually honours are modelled. `extra` captures
/// every other key verbatim so round-tripping a CLIProxyAPI config through
/// mahoquot never silently drops settings this build does not implement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(rename = "auth-dir", default)]
    pub auth_dir: String,
    #[serde(default)]
    pub debug: bool,
    #[serde(rename = "logging-to-file", default = "yes")]
    pub logging_to_file: bool,
    #[serde(rename = "logs-max-total-size-mb", default = "default_log_size_mb")]
    pub logs_max_total_size_mb: i64,
    #[serde(rename = "error-logs-max-files", default)]
    pub error_logs_max_files: i64,
    #[serde(rename = "usage-statistics-enabled", default)]
    pub usage_statistics_enabled: bool,
    #[serde(rename = "request-log", default)]
    pub request_log: bool,
    #[serde(rename = "proxy-url", default)]
    pub proxy_url: String,
    #[serde(rename = "request-retry", default)]
    pub request_retry: i64,
    #[serde(rename = "max-retry-credentials", default = "default_max_retry")]
    pub max_retry_credentials: usize,
    #[serde(rename = "max-retry-interval", default)]
    pub max_retry_interval: i64,
    #[serde(rename = "force-model-prefix", default)]
    pub force_model_prefix: bool,
    #[serde(rename = "ws-auth", default)]
    pub ws_auth: bool,
    #[serde(default)]
    pub routing: RoutingSettings,
    #[serde(default)]
    pub plugins: PluginsSettings,
    #[serde(default, rename = "commercial-mode")]
    pub commercial_mode: bool,
    #[serde(default, rename = "disable-cooling")]
    pub disable_cooling: bool,
    #[serde(default, rename = "disable-image-generation")]
    pub disable_image_generation: bool,
    #[serde(default, rename = "disable-claude-cloak-mode")]
    pub disable_claude_cloak_mode: bool,
    #[serde(default, rename = "save-cooldown-status")]
    pub save_cooldown_status: bool,
    #[serde(default, rename = "passthrough-headers")]
    pub passthrough_headers: bool,
    #[serde(default, rename = "auth-auto-refresh-workers")]
    pub auth_auto_refresh_workers: i64,
    #[serde(default, rename = "transient-error-cooldown-seconds")]
    pub transient_error_cooldown_seconds: i64,
    #[serde(default, rename = "redis-usage-queue-retention-seconds")]
    pub redis_usage_queue_retention_seconds: i64,
    #[serde(default, rename = "claude-code")]
    pub claude_code: PassthroughSettings,
    #[serde(default)]
    pub codex: PassthroughSettings,
    #[serde(default)]
    pub antigravity: PassthroughSettings,
    #[serde(default)]
    pub xai: PassthroughSettings,
    #[serde(default)]
    pub tls: PassthroughSettings,
    #[serde(default)]
    pub payload: PassthroughSettings,
    #[serde(default)]
    pub streaming: PassthroughSettings,
    #[serde(default)]
    pub pprof: PassthroughSettings,
    #[serde(default, rename = "claude-header-defaults")]
    pub claude_header_defaults: PassthroughSettings,
    #[serde(default, rename = "codex-header-defaults")]
    pub codex_header_defaults: PassthroughSettings,
    #[serde(default, rename = "credential-concurrency")]
    pub credential_concurrency: PassthroughSettings,
    #[serde(default, rename = "credential-in-flight")]
    pub credential_in_flight: PassthroughSettings,
    #[serde(rename = "quota-exceeded", default)]
    pub quota_exceeded: QuotaExceededSettings,
    #[serde(rename = "remote-management", default)]
    pub remote_management: RemoteManagement,
    #[serde(rename = "api-keys", default)]
    pub api_keys: Vec<String>,
    #[serde(rename = "api-key-bindings", default)]
    pub api_key_bindings: Vec<ApiKeyBinding>,
    #[serde(rename = "scoped-api-keys", default)]
    pub scoped_api_keys: Vec<ScopedApiKey>,
    #[serde(rename = "oauth-excluded-models", default)]
    pub oauth_excluded_models: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(rename = "gemini-api-key", default)]
    pub gemini_api_key: Vec<String>,
    #[serde(rename = "claude-api-key", default)]
    pub claude_api_key: Vec<String>,
    #[serde(rename = "codex-api-key", default)]
    pub codex_api_key: Vec<String>,
    #[serde(rename = "xai-api-key", default)]
    pub xai_api_key: Vec<String>,
    #[serde(rename = "vertex-api-key", default)]
    pub vertex_api_key: Vec<String>,
    #[serde(rename = "interactions-api-key", default)]
    pub interactions_api_key: Vec<String>,
    #[serde(rename = "openai-compatibility", default)]
    pub openai_compatibility: Vec<String>,
    #[serde(rename = "oauth-model-alias", default)]
    pub oauth_model_alias: serde_json::Value,
    #[serde(rename = "oauth-request-scoped-errors", default)]
    pub oauth_request_scoped_errors: serde_json::Value,
    #[serde(
        rename = "model-catalog",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub model_catalog: Option<ModelCatalogSettings>,

    #[serde(flatten)]
    pub extra: serde_yaml::Mapping,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: default_port(),
            auth_dir: String::new(),
            debug: false,
            logging_to_file: true,
            logs_max_total_size_mb: default_log_size_mb(),
            error_logs_max_files: 0,
            usage_statistics_enabled: false,
            request_log: false,
            proxy_url: String::new(),
            request_retry: 0,
            max_retry_credentials: default_max_retry(),
            max_retry_interval: 0,
            force_model_prefix: false,
            ws_auth: false,
            routing: RoutingSettings::default(),
            plugins: PluginsSettings::default(),
            commercial_mode: false,
            disable_cooling: false,
            disable_image_generation: false,
            disable_claude_cloak_mode: false,
            save_cooldown_status: false,
            passthrough_headers: false,
            auth_auto_refresh_workers: 0,
            transient_error_cooldown_seconds: 0,
            redis_usage_queue_retention_seconds: 60,
            claude_code: PassthroughSettings::default(),
            codex: PassthroughSettings::default(),
            antigravity: PassthroughSettings::default(),
            xai: PassthroughSettings::default(),
            tls: PassthroughSettings::default(),
            payload: PassthroughSettings::default(),
            streaming: PassthroughSettings::default(),
            pprof: PassthroughSettings::default(),
            claude_header_defaults: PassthroughSettings::default(),
            codex_header_defaults: PassthroughSettings::default(),
            credential_concurrency: PassthroughSettings::default(),
            credential_in_flight: PassthroughSettings::default(),
            quota_exceeded: QuotaExceededSettings::default(),
            remote_management: RemoteManagement::default(),
            api_keys: Vec::new(),
            api_key_bindings: Vec::new(),
            scoped_api_keys: Vec::new(),
            oauth_excluded_models: std::collections::BTreeMap::new(),
            gemini_api_key: Vec::new(),
            claude_api_key: Vec::new(),
            codex_api_key: Vec::new(),
            xai_api_key: Vec::new(),
            vertex_api_key: Vec::new(),
            interactions_api_key: Vec::new(),
            openai_compatibility: Vec::new(),
            oauth_model_alias: serde_json::Value::Null,
            oauth_request_scoped_errors: serde_json::Value::Null,
            model_catalog: None,
            extra: serde_yaml::Mapping::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("failed to serialise settings: {0}")]
    Serialise(#[from] serde_yaml::Error),
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("registry validation error: {0}")]
    Validation(#[from] mahoquot_registry::RegistryError),
    #[error("invalid model-catalog setting: {0}")]
    InvalidCatalogConfig(String),
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        let raw = std::fs::read_to_string(path).map_err(|source| SettingsError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_yaml(&raw).map_err(|source| SettingsError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn from_yaml(raw: &str) -> Result<Self, serde_yaml::Error> {
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_yaml::from_str(raw)
    }

    pub fn to_yaml(&self) -> Result<String, SettingsError> {
        Ok(serde_yaml::to_string(self)?)
    }

    /// Write the document so a reader never observes a partial file: render to
    /// a sibling temp file, fsync it, then rename over the target. A crash
    /// mid-write leaves either the old file or the new one, never a truncated
    /// config that would fail to parse on the next boot.
    pub fn persist(&self, path: &Path) -> Result<(), SettingsError> {
        use std::io::Write;

        let rendered = self.to_yaml()?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| SettingsError::Write {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
        }

        let temp_path = path.with_extension(format!("tmp{}", std::process::id()));
        let write = |target: &Path| -> std::io::Result<()> {
            let mut file = std::fs::File::create(target)?;
            file.write_all(rendered.as_bytes())?;
            file.sync_all()
        };
        write(&temp_path).map_err(|source| SettingsError::Write {
            path: temp_path.clone(),
            source,
        })?;

        std::fs::rename(&temp_path, path).map_err(|source| {
            let _ = std::fs::remove_file(&temp_path);
            SettingsError::Write {
                path: path.to_path_buf(),
                source,
            }
        })
    }

    /// Parse `oauth_model_alias` JSON value into typed `ModelAliasRule` list.
    pub fn parse_model_aliases(
        &self,
    ) -> Result<Vec<mahoquot_registry::ModelAliasRule>, SettingsError> {
        let mut rules = Vec::new();
        if self.oauth_model_alias.is_null() {
            return Ok(rules);
        }

        let mut root = &self.oauth_model_alias;
        if let Some(obj) = root.as_object() {
            if obj.len() == 1 && obj.contains_key("items") {
                root = &obj["items"];
            }
        }

        match root {
            serde_json::Value::Object(map) => {
                for (key, val) in map {
                    match val {
                        serde_json::Value::String(target_str) => {
                            let alias_id = mahoquot_registry::ModelId::new(key)?;
                            let target_id = mahoquot_registry::ModelId::new(target_str)?;
                            rules.push(mahoquot_registry::ModelAliasRule {
                                alias: alias_id,
                                target: target_id,
                                provider_id: None,
                            });
                        }
                        serde_json::Value::Object(inner_map) => {
                            let pid = mahoquot_registry::ProviderId::canonical(key)?;
                            for (alias_str, target_val) in inner_map {
                                let target_str = target_val.as_str().ok_or_else(|| {
                                    SettingsError::InvalidCatalogConfig(format!(
                                        "alias target for '{alias_str}' must be a string"
                                    ))
                                })?;
                                let alias_id = mahoquot_registry::ModelId::new(alias_str)?;
                                let target_id = mahoquot_registry::ModelId::new(target_str)?;
                                rules.push(mahoquot_registry::ModelAliasRule {
                                    alias: alias_id,
                                    target: target_id,
                                    provider_id: Some(pid.clone()),
                                });
                            }
                        }
                        serde_json::Value::Array(items) => {
                            let pid = mahoquot_registry::ProviderId::canonical(key)?;
                            for item in items {
                                let item_obj = item.as_object().ok_or_else(|| {
                                    SettingsError::InvalidCatalogConfig(
                                        "alias item in array must be an object".to_string(),
                                    )
                                })?;
                                let alias_str = item_obj
                                    .get("alias")
                                    .and_then(|v| v.as_str())
                                    .or_else(|| item_obj.get("from").and_then(|v| v.as_str()))
                                    .ok_or_else(|| {
                                        SettingsError::InvalidCatalogConfig(
                                            "alias item missing 'alias' field".to_string(),
                                        )
                                    })?;
                                let target_str = item_obj
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .or_else(|| item_obj.get("target").and_then(|v| v.as_str()))
                                    .or_else(|| item_obj.get("model").and_then(|v| v.as_str()))
                                    .or_else(|| item_obj.get("to").and_then(|v| v.as_str()))
                                    .ok_or_else(|| {
                                        SettingsError::InvalidCatalogConfig(
                                            "alias item missing 'name' or 'target' field"
                                                .to_string(),
                                        )
                                    })?;
                                let alias_id = mahoquot_registry::ModelId::new(alias_str)?;
                                let target_id = mahoquot_registry::ModelId::new(target_str)?;
                                rules.push(mahoquot_registry::ModelAliasRule {
                                    alias: alias_id,
                                    target: target_id,
                                    provider_id: Some(pid.clone()),
                                });
                            }
                        }
                        _ => {
                            return Err(SettingsError::InvalidCatalogConfig(format!(
                                "unsupported alias value format for key '{key}'"
                            )));
                        }
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    let item_obj = item.as_object().ok_or_else(|| {
                        SettingsError::InvalidCatalogConfig(
                            "top-level alias array item must be an object".to_string(),
                        )
                    })?;
                    let alias_str = item_obj
                        .get("alias")
                        .and_then(|v| v.as_str())
                        .or_else(|| item_obj.get("from").and_then(|v| v.as_str()))
                        .ok_or_else(|| {
                            SettingsError::InvalidCatalogConfig(
                                "alias item missing 'alias' field".to_string(),
                            )
                        })?;
                    let target_str = item_obj
                        .get("name")
                        .and_then(|v| v.as_str())
                        .or_else(|| item_obj.get("target").and_then(|v| v.as_str()))
                        .or_else(|| item_obj.get("model").and_then(|v| v.as_str()))
                        .or_else(|| item_obj.get("to").and_then(|v| v.as_str()))
                        .ok_or_else(|| {
                            SettingsError::InvalidCatalogConfig(
                                "alias item missing 'name' or 'target' field".to_string(),
                            )
                        })?;
                    let pid = match item_obj
                        .get("provider")
                        .or_else(|| item_obj.get("provider_id"))
                        .and_then(|v| v.as_str())
                    {
                        Some(p) if !p.trim().is_empty() => {
                            Some(mahoquot_registry::ProviderId::canonical(p)?)
                        }
                        _ => None,
                    };
                    let alias_id = mahoquot_registry::ModelId::new(alias_str)?;
                    let target_id = mahoquot_registry::ModelId::new(target_str)?;
                    rules.push(mahoquot_registry::ModelAliasRule {
                        alias: alias_id,
                        target: target_id,
                        provider_id: pid,
                    });
                }
            }
            _ => {
                return Err(SettingsError::InvalidCatalogConfig(
                    "oauth-model-alias must be an object or array".to_string(),
                ));
            }
        }

        Ok(rules)
    }

    /// Parse `oauth_excluded_models` into typed `ModelExclusionRule` list.
    pub fn parse_model_exclusions(
        &self,
    ) -> Result<Vec<mahoquot_registry::ModelExclusionRule>, SettingsError> {
        let mut rules = Vec::new();
        for (provider_str, models) in &self.oauth_excluded_models {
            let pid = if provider_str == "*"
                || provider_str.is_empty()
                || provider_str.eq_ignore_ascii_case("all")
            {
                None
            } else {
                Some(mahoquot_registry::ProviderId::canonical(provider_str)?)
            };

            for model_str in models {
                let model_id = mahoquot_registry::ModelId::new(model_str)?;
                rules.push(mahoquot_registry::ModelExclusionRule {
                    model_id,
                    provider_id: pid.clone(),
                });
            }
        }
        Ok(rules)
    }

    /// Validate this settings document atomically against an active registry snapshot,
    /// returning the composed candidate snapshot on success.
    pub fn validate_against_registry(
        &self,
        snapshot: &mahoquot_registry::RegistrySnapshot,
    ) -> Result<mahoquot_registry::RegistrySnapshot, SettingsError> {
        let aliases = self.parse_model_aliases()?;
        let exclusions = self.parse_model_exclusions()?;

        let mut allowed_blackouts = Vec::new();
        let mut custom_models = Vec::new();

        if let Some(ref cat) = self.model_catalog {
            validate_catalog_url(&cat.url, "catalog url")?;
            validate_catalog_url(&cat.signature_url, "signature url")?;

            for pid_str in &cat.allowed_blackouts {
                allowed_blackouts.push(mahoquot_registry::ProviderId::canonical(pid_str)?);
            }
            custom_models = cat.custom_models.clone();
        }

        let candidate = snapshot.compose_with_settings(
            aliases,
            exclusions.into_iter().collect(),
            custom_models,
            &allowed_blackouts,
        )?;

        Ok(candidate)
    }
}

fn validate_catalog_url(url_str: &str, field_name: &str) -> Result<(), SettingsError> {
    if url_str.trim().is_empty() {
        return Ok(());
    }
    let parsed = reqwest::Url::parse(url_str).map_err(|e| {
        SettingsError::InvalidCatalogConfig(format!("invalid {field_name} '{url_str}': {e}"))
    })?;

    let scheme = parsed.scheme();
    let host = parsed.host_str().unwrap_or("");
    if host.is_empty() {
        return Err(SettingsError::InvalidCatalogConfig(format!(
            "{field_name} must contain a non-empty host: '{url_str}'"
        )));
    }

    if scheme != "https" {
        let is_localhost = host == "127.0.0.1" || host == "localhost";
        if scheme == "http" && is_localhost {
            // allowed for local test/dev
        } else {
            return Err(SettingsError::InvalidCatalogConfig(format!(
                "insecure {field_name} scheme '{scheme}': must be https (http only permitted for 127.0.0.1/localhost)"
            )));
        }
    }

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(SettingsError::InvalidCatalogConfig(format!(
            "{field_name} must not contain embedded user/password credentials"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_upstream_keys_survive_a_round_trip() {
        // given a config carrying keys this build does not model
        let raw =
            "port: 9999\nauth-dir: /tmp/a\nclaude-api-key:\n  - sk-test\ntls:\n  enable: true\n";
        // when it is parsed and re-rendered
        let settings = Settings::from_yaml(raw).expect("parses");
        let rendered = settings.to_yaml().expect("renders");
        // then the unmodelled keys are still present
        assert!(rendered.contains("claude-api-key"), "{rendered}");
        assert!(rendered.contains("tls"), "{rendered}");
        // and the modelled ones round-trip by value
        assert_eq!(settings.port, 9999);
        assert_eq!(settings.auth_dir, "/tmp/a");
    }

    #[test]
    fn upstream_yaml_key_names_are_kebab_case() {
        // given a config written with upstream's key spelling
        let raw = concat!(
            "logging-to-file: true\n",
            "logs-max-total-size-mb: 42\n",
            "request-retry: 7\n",
            "max-retry-credentials: 5\n",
            "force-model-prefix: true\n",
            "remote-management:\n  allow-remote: true\n  secret-key: shh\n",
        );
        // when parsed
        let settings = Settings::from_yaml(raw).expect("parses");
        // then every field binds
        assert!(settings.logging_to_file);
        assert_eq!(settings.logs_max_total_size_mb, 42);
        assert_eq!(settings.request_retry, 7);
        assert_eq!(settings.max_retry_credentials, 5);
        assert!(settings.force_model_prefix);
        assert!(settings.remote_management.allow_remote);
        assert_eq!(settings.remote_management.secret_key, "shh");
    }

    #[test]
    fn an_empty_document_yields_defaults() {
        // given an empty config file
        let settings = Settings::from_yaml("   \n").expect("parses");
        // then defaults apply rather than an error
        assert_eq!(settings.port, default_port());
        assert_eq!(settings.max_retry_credentials, default_max_retry());
        assert!(settings.quota_exceeded.switch_project);
    }

    #[test]
    fn persist_then_load_round_trips() {
        // given a settings document persisted to a temp dir
        let dir = std::env::temp_dir().join(format!("mahoquot-settings-{}", std::process::id()));
        let path = dir.join("config.yaml");
        let settings = Settings {
            port: 18899,
            remote_management: RemoteManagement {
                secret_key: "abc".to_string(),
                ..RemoteManagement::default()
            },
            ..Settings::default()
        };
        settings.persist(&path).expect("persists");
        // when reloaded from disk
        let loaded = Settings::load(&path).expect("loads");
        // then it matches what was written
        assert_eq!(loaded.port, 18899);
        assert_eq!(loaded.remote_management.secret_key, "abc");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn persist_leaves_no_temp_file_behind() {
        // given a persisted config
        let dir =
            std::env::temp_dir().join(format!("mahoquot-settings-tmp-{}", std::process::id()));
        let path = dir.join("config.yaml");
        Settings::default().persist(&path).expect("persists");
        // when the directory is listed
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("readable")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "config.yaml")
            .collect();
        // then only the final file remains
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
